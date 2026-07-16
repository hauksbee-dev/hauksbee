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
//! Variable substitution handles `$1..$n` from the flash's positional
//! arguments and a small arithmetic evaluator covers the `$1+$1`,
//! `$2-$3`, `0.5*$4` forms KiCad emits. Anything we cannot evaluate
//! (nested macro variable definitions, transcendental expressions) is dropped
//! from that primitive; if nothing survives, the caller falls back to a disc.

use std::collections::HashMap;

use gerber_types::{ApertureMacro, MacroBoolean, MacroContent, MacroDecimal, MacroInteger};

/// Evaluate a macro into the convex-hull polygon of its solid area, centred at
/// the flash point `(cx, cy)` and scaled by `s` (inch->mm or 1.0). `args` are
/// the flash's positional parameters (`$1` = args[0], ...).
pub fn instantiate_macro(
    m: &ApertureMacro,
    args: &[MacroDecimal],
    cx: f64,
    cy: f64,
    s: f64,
) -> Vec<(f64, f64)> {
    let mut vars: HashMap<u32, f64> = HashMap::new();
    for (i, a) in args.iter().enumerate() {
        if let Some(v) = decimal(a, &vars) {
            vars.insert((i + 1) as u32, v);
        }
    }

    let mut pts: Vec<(f64, f64)> = Vec::new();
    for content in &m.content {
        match content {
            MacroContent::VariableDefinition(def) => {
                if let Some(v) = eval_expr(&def.expression, &vars) {
                    vars.insert(def.number, v);
                }
            }
            MacroContent::Circle(c) => {
                if !exposed(&c.exposure, &vars) {
                    continue;
                }
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
                // MOVES the center, so it must be applied — dropping it placed a
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
                // Sample the circle as an octagon (hull absorbs the rest).
                for k in 0..8 {
                    let a = k as f64 * std::f64::consts::TAU / 8.0;
                    pts.push((cxr + r * a.cos(), cyr + r * a.sin()));
                }
            }
            MacroContent::VectorLine(l) => {
                if !exposed(&l.exposure, &vars) {
                    continue;
                }
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
                let (sin, cos) = decimal(&l.angle, &vars).unwrap_or(0.0).to_radians().sin_cos();
                let (rx0, ry0) = (x0 * cos - y0 * sin, x0 * sin + y0 * cos);
                let (rx1, ry1) = (x1 * cos - y1 * sin, x1 * sin + y1 * cos);
                push_thick_segment(&mut pts, rx0, ry0, rx1, ry1, w / 2.0);
            }
            MacroContent::CenterLine(l) => {
                if !exposed(&l.exposure, &vars) {
                    continue;
                }
                let (Some(w), Some(h), Some(x), Some(y)) = (
                    decimal(&l.dimensions.0, &vars),
                    decimal(&l.dimensions.1, &vars),
                    decimal(&l.center.0, &vars),
                    decimal(&l.center.1, &vars),
                ) else {
                    continue;
                };
                let (hw, hh) = (w / 2.0, h / 2.0);
                // The CenterLine primitive carries a rotation angle; the
                // earlier axis-aligned emission silently dropped it, so a
                // rotated rectangular pad was reconstructed axis-aligned.
                // Rotate each corner offset, then translate by the center
                // (matching the Polygon primitive's handling above).
                let (sin, cos) = decimal(&l.angle, &vars).unwrap_or(0.0).to_radians().sin_cos();
                for (dx, dy) in [(-hw, -hh), (hw, -hh), (hw, hh), (-hw, hh)] {
                    pts.push((x + dx * cos - dy * sin, y + dx * sin + dy * cos));
                }
            }
            MacroContent::Outline(o) => {
                if !exposed(&o.exposure, &vars) {
                    continue;
                }
                // The outline primitive rotates about the macro origin (0,0);
                // its points are absolute macro coordinates, so the rotation both
                // reorients AND (for an off-origin outline) translates the shape.
                // Dropping it — the sibling of the CenterLine bug — reconstructed
                // a rotated custom pad at the wrong orientation and location.
                let (sin, cos) = decimal(&o.angle, &vars).unwrap_or(0.0).to_radians().sin_cos();
                for (px, py) in &o.points {
                    if let (Some(x), Some(y)) = (decimal(px, &vars), decimal(py, &vars)) {
                        pts.push((x * cos - y * sin, x * sin + y * cos));
                    }
                }
            }
            MacroContent::Polygon(p) => {
                if !exposed(&p.exposure, &vars) {
                    continue;
                }
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
                for k in 0..n {
                    let a = rot + k as f64 * std::f64::consts::TAU / n as f64;
                    pts.push((x + r * a.cos(), y + r * a.sin()));
                }
            }
            // Moire/Thermal are fiducial/relief shapes, not pads we bind to.
            _ => {}
        }
    }

    if pts.len() < 3 {
        return Vec::new();
    }
    let hull = convex_hull(pts);
    hull.into_iter()
        .map(|(x, y)| (cx + x * s, cy + y * s))
        .collect()
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
        // trusted — otherwise it drives a multi-gigabyte vertex push (OOM/hang).
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
        let pts = instantiate_macro(&m, &[], 0.0, 0.0, 1.0);
        assert!(pts.len() <= 12, "vertex count must be clamped, got {}", pts.len());
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
        let m = ApertureMacro { name: "CL".to_string(), content: vec![MacroContent::CenterLine(cl)] };
        let pts = instantiate_macro(&m, &[], 0.0, 0.0, 1.0);
        let (minx, maxx) = pts.iter().fold((f64::MAX, f64::MIN), |(a, b), p| (a.min(p.0), b.max(p.0)));
        let (miny, maxy) = pts.iter().fold((f64::MAX, f64::MIN), |(a, b), p| (a.min(p.1), b.max(p.1)));
        assert!((maxx - minx - 2.0).abs() < 1e-6, "x span {} (expected ~2 after 90° rotation)", maxx - minx);
        assert!((maxy - miny - 4.0).abs() < 1e-6, "y span {} (expected ~4 after 90° rotation)", maxy - miny);
    }

    #[test]
    fn outline_applies_rotation_about_origin() {
        use gerber_types::{MacroBoolean, OutlinePrimitive};
        // An off-origin outline (a 2×2 square whose corners sit at x∈[1,3],
        // y∈[0,2]) rotated 90° about the macro origin must land at x∈[-2,0],
        // y∈[1,3] — the rotation both reorients AND translates it. The old
        // handler dropped the angle and left it at x∈[1,3], y∈[0,2], so the pad
        // centroid was in the wrong place and bound to the wrong net.
        let pt = |x: f64, y: f64| (MacroDecimal::Value(x), MacroDecimal::Value(y));
        let outline = OutlinePrimitive {
            exposure: MacroBoolean::Value(true),
            points: vec![pt(1.0, 0.0), pt(3.0, 0.0), pt(3.0, 2.0), pt(1.0, 2.0), pt(1.0, 0.0)],
            angle: MacroDecimal::Value(90.0),
        };
        let m = ApertureMacro { name: "OUT".to_string(), content: vec![MacroContent::Outline(outline)] };
        let pts = instantiate_macro(&m, &[], 0.0, 0.0, 1.0);
        let (minx, maxx) = pts.iter().fold((f64::MAX, f64::MIN), |(a, b), p| (a.min(p.0), b.max(p.0)));
        let (miny, maxy) = pts.iter().fold((f64::MAX, f64::MIN), |(a, b), p| (a.min(p.1), b.max(p.1)));
        assert!((minx + 2.0).abs() < 1e-6 && maxx.abs() < 1e-6, "x∈[-2,0] after rotation, got [{minx},{maxx}]");
        assert!((miny - 1.0).abs() < 1e-6 && (maxy - 3.0).abs() < 1e-6, "y∈[1,3] after rotation, got [{miny},{maxy}]");
    }

    #[test]
    fn vector_line_applies_rotation_about_origin() {
        use gerber_types::{MacroBoolean, VectorLinePrimitive};
        // A horizontal bar from (0,0) to (4,0), width 1, rotated 90°: it becomes
        // a vertical bar spanning y∈[0,4], x∈[-0.5,0.5]. The old handler ignored
        // the angle and left it horizontal.
        let vl = VectorLinePrimitive {
            exposure: MacroBoolean::Value(true),
            width: MacroDecimal::Value(1.0),
            start: (MacroDecimal::Value(0.0), MacroDecimal::Value(0.0)),
            end: (MacroDecimal::Value(4.0), MacroDecimal::Value(0.0)),
            angle: MacroDecimal::Value(90.0),
        };
        let m = ApertureMacro { name: "VL".to_string(), content: vec![MacroContent::VectorLine(vl)] };
        let pts = instantiate_macro(&m, &[], 0.0, 0.0, 1.0);
        let (minx, maxx) = pts.iter().fold((f64::MAX, f64::MIN), |(a, b), p| (a.min(p.0), b.max(p.0)));
        let (miny, maxy) = pts.iter().fold((f64::MAX, f64::MIN), |(a, b), p| (a.min(p.1), b.max(p.1)));
        assert!((maxx - minx - 1.0).abs() < 1e-6, "x span ~1 (the width) after 90°, got {}", maxx - minx);
        assert!((maxy - miny - 4.0).abs() < 1e-6, "y span ~4 (the length) after 90°, got {}", maxy - miny);
    }
}
