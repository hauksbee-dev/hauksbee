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
                // Sample the circle as an octagon (hull absorbs the rest).
                for k in 0..8 {
                    let a = k as f64 * std::f64::consts::TAU / 8.0;
                    pts.push((x + r * a.cos(), y + r * a.sin()));
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
                push_thick_segment(&mut pts, x0, y0, x1, y1, w / 2.0);
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
                pts.extend([
                    (x - hw, y - hh),
                    (x + hw, y - hh),
                    (x + hw, y + hh),
                    (x - hw, y + hh),
                ]);
            }
            MacroContent::Outline(o) => {
                if !exposed(&o.exposure, &vars) {
                    continue;
                }
                for (px, py) in &o.points {
                    if let (Some(x), Some(y)) = (decimal(px, &vars), decimal(py, &vars)) {
                        pts.push((x, y));
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
}
