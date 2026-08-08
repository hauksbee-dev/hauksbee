//! Aperture-macro (AM) instantiation.
//!
//! A macro is a parameterised stack of primitives (circles, lines, rectangles,
//! outlines, polygons) combined by exposure. Hauksbee only needs each flash's
//! *solid footprint* to decide which pad a placed component sits on, not a
//! pixel-perfect render, so we evaluate every exposure-on primitive to a point
//! set and return the convex hull of their union, translated to the flash
//! location. That over-approximates concave macros slightly (a cross becomes
//! its bounding diamond) but never loses a pad, and the common KiCad macros
//! (RoundRect, the chamfered/rounded pads) are convex anyway.
//!
//! **Exposure-off primitives are subtracted**, per the spec's paint model: a
//! primitive with exposure 0 erases whatever was painted under it. A clear
//! primitive whose area sits wholly inside the solid hull, and which no LATER
//! exposure-on primitive paints back over, becomes a hole contour in the
//! returned shape (even-odd containment reads its interior as empty). Ignoring
//! these read a macro's punched-out void as solid copper, so foreign copper
//! routed through the void was unioned onto the pad's net: a false short. A
//! clear that a later dark repaints, or one that crosses the hull boundary, is
//! dropped (the area stays solid), which errs toward the old over-approximation
//! and never toward inventing emptiness where copper is.
//!
//! Variable substitution handles `$1..$n` from the flash's positional
//! arguments and a small arithmetic evaluator covers the `$1+$1`,
//! `$2-$3`, `0.5*$4` forms KiCad emits. Anything we cannot evaluate
//! (nested macro variable definitions, transcendental expressions) is dropped
//! from that primitive; if nothing survives, the caller falls back to a disc.

use std::collections::HashMap;

use gerber_types::{ApertureMacro, MacroBoolean, MacroContent, MacroDecimal, MacroInteger};

use super::geo::point_in_polygon;

/// The instantiated solid area of a macro flash: the convex hull of its
/// exposure-on primitives, minus the voids its exposure-off primitives punch
/// out of it. `hull` empty means the macro could not be evaluated (the caller
/// falls back to a disc); `holes` empty means a plain convex polygon.
#[derive(Debug, Clone, Default)]
pub struct MacroShape {
    pub hull: Vec<(f64, f64)>,
    pub holes: Vec<Vec<(f64, f64)>>,
}

/// One evaluated primitive, in macro-local coordinates: its closed outline and
/// whether it paints (exposure on) or erases (exposure off).
struct EvaledPrim {
    on: bool,
    outline: Vec<(f64, f64)>,
}

/// Evaluate a macro into the polygon of its solid area (convex hull of the
/// exposure-on primitives, minus fully-interior exposure-off voids), centred at
/// the flash point `(cx, cy)` and scaled by `s` (inch->mm or 1.0). `args` are
/// the flash's positional parameters (`$1` = `args[0]`, ...).
pub fn instantiate_macro(
    m: &ApertureMacro,
    args: &[MacroDecimal],
    cx: f64,
    cy: f64,
    s: f64,
) -> MacroShape {
    let mut vars: HashMap<u32, f64> = HashMap::new();
    for (i, a) in args.iter().enumerate() {
        if let Some(v) = decimal(a, &vars) {
            vars.insert((i + 1) as u32, v);
        }
    }

    // Evaluate every primitive to its outline, in paint order, keeping the
    // exposure flag: the darks build the hull, the clears carve holes, and
    // ORDER decides whether a clear survives (a later dark repaints it).
    let mut evaled: Vec<EvaledPrim> = Vec::new();
    for content in &m.content {
        if let MacroContent::VariableDefinition(def) = content {
            if let Some(v) = eval_expr(&def.expression, &vars) {
                vars.insert(def.number, v);
            }
            continue;
        }
        let (on, outline) = match content {
            MacroContent::Circle(c) => {
                let (Some(d), Some(x), Some(y)) = (
                    decimal(&c.diameter, &vars),
                    decimal(&c.center.0, &vars),
                    decimal(&c.center.1, &vars),
                ) else {
                    continue;
                };
                let r = d / 2.0;
                // The circle primitive carries an optional rotation about the
                // macro origin (0,0). For an off-origin circle that rotation
                // MOVES the center, so it must be applied, dropping it placed a
                // rotated arm/eye at the wrong location. (An origin-centered
                // circle is unaffected, as expected.)
                let (rsin, rcos) = c
                    .angle
                    .as_ref()
                    .and_then(|a| decimal(a, &vars))
                    .unwrap_or(0.0)
                    .to_radians()
                    .sin_cos();
                let (cxr, cyr) = (x * rcos - y * rsin, x * rsin + y * rcos);
                // Sample the circle as an octagon (the hull absorbs the rest;
                // for a hole the inscribed octagon under-cuts the void
                // slightly, erring toward solid, never toward emptiness).
                let outline: Vec<(f64, f64)> = (0..8)
                    .map(|k| {
                        let a = k as f64 * std::f64::consts::TAU / 8.0;
                        (cxr + r * a.cos(), cyr + r * a.sin())
                    })
                    .collect();
                (exposed(&c.exposure, &vars), outline)
            }
            MacroContent::VectorLine(l) => {
                let (Some(w), Some(x0), Some(y0), Some(x1), Some(y1)) = (
                    decimal(&l.width, &vars),
                    decimal(&l.start.0, &vars),
                    decimal(&l.start.1, &vars),
                    decimal(&l.end.0, &vars),
                    decimal(&l.end.1, &vars),
                ) else {
                    continue;
                };
                // The vector-line primitive rotates about the macro origin
                // (0,0); rotate both endpoints before laying down the thick
                // segment, or a rotated bar/cross arm is reconstructed at the
                // wrong angle and location (same defect the CenterLine fix
                // addressed for its sibling).
                let (sin, cos) = decimal(&l.angle, &vars)
                    .unwrap_or(0.0)
                    .to_radians()
                    .sin_cos();
                let (rx0, ry0) = (x0 * cos - y0 * sin, x0 * sin + y0 * cos);
                let (rx1, ry1) = (x1 * cos - y1 * sin, x1 * sin + y1 * cos);
                let mut outline = Vec::with_capacity(4);
                push_thick_segment(&mut outline, rx0, ry0, rx1, ry1, w / 2.0);
                (exposed(&l.exposure, &vars), outline)
            }
            MacroContent::CenterLine(l) => {
                let (Some(w), Some(h), Some(x), Some(y)) = (
                    decimal(&l.dimensions.0, &vars),
                    decimal(&l.dimensions.1, &vars),
                    decimal(&l.center.0, &vars),
                    decimal(&l.center.1, &vars),
                ) else {
                    continue;
                };
                let (hw, hh) = (w / 2.0, h / 2.0);
                // The CenterLine primitive carries a rotation angle that rotates
                // it about the MACRO ORIGIN (0,0), not about its own center; the
                // same rule the Circle/VectorLine/Outline siblings follow. So the
                // center itself must be carried through the rotation: form the
                // absolute corner (x+dx, y+dy) and rotate the whole point about
                // the origin. Rotating only the corner offsets about the (kept)
                // center reconstructed an off-origin rotated pad at the wrong
                // location; the two agree only when the center is at the origin
                // or the angle is zero.
                let (sin, cos) = decimal(&l.angle, &vars)
                    .unwrap_or(0.0)
                    .to_radians()
                    .sin_cos();
                let outline: Vec<(f64, f64)> = [(-hw, -hh), (hw, -hh), (hw, hh), (-hw, hh)]
                    .into_iter()
                    .map(|(dx, dy)| {
                        let (ax, ay) = (x + dx, y + dy);
                        (ax * cos - ay * sin, ax * sin + ay * cos)
                    })
                    .collect();
                (exposed(&l.exposure, &vars), outline)
            }
            MacroContent::Outline(o) => {
                // The outline primitive rotates about the macro origin (0,0);
                // its points are absolute macro coordinates, so the rotation both
                // reorients AND (for an off-origin outline) translates the shape.
                // Dropping it; the sibling of the CenterLine bug, reconstructed
                // a rotated custom pad at the wrong orientation and location.
                let (sin, cos) = decimal(&o.angle, &vars)
                    .unwrap_or(0.0)
                    .to_radians()
                    .sin_cos();
                let outline: Vec<(f64, f64)> = o
                    .points
                    .iter()
                    .filter_map(|(px, py)| {
                        let (x, y) = (decimal(px, &vars)?, decimal(py, &vars)?);
                        Some((x * cos - y * sin, x * sin + y * cos))
                    })
                    .collect();
                (exposed(&o.exposure, &vars), outline)
            }
            MacroContent::Polygon(p) => {
                let (Some(nv), Some(x), Some(y), Some(d)) = (
                    integer(&p.vertices, &vars),
                    decimal(&p.center.0, &vars),
                    decimal(&p.center.1, &vars),
                    decimal(&p.diameter, &vars),
                ) else {
                    continue;
                };
                let r = d / 2.0;
                let rot = decimal(&p.angle, &vars).unwrap_or(0.0).to_radians();
                // The Gerber spec bounds a polygon primitive to 3..=12 vertices.
                // Clamp rather than trust the file: an out-of-spec count (e.g.
                // `%AMFOO*5,1,4294967295,...*%` from a hostile/corrupt gerber)
                // would otherwise drive a multi-gigabyte vertex push (OOM/hang).
                let n = nv.clamp(3, 12) as usize;
                // An AM primitive's rotation is about the MACRO ORIGIN (0,0), not
                // the primitive's own center, so an off-origin polygon center must
                // itself be rotated, exactly as the Circle/CenterLine/Outline/
                // VectorLine handlers do. Rotating only the vertex angle left the
                // centroid at its unrotated (x,y), placing an off-origin rotated
                // polygon pad ~|center| mm off in the wrong direction.
                let (cx0, cy0) = (x * rot.cos() - y * rot.sin(), x * rot.sin() + y * rot.cos());
                let outline: Vec<(f64, f64)> = (0..n)
                    .map(|k| {
                        let a = rot + k as f64 * std::f64::consts::TAU / n as f64;
                        (cx0 + r * a.cos(), cy0 + r * a.sin())
                    })
                    .collect();
                (exposed(&p.exposure, &vars), outline)
            }
            // Moire/Thermal are fiducial/relief shapes, not pads we bind to.
            _ => continue,
        };
        if outline.len() >= 3 {
            evaled.push(EvaledPrim { on, outline });
        }
    }

    let pts: Vec<(f64, f64)> = evaled
        .iter()
        .filter(|e| e.on)
        .flat_map(|e| e.outline.iter().copied())
        .collect();
    if pts.len() < 3 {
        return MacroShape::default();
    }
    let hull = convex_hull(pts);

    // Exposure-off subtraction, per the paint model: a clear primitive erases
    // what is under it, so its area is a VOID unless a later dark repaints it.
    // A hole is kept only when it is fully inside the hull (even-odd
    // containment cannot represent a void poking past the outer boundary) and
    // no later exposure-on primitive overlaps it. Anything else stays solid:
    // the old, safe over-approximation.
    let mut holes: Vec<Vec<(f64, f64)>> = Vec::new();
    for (i, e) in evaled.iter().enumerate() {
        if e.on {
            continue;
        }
        let fully_inside = e
            .outline
            .iter()
            .all(|&(x, y)| point_in_polygon(x, y, &hull));
        if !fully_inside {
            continue;
        }
        let repainted = evaled[i + 1..]
            .iter()
            .filter(|later| later.on)
            .any(|later| outlines_overlap(&e.outline, &later.outline));
        if !repainted {
            holes.push(e.outline.clone());
        }
    }

    let place = |pts: Vec<(f64, f64)>| -> Vec<(f64, f64)> {
        pts.into_iter()
            .map(|(x, y)| (cx + x * s, cy + y * s))
            .collect()
    };
    MacroShape {
        hull: place(hull),
        holes: holes.into_iter().map(place).collect(),
    }
}

/// Whether two closed convex-ish outlines overlap: any vertex of one inside
/// the other, or any pair of edges crossing. Used only to decide whether a
/// later exposure-on primitive repaints (part of) a clear one, in which case
/// the clear is conservatively kept solid.
fn outlines_overlap(a: &[(f64, f64)], b: &[(f64, f64)]) -> bool {
    if a.iter().any(|&(x, y)| point_in_polygon(x, y, b))
        || b.iter().any(|&(x, y)| point_in_polygon(x, y, a))
    {
        return true;
    }
    let na = a.len();
    let nb = b.len();
    for ia in 0..na {
        let (a1, a2) = (a[ia], a[(ia + 1) % na]);
        for ib in 0..nb {
            if super::geo::segments_intersect(a1, a2, b[ib], b[(ib + 1) % nb]) {
                return true;
            }
        }
    }
    false
}

fn exposed(e: &MacroBoolean, vars: &HashMap<u32, f64>) -> bool {
    match e {
        MacroBoolean::Value(b) => *b,
        MacroBoolean::Variable(n) => vars.get(n).map(|v| *v != 0.0).unwrap_or(true),
        MacroBoolean::Expression(s) => eval_expr(s, vars).map(|v| v != 0.0).unwrap_or(true),
    }
}

fn decimal(d: &MacroDecimal, vars: &HashMap<u32, f64>) -> Option<f64> {
    match d {
        MacroDecimal::Value(v) => Some(*v),
        MacroDecimal::Variable(n) => vars.get(n).copied(),
        MacroDecimal::Expression(s) => eval_expr(s, vars),
    }
}

fn integer(i: &MacroInteger, vars: &HashMap<u32, f64>) -> Option<u32> {
    match i {
        MacroInteger::Value(v) => Some(*v),
        MacroInteger::Variable(n) => vars.get(n).map(|v| *v as u32),
        MacroInteger::Expression(s) => eval_expr(s, vars).map(|v| v as u32),
    }
}

/// Tiny arithmetic evaluator for macro expressions: `$n`, decimals, and the
/// operators `+ - x/X * /` with `()`. Gerber uses `x`/`X` for multiply and `/`
/// for divide. Left-to-right with `* /` binding tighter than `+ -`.
fn eval_expr(expr: &str, vars: &HashMap<u32, f64>) -> Option<f64> {
    let tokens = tokenize(expr, vars)?;
    let mut p = Parser { t: &tokens, i: 0 };
    let v = p.expr()?;
    if p.i == p.t.len() {
        Some(v)
    } else {
        None
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(f64),
    Plus,
    Minus,
    Mul,
    Div,
    LParen,
    RParen,
}

fn tokenize(expr: &str, vars: &HashMap<u32, f64>) -> Option<Vec<Tok>> {
    let mut out = Vec::new();
    let chars: Vec<char> = expr.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            ' ' | '\t' => i += 1,
            '+' => {
                out.push(Tok::Plus);
                i += 1;
            }
            '-' => {
                out.push(Tok::Minus);
                i += 1;
            }
            'x' | 'X' | '*' => {
                out.push(Tok::Mul);
                i += 1;
            }
            '/' => {
                out.push(Tok::Div);
                i += 1;
            }
            '(' => {
                out.push(Tok::LParen);
                i += 1;
            }
            ')' => {
                out.push(Tok::RParen);
                i += 1;
            }
            '$' => {
                i += 1;
                let mut num = String::new();
                while i < chars.len() && chars[i].is_ascii_digit() {
                    num.push(chars[i]);
                    i += 1;
                }
                let n: u32 = num.parse().ok()?;
                out.push(Tok::Num(*vars.get(&n)?));
            }
            d if d.is_ascii_digit() || d == '.' => {
                let mut num = String::new();
                while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                    num.push(chars[i]);
                    i += 1;
                }
                out.push(Tok::Num(num.parse().ok()?));
            }
            _ => return None,
        }
    }
    Some(out)
}

struct Parser<'a> {
    t: &'a [Tok],
    i: usize,
}

impl Parser<'_> {
    fn expr(&mut self) -> Option<f64> {
        let mut v = self.term()?;
        while let Some(op) = self.t.get(self.i) {
            match op {
                Tok::Plus => {
                    self.i += 1;
                    v += self.term()?;
                }
                Tok::Minus => {
                    self.i += 1;
                    v -= self.term()?;
                }
                _ => break,
            }
        }
        Some(v)
    }

    fn term(&mut self) -> Option<f64> {
        let mut v = self.factor()?;
        while let Some(op) = self.t.get(self.i) {
            match op {
                Tok::Mul => {
                    self.i += 1;
                    v *= self.factor()?;
                }
                Tok::Div => {
                    self.i += 1;
                    let d = self.factor()?;
                    if d == 0.0 {
                        // A zero divisor (e.g. `$1/0`) would make Inf/NaN and
                        // silently poison the diameter/coordinate math downstream
                        // (convex_hull produces a degenerate pad). Refuse instead:
                        // returning None routes to the module's documented
                        // "can't evaluate the expression -> disc fallback".
                        return None;
                    }
                    v /= d;
                }
                _ => break,
            }
        }
        Some(v)
    }

    fn factor(&mut self) -> Option<f64> {
        match self.t.get(self.i)? {
            Tok::Num(n) => {
                self.i += 1;
                Some(*n)
            }
            Tok::Minus => {
                self.i += 1;
                Some(-self.factor()?)
            }
            Tok::Plus => {
                self.i += 1;
                self.factor()
            }
            Tok::LParen => {
                self.i += 1;
                let v = self.expr()?;
                if self.t.get(self.i) == Some(&Tok::RParen) {
                    self.i += 1;
                    Some(v)
                } else {
                    None
                }
            }
            _ => None,
        }
    }
}

fn push_thick_segment(pts: &mut Vec<(f64, f64)>, x0: f64, y0: f64, x1: f64, y1: f64, r: f64) {
    let dx = x1 - x0;
    let dy = y1 - y0;
    let len = (dx * dx + dy * dy).sqrt().max(1e-9);
    let (nx, ny) = (-dy / len * r, dx / len * r);
    pts.extend([
        (x0 + nx, y0 + ny),
        (x1 + nx, y1 + ny),
        (x1 - nx, y1 - ny),
        (x0 - nx, y0 - ny),
    ]);
}

/// Andrew's monotone chain convex hull.
fn convex_hull(mut pts: Vec<(f64, f64)>) -> Vec<(f64, f64)> {
    pts.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    pts.dedup();
    let n = pts.len();
    if n < 3 {
        return pts;
    }
    let cross = |o: (f64, f64), a: (f64, f64), b: (f64, f64)| {
        (a.0 - o.0) * (b.1 - o.1) - (a.1 - o.1) * (b.0 - o.0)
    };
    let mut hull = Vec::with_capacity(2 * n);
    for &p in &pts {
        while hull.len() >= 2 && cross(hull[hull.len() - 2], hull[hull.len() - 1], p) <= 0.0 {
            hull.pop();
        }
        hull.push(p);
    }
    let lower = hull.len() + 1;
    for &p in pts.iter().rev() {
        while hull.len() >= lower && cross(hull[hull.len() - 2], hull[hull.len() - 1], p) <= 0.0 {
            hull.pop();
        }
        hull.push(p);
    }
    hull.pop();
    hull
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arithmetic() {
        let mut vars = HashMap::new();
        vars.insert(1u32, 0.25);
        assert_eq!(eval_expr("$1+$1", &vars), Some(0.5));
        assert_eq!(eval_expr("2x$1", &vars), Some(0.5));
        assert_eq!(eval_expr("1.0-0.5", &vars), Some(0.5));
        assert_eq!(eval_expr("(1+1)x2", &vars), Some(4.0));
        assert_eq!(eval_expr("$9", &vars), None); // undefined var
    }

    #[test]
    fn division_by_zero_refuses() {
        // Bug-hunt #6: a zero divisor must yield None (routing to the disc
        // fallback), not the Inf/NaN that silently poisoned the pad geometry.
        let mut vars = HashMap::new();
        vars.insert(1u32, 5.0);
        assert_eq!(eval_expr("$1/0", &vars), None);
        assert_eq!(eval_expr("1.0/0.0", &vars), None);
        assert_eq!(eval_expr("($1-5.0)/0", &vars), None);
        // Ordinary division is unaffected.
        assert_eq!(eval_expr("1.0/2.0", &vars), Some(0.5));
    }

    fn donut(extra_dark_after: bool, hole_at: (f64, f64)) -> ApertureMacro {
        use gerber_types::{CenterLinePrimitive, CirclePrimitive};
        let mut content = vec![
            MacroContent::CenterLine(CenterLinePrimitive {
                exposure: MacroBoolean::Value(true),
                dimensions: (MacroDecimal::Value(4.0), MacroDecimal::Value(4.0)),
                center: (MacroDecimal::Value(0.0), MacroDecimal::Value(0.0)),
                angle: MacroDecimal::Value(0.0),
            }),
            MacroContent::Circle(CirclePrimitive {
                exposure: MacroBoolean::Value(false),
                diameter: MacroDecimal::Value(2.0),
                center: (MacroDecimal::Value(hole_at.0), MacroDecimal::Value(hole_at.1)),
                angle: None,
            }),
        ];
        if extra_dark_after {
            content.push(MacroContent::CenterLine(CenterLinePrimitive {
                exposure: MacroBoolean::Value(true),
                dimensions: (MacroDecimal::Value(1.0), MacroDecimal::Value(1.0)),
                center: (MacroDecimal::Value(0.0), MacroDecimal::Value(0.0)),
                angle: MacroDecimal::Value(0.0),
            }));
        }
        ApertureMacro {
            name: "DONUT".to_string(),
            content,
        }
    }

    #[test]
    fn exposure_off_primitive_punches_a_void() {
        use crate::gerber::geo::point_in_contours;
        // A 4x4 dark square with a 2 mm clear circle at its centre. The old
        // handler `continue`d over the clear and hulled the rest, so the void
        // read as solid copper and anything routed through it false-shorted.
        let ms = instantiate_macro(&donut(false, (0.0, 0.0)), &[], 0.0, 0.0, 1.0);
        assert_eq!(ms.holes.len(), 1, "the clear circle is a hole");
        let mut contours = vec![ms.hull.clone()];
        contours.extend(ms.holes.clone());
        assert!(
            !point_in_contours(0.0, 0.0, &contours),
            "the void centre is NOT copper"
        );
        assert!(
            point_in_contours(1.7, 0.0, &contours),
            "the ring around the void IS copper"
        );
        assert!(
            !point_in_contours(2.5, 0.0, &contours),
            "outside the pad is not copper"
        );
    }

    #[test]
    fn a_later_dark_repaints_the_void_solid() {
        // Paint order matters: dark, clear, then dark AGAIN over the void. The
        // final image is solid there, so no hole may be carved. (This errs the
        // safe way even for partial repaints: the whole clear stays solid.)
        let ms = instantiate_macro(&donut(true, (0.0, 0.0)), &[], 0.0, 0.0, 1.0);
        assert!(
            ms.holes.is_empty(),
            "a repainted clear must not survive as a hole"
        );
    }

    #[test]
    fn a_clear_crossing_the_hull_boundary_stays_solid() {
        // A clear circle centred ON the square's edge pokes outside the hull.
        // Even-odd contours cannot represent that void without inventing
        // copper outside the boundary, so it is dropped: the old (solid)
        // over-approximation, never phantom emptiness or phantom copper.
        let ms = instantiate_macro(&donut(false, (2.0, 0.0)), &[], 0.0, 0.0, 1.0);
        assert!(ms.holes.is_empty(), "a boundary-crossing clear is dropped");
    }

    #[test]
    fn hull_of_square_points() {
        let pts = vec![(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0), (0.5, 0.5)];
        let hull = convex_hull(pts);
        assert_eq!(hull.len(), 4); // interior point dropped
    }

    #[test]
    fn polygon_vertex_count_is_clamped() {
        use gerber_types::{MacroBoolean, PolygonPrimitive};
        // An out-of-spec vertex count (here u32::MAX, as a hostile/corrupt
        // gerber could declare) must be clamped to the Gerber-legal 3..=12, not
        // trusted, otherwise it drives a multi-gigabyte vertex push (OOM/hang).
        let poly = PolygonPrimitive {
            exposure: MacroBoolean::Value(true),
            vertices: MacroInteger::Value(u32::MAX),
            center: (MacroDecimal::Value(0.0), MacroDecimal::Value(0.0)),
            diameter: MacroDecimal::Value(1.0),
            angle: MacroDecimal::Value(0.0),
        };
        let m = ApertureMacro {
            name: "FOO".to_string(),
            content: vec![MacroContent::Polygon(poly)],
        };
        let pts = instantiate_macro(&m, &[], 0.0, 0.0, 1.0).hull;
        assert!(
            pts.len() <= 12,
            "vertex count must be clamped, got {}",
            pts.len()
        );
    }

    #[test]
    fn polygon_off_origin_center_rotates_about_the_macro_origin() {
        use gerber_types::{MacroBoolean, PolygonPrimitive};
        // R41: a hexagon centred at (3,0) rotated 45°. Per RS-274X an AM primitive
        // rotates about the MACRO ORIGIN, so the centre moves to (3cos45, 3sin45)
        // ~= (2.12, 2.12), like the Circle/CenterLine/Outline siblings. The old
        // handler rotated only the vertex angle, leaving the centroid at (3,0),
        // ~2.1 mm off in the wrong direction (a mis-bound pad/net).
        let poly = PolygonPrimitive {
            exposure: MacroBoolean::Value(true),
            vertices: MacroInteger::Value(6),
            center: (MacroDecimal::Value(3.0), MacroDecimal::Value(0.0)),
            diameter: MacroDecimal::Value(6.0),
            angle: MacroDecimal::Value(45.0),
        };
        let m = ApertureMacro {
            name: "HEX".to_string(),
            content: vec![MacroContent::Polygon(poly)],
        };
        let pts = instantiate_macro(&m, &[], 0.0, 0.0, 1.0).hull;
        let n = pts.len() as f64;
        let cx: f64 = pts.iter().map(|p| p.0).sum::<f64>() / n;
        let cy: f64 = pts.iter().map(|p| p.1).sum::<f64>() / n;
        let want = 3.0 * std::f64::consts::FRAC_1_SQRT_2; // 3·cos45 = 3·sin45 ≈ 2.121
        assert!(
            (cx - want).abs() < 0.05 && (cy - want).abs() < 0.05,
            "polygon centre must rotate about the macro origin to ({want:.3}, {want:.3}), \
             got ({cx:.3}, {cy:.3})"
        );
    }

    #[test]
    fn center_line_applies_rotation() {
        use gerber_types::{CenterLinePrimitive, MacroBoolean};
        // A 4×2 rectangle centred at the origin, rotated 90°: width and height
        // swap, so the instantiated hull spans ~2 in x and ~4 in y. The old
        // handler ignored the angle and emitted an axis-aligned 4×2.
        let cl = CenterLinePrimitive {
            exposure: MacroBoolean::Value(true),
            dimensions: (MacroDecimal::Value(4.0), MacroDecimal::Value(2.0)),
            center: (MacroDecimal::Value(0.0), MacroDecimal::Value(0.0)),
            angle: MacroDecimal::Value(90.0),
        };
        let m = ApertureMacro {
            name: "CL".to_string(),
            content: vec![MacroContent::CenterLine(cl)],
        };
        let pts = instantiate_macro(&m, &[], 0.0, 0.0, 1.0).hull;
        let (minx, maxx) = pts
            .iter()
            .fold((f64::MAX, f64::MIN), |(a, b), p| (a.min(p.0), b.max(p.0)));
        let (miny, maxy) = pts
            .iter()
            .fold((f64::MAX, f64::MIN), |(a, b), p| (a.min(p.1), b.max(p.1)));
        assert!(
            (maxx - minx - 2.0).abs() < 1e-6,
            "x span {} (expected ~2 after 90° rotation)",
            maxx - minx
        );
        assert!(
            (maxy - miny - 4.0).abs() < 1e-6,
            "y span {} (expected ~4 after 90° rotation)",
            maxy - miny
        );
    }

    #[test]
    fn center_line_rotates_about_the_macro_origin_not_its_own_center() {
        use gerber_types::{CenterLinePrimitive, MacroBoolean};
        // A 2×2 rectangle centred OFF the origin at (3,0), rotated 90° about the
        // macro origin. The center itself must rotate: (3,0)→(0,3), so the hull
        // spans x∈[-1,1], y∈[2,4]. Rotating only the corner offsets and keeping
        // the center at (3,0) would leave the rect at x∈[2,4], y∈[-1,1], a pad
        // centroid ~3 units away from where the fab put it.
        let cl = CenterLinePrimitive {
            exposure: MacroBoolean::Value(true),
            dimensions: (MacroDecimal::Value(2.0), MacroDecimal::Value(2.0)),
            center: (MacroDecimal::Value(3.0), MacroDecimal::Value(0.0)),
            angle: MacroDecimal::Value(90.0),
        };
        let m = ApertureMacro {
            name: "CL".to_string(),
            content: vec![MacroContent::CenterLine(cl)],
        };
        let pts = instantiate_macro(&m, &[], 0.0, 0.0, 1.0).hull;
        let (minx, maxx) = pts
            .iter()
            .fold((f64::MAX, f64::MIN), |(a, b), p| (a.min(p.0), b.max(p.0)));
        let (miny, maxy) = pts
            .iter()
            .fold((f64::MAX, f64::MIN), |(a, b), p| (a.min(p.1), b.max(p.1)));
        assert!(
            (minx + 1.0).abs() < 1e-6 && (maxx - 1.0).abs() < 1e-6,
            "x∈[-1,1], got [{minx},{maxx}]"
        );
        assert!(
            (miny - 2.0).abs() < 1e-6 && (maxy - 4.0).abs() < 1e-6,
            "y∈[2,4], got [{miny},{maxy}]"
        );
    }

    #[test]
    fn outline_applies_rotation_about_origin() {
        use gerber_types::{MacroBoolean, OutlinePrimitive};
        // An off-origin outline (a 2×2 square whose corners sit at x∈[1,3],
        // y∈[0,2]) rotated 90° about the macro origin must land at x∈[-2,0],
        // y∈[1,3]; the rotation both reorients AND translates it. The old
        // handler dropped the angle and left it at x∈[1,3], y∈[0,2], so the pad
        // centroid was in the wrong place and bound to the wrong net.
        let pt = |x: f64, y: f64| (MacroDecimal::Value(x), MacroDecimal::Value(y));
        let outline = OutlinePrimitive {
            exposure: MacroBoolean::Value(true),
            points: vec![
                pt(1.0, 0.0),
                pt(3.0, 0.0),
                pt(3.0, 2.0),
                pt(1.0, 2.0),
                pt(1.0, 0.0),
            ],
            angle: MacroDecimal::Value(90.0),
        };
        let m = ApertureMacro {
            name: "OUT".to_string(),
            content: vec![MacroContent::Outline(outline)],
        };
        let pts = instantiate_macro(&m, &[], 0.0, 0.0, 1.0).hull;
        let (minx, maxx) = pts
            .iter()
            .fold((f64::MAX, f64::MIN), |(a, b), p| (a.min(p.0), b.max(p.0)));
        let (miny, maxy) = pts
            .iter()
            .fold((f64::MAX, f64::MIN), |(a, b), p| (a.min(p.1), b.max(p.1)));
        assert!(
            (minx + 2.0).abs() < 1e-6 && maxx.abs() < 1e-6,
            "x∈[-2,0] after rotation, got [{minx},{maxx}]"
        );
        assert!(
            (miny - 1.0).abs() < 1e-6 && (maxy - 3.0).abs() < 1e-6,
            "y∈[1,3] after rotation, got [{miny},{maxy}]"
        );
    }

    #[test]
    fn vector_line_applies_rotation_about_origin() {
        use gerber_types::{MacroBoolean, VectorLinePrimitive};
        // A horizontal bar from (0,0) to (4,0), width 1, rotated 90°: it becomes
        // a vertical bar spanning y∈[0,4], x∈[-0.5,0.5]. Ignoring the angle would
        // leave it horizontal.
        let vl = VectorLinePrimitive {
            exposure: MacroBoolean::Value(true),
            width: MacroDecimal::Value(1.0),
            start: (MacroDecimal::Value(0.0), MacroDecimal::Value(0.0)),
            end: (MacroDecimal::Value(4.0), MacroDecimal::Value(0.0)),
            angle: MacroDecimal::Value(90.0),
        };
        let m = ApertureMacro {
            name: "VL".to_string(),
            content: vec![MacroContent::VectorLine(vl)],
        };
        let pts = instantiate_macro(&m, &[], 0.0, 0.0, 1.0).hull;
        let (minx, maxx) = pts
            .iter()
            .fold((f64::MAX, f64::MIN), |(a, b), p| (a.min(p.0), b.max(p.0)));
        let (miny, maxy) = pts
            .iter()
            .fold((f64::MAX, f64::MIN), |(a, b), p| (a.min(p.1), b.max(p.1)));
        assert!(
            (maxx - minx - 1.0).abs() < 1e-6,
            "x span ~1 (the width) after 90°, got {}",
            maxx - minx
        );
        assert!(
            (maxy - miny - 4.0).abs() < 1e-6,
            "y span ~4 (the length) after 90°, got {}",
            maxy - miny
        );
    }
}
