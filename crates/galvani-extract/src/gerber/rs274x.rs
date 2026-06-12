//! RS-274X (extended Gerber) copper-layer reader.
//!
//! We adapt the `gerber_parser` crate (which parses RS-274X into the
//! `gerber-types` model) rather than hand-rolling the grammar; aperture macros,
//! coordinate-format scaling, polarity and the deprecated codes are all its
//! problem. Our job is to *replay* its command stream as a plotter would and
//! emit solid copper primitives (capsules / polygons in board mm) for the
//! connectivity tracer.
//!
//! What we model:
//!   - **Apertures**: circle (disc), rectangle, obround, regular polygon,
//!     and macros (the common primitives: circle, center-line, vector-line,
//!     outline, polygon). A flash stamps the aperture shape at the current
//!     point; a draw with a circular aperture sweeps a capsule of that width.
//!   - **Operations**: D01 interpolate (linear -> capsule; circular -> arc
//!     flattened to capsules), D02 move, D03 flash.
//!   - **Regions** (G36/G37): the accumulated contour becomes a filled polygon
//!     (a pour / copper fill).
//!   - **Polarity** (LPD/LPC): clear (LPC) primitives erase copper. For
//!     *connectivity* we treat the board as additive: a thermal-relief or
//!     antipad clearing a sliver inside a pour does not disconnect the net the
//!     way it would change a rendered image. We therefore skip clear primitives
//!     for connectivity (documented limitation: a deliberately split pour drawn
//!     as one dark fill minus a clear gap is read as still-connected). KiCad's
//!     fills are emitted as separate dark regions, so this is rarely wrong in
//!     practice.

use std::io::BufReader;

use gerber_types::{
    Aperture, Command, ExtendedCode, FunctionCode, GCode, Operation, Polarity,
};
use gerber_types::{Circle, Polygon as GPolygon, Rectangular};
use gerber_types::{CoordinateNumber, Coordinates, InterpolationMode};

use super::geo::{Capsule, Shape};
use super::macros::instantiate_macro;

/// Arc flattening resolution (matches drc's ARC_SEGMENTS).
const ARC_SEGMENTS: usize = 16;

/// One solid copper region the plotter painted, with the aperture/flash kind so
/// the tracer can tell pads (flashes) from routing (draws) from pours.
#[derive(Debug, Clone)]
pub struct CopperPrim {
    pub shape: Shape,
    pub kind: PrimKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrimKind {
    /// D03 flash of a pad-like aperture: a candidate component pad.
    Flash,
    /// D01 draw: routing track / arc.
    Track,
    /// G36/G37 region: a pour / fill.
    Region,
    /// A synthesised plated-drill disc (via / through-hole barrel). Stitches
    /// layers and carries a net like any copper, but is *not* a component pad,
    /// so it is excluded from pad assignment. Real through-hole component pads
    /// still bind via their copper-gerber annular-ring flash.
    Via,
}

/// Parse one RS-274X copper layer's text into solid copper primitives.
pub fn parse_layer(text: &str) -> Result<Vec<CopperPrim>, String> {
    let normalized = normalize_rs274x(text);
    // `parse` returns the partially-built document even on a hard error (the
    // error and the doc-so-far are paired). Per-command parse errors are kept
    // inside `doc.commands` as `Err` and skipped by `doc.commands()`. We want
    // every primitive we *can* recover, so we take the doc in both cases; a
    // truly empty/garbage file simply yields no primitives.
    let doc = match gerber_parser::parse(BufReader::new(normalized.as_bytes())) {
        Ok(doc) => doc,
        Err((doc, _err)) => doc,
    };

    let mut plotter = Plotter::new(&doc);
    for cmd in doc.commands() {
        plotter.run(cmd);
    }
    Ok(plotter.out)
}

/// Normalise older / vendor RS-274X dialects into the strict form the
/// `gerber_parser` regexes accept. Real fab gerbers (e.g. Allegro `.art`
/// exports like the uConsole mainboard) differ from the textbook form in two
/// ways the parser rejects outright, which otherwise drops the *entire* layer:
///
///   1. **Multi-statement extended blocks.** A single `%...%` may pack several
///      statements: `%FSAX55Y55*MOIN*%` or
///      `%IR0*IPPOS*OFA0B0*MIA0B0*SFA1B1*%`. The parser expects one statement
///      per `%...%`. We split each inner `...*` into its own `%...*%`.
///   2. **FS without a zero-omission char.** Allegro writes `%FSAX55Y55*`
///      (absolute, 5.5) with no leading `L`/`T`; the parser's regex requires
///      one. Coordinates in these files are zero-padded to the full width, so
///      inserting `L` (omit-leading, a no-op on full-width numbers) is exact.
///
/// Everything else is passed through untouched, so well-formed KiCad/JLCPCB
/// gerbers are unaffected (their `%...%` blocks are already single-statement
/// and their FS already carries the zero char).
fn normalize_rs274x(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        let trimmed = line.trim();
        // Only extended-code lines (start and end with %) can need splitting.
        if trimmed.starts_with('%') && trimmed.ends_with('%') && trimmed.len() > 2 {
            let inner = &trimmed[1..trimmed.len() - 1];
            // Count statements (each ends with '*'). One is the normal case.
            let stmts: Vec<&str> = inner.split('*').filter(|s| !s.is_empty()).collect();
            if stmts.len() > 1 {
                for s in stmts {
                    out.push('%');
                    out.push_str(&patch_fs(s));
                    out.push_str("*%\n");
                }
                continue;
            } else if stmts.len() == 1 {
                out.push('%');
                out.push_str(&patch_fs(stmts[0]));
                out.push_str("*%\n");
                continue;
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Insert the leading-zero-omission char into a bare `FSA…`/`FSI…` statement.
fn patch_fs(stmt: &str) -> String {
    if let Some(rest) = stmt.strip_prefix("FS") {
        // Already `FSL…`/`FST…`? leave it.
        if rest.starts_with('L') || rest.starts_with('T') {
            return stmt.to_string();
        }
        // `FSA…` / `FSI…` (absolute/incremental with no zero char): add `L`.
        if rest.starts_with('A') || rest.starts_with('I') {
            return format!("FSL{rest}");
        }
    }
    stmt.to_string()
}

fn num(c: &CoordinateNumber) -> f64 {
    // gerber-types stores nanounits; Into<f64> yields the value in the
    // document's unit (mm or inch). We normalise inch->mm at the call site.
    (*c).into()
}

struct Plotter<'a> {
    doc: &'a gerber_parser::GerberDoc,
    /// inch->mm factor (1.0 if already mm).
    to_mm: f64,
    x: f64,
    y: f64,
    aperture: Option<i32>,
    interp: InterpolationMode,
    /// Inside a G36 region: accumulating contour points.
    region: Option<Vec<(f64, f64)>>,
    /// Current load polarity. Clear (LPC) primitives are skipped (see header).
    dark: bool,
    out: Vec<CopperPrim>,
}

impl<'a> Plotter<'a> {
    fn new(doc: &'a gerber_parser::GerberDoc) -> Self {
        let to_mm = match doc.units {
            Some(gerber_types::Unit::Inches) => 25.4,
            _ => 1.0,
        };
        Plotter {
            doc,
            to_mm,
            x: 0.0,
            y: 0.0,
            aperture: None,
            interp: InterpolationMode::Linear,
            region: None,
            dark: true,
            out: Vec::new(),
        }
    }

    fn coord(&self, c: &Coordinates) -> (f64, f64) {
        let nx = c.x.as_ref().map(num).map(|v| v * self.to_mm).unwrap_or(self.x);
        let ny = c.y.as_ref().map(num).map(|v| v * self.to_mm).unwrap_or(self.y);
        (nx, ny)
    }

    fn run(&mut self, cmd: &Command) {
        match cmd {
            Command::FunctionCode(FunctionCode::GCode(g)) => match g {
                GCode::InterpolationMode(m) => self.interp = *m,
                GCode::RegionMode(true) => self.region = Some(Vec::new()),
                GCode::RegionMode(false) => self.close_region(),
                _ => {}
            },
            Command::FunctionCode(FunctionCode::DCode(d)) => match d {
                gerber_types::DCode::SelectAperture(code) => self.aperture = Some(*code),
                gerber_types::DCode::Operation(op) => self.operation(op),
            },
            Command::ExtendedCode(ExtendedCode::LoadPolarity(p)) => {
                self.dark = matches!(p, Polarity::Dark);
            }
            _ => {}
        }
    }

    fn operation(&mut self, op: &Operation) {
        match op {
            Operation::Move(coord) => {
                if let Some(c) = coord {
                    let (nx, ny) = self.coord(c);
                    self.x = nx;
                    self.y = ny;
                }
            }
            Operation::Interpolate(coord, offset) => {
                let (sx, sy) = (self.x, self.y);
                let (ex, ey) = coord.as_ref().map(|c| self.coord(c)).unwrap_or((sx, sy));
                if let Some(pts) = self.region.as_mut() {
                    // Region contour: just collect the vertices.
                    if pts.is_empty() {
                        pts.push((sx, sy));
                    }
                    pts.push((ex, ey));
                } else if self.dark {
                    // A routed segment of the current aperture's width.
                    let width = self.aperture_line_width();
                    match self.interp {
                        InterpolationMode::Linear => {
                            self.push_capsule(sx, sy, ex, ey, width / 2.0);
                        }
                        InterpolationMode::ClockwiseCircular
                        | InterpolationMode::CounterclockwiseCircular => {
                            let (ox, oy) = offset
                                .as_ref()
                                .map(|o| {
                                    (
                                        o.x.as_ref().map(num).unwrap_or(0.0) * self.to_mm,
                                        o.y.as_ref().map(num).unwrap_or(0.0) * self.to_mm,
                                    )
                                })
                                .unwrap_or((0.0, 0.0));
                            let ccw =
                                matches!(self.interp, InterpolationMode::CounterclockwiseCircular);
                            self.push_arc(sx, sy, ex, ey, ox, oy, ccw, width / 2.0);
                        }
                    }
                }
                self.x = ex;
                self.y = ey;
            }
            Operation::Flash(coord) => {
                if let Some(c) = coord {
                    let (nx, ny) = self.coord(c);
                    self.x = nx;
                    self.y = ny;
                }
                if self.dark {
                    self.flash();
                }
            }
        }
    }

    /// The effective draw width: a draw is only well-defined with a circular
    /// aperture; for non-circular we use the smaller dimension as a width
    /// (KiCad always routes with round apertures, so this is the common path).
    fn aperture_line_width(&self) -> f64 {
        match self.aperture.and_then(|a| self.doc.apertures.get(&a)) {
            Some(Aperture::Circle(Circle { diameter, .. })) => diameter * self.to_mm,
            Some(Aperture::Rectangle(Rectangular { x, y, .. }))
            | Some(Aperture::Obround(Rectangular { x, y, .. })) => x.min(*y) * self.to_mm,
            _ => 0.1 * self.to_mm, // unknown: a hairline, still connects endpoints
        }
    }

    fn flash(&mut self) {
        let Some(code) = self.aperture else { return };
        let Some(ap) = self.doc.apertures.get(&code) else { return };
        let (cx, cy) = (self.x, self.y);
        let s = self.to_mm;
        let shape = match ap {
            Aperture::Circle(Circle { diameter, .. }) => Shape::disc(cx, cy, diameter * s / 2.0),
            Aperture::Rectangle(Rectangular { x, y, .. }) => {
                rect_polygon(cx, cy, x * s, y * s, 0.0)
            }
            Aperture::Obround(Rectangular { x, y, .. }) => {
                // Obround = stadium; model as a capsule along the long axis.
                let (w, h) = (x * s, y * s);
                if w >= h {
                    let r = h / 2.0;
                    Shape::Capsule(Capsule {
                        ax: cx - (w - h) / 2.0,
                        ay: cy,
                        bx: cx + (w - h) / 2.0,
                        by: cy,
                        r,
                    })
                } else {
                    let r = w / 2.0;
                    Shape::Capsule(Capsule {
                        ax: cx,
                        ay: cy - (h - w) / 2.0,
                        bx: cx,
                        by: cy + (h - w) / 2.0,
                        r,
                    })
                }
            }
            Aperture::Polygon(GPolygon { diameter, vertices, rotation, .. }) => {
                regular_polygon(cx, cy, diameter * s / 2.0, *vertices, rotation.unwrap_or(0.0))
            }
            Aperture::Macro(name, args) => {
                match self.doc.commands().iter().find_map(|c| match c {
                    Command::ExtendedCode(ExtendedCode::ApertureMacro(m)) if &m.name == name => {
                        Some(m)
                    }
                    _ => None,
                }) {
                    Some(m) => {
                        let pts = instantiate_macro(m, args.as_deref().unwrap_or(&[]), cx, cy, s);
                        if pts.len() >= 3 {
                            Shape::Polygon { pts, r: 0.0 }
                        } else {
                            // Couldn't evaluate (variables/expressions we don't
                            // support): fall back to a small disc so the flash
                            // still anchors a pad rather than vanishing.
                            Shape::disc(cx, cy, 0.25 * s)
                        }
                    }
                    None => Shape::disc(cx, cy, 0.25 * s),
                }
            }
        };
        self.out.push(CopperPrim { shape, kind: PrimKind::Flash });
    }

    fn push_capsule(&mut self, ax: f64, ay: f64, bx: f64, by: f64, r: f64) {
        self.out.push(CopperPrim {
            shape: Shape::Capsule(Capsule { ax, ay, bx, by, r }),
            kind: PrimKind::Track,
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn push_arc(&mut self, sx: f64, sy: f64, ex: f64, ey: f64, ox: f64, oy: f64, ccw: bool, r: f64) {
        let cx = sx + ox;
        let cy = sy + oy;
        let radius = (ox * ox + oy * oy).sqrt();
        if radius <= f64::EPSILON {
            self.push_capsule(sx, sy, ex, ey, r);
            return;
        }
        let a0 = (sy - cy).atan2(sx - cx);
        let mut a1 = (ey - cy).atan2(ex - cx);
        // Choose sweep direction.
        use std::f64::consts::TAU;
        if ccw {
            while a1 <= a0 {
                a1 += TAU;
            }
        } else {
            while a1 >= a0 {
                a1 -= TAU;
            }
        }
        let sweep = a1 - a0;
        let mut prev = (sx, sy);
        for i in 1..=ARC_SEGMENTS {
            let t = i as f64 / ARC_SEGMENTS as f64;
            let a = a0 + sweep * t;
            let p = (cx + radius * a.cos(), cy + radius * a.sin());
            self.push_capsule(prev.0, prev.1, p.0, p.1, r);
            prev = p;
        }
    }

    fn close_region(&mut self) {
        if let Some(pts) = self.region.take() {
            if pts.len() >= 3 {
                self.out.push(CopperPrim {
                    shape: Shape::Polygon { pts, r: 0.0 },
                    kind: PrimKind::Region,
                });
            }
        }
    }
}

/// Axis-aligned rectangle polygon centred at (cx, cy).
fn rect_polygon(cx: f64, cy: f64, w: f64, h: f64, _r: f64) -> Shape {
    let (hw, hh) = (w / 2.0, h / 2.0);
    Shape::Polygon {
        pts: vec![
            (cx - hw, cy - hh),
            (cx + hw, cy - hh),
            (cx + hw, cy + hh),
            (cx - hw, cy + hh),
        ],
        r: 0.0,
    }
}

/// Regular n-gon inscribed in a circle of radius `r`, rotated `rot_deg`.
fn regular_polygon(cx: f64, cy: f64, r: f64, vertices: u8, rot_deg: f64) -> Shape {
    let n = vertices.max(3) as usize;
    let rot = rot_deg.to_radians();
    let mut pts = Vec::with_capacity(n);
    for i in 0..n {
        let a = rot + (i as f64) * std::f64::consts::TAU / n as f64;
        pts.push((cx + r * a.cos(), cy + r * a.sin()));
    }
    Shape::Polygon { pts, r: 0.0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIMPLE: &str = "\
%FSLAX46Y46*%
%MOMM*%
%ADD10C,0.500000*%
%ADD11R,1.000000X2.000000*%
G01*
D10*
X0Y0D02*
X5000000Y0D01*
D11*
X10000000Y0D03*
M02*
";

    #[test]
    fn parses_track_and_pad() {
        let prims = parse_layer(SIMPLE).unwrap();
        // One track (the draw) and one flash (the rect pad).
        assert!(prims.iter().any(|p| p.kind == PrimKind::Track));
        let flash = prims.iter().find(|p| p.kind == PrimKind::Flash).unwrap();
        // Rect flash -> polygon centred at (10, 0).
        if let Shape::Polygon { pts, .. } = &flash.shape {
            let cx = pts.iter().map(|p| p.0).sum::<f64>() / pts.len() as f64;
            assert!((cx - 10.0).abs() < 1e-6, "pad centre at {cx}");
        } else {
            panic!("rect flash should be a polygon");
        }
    }

    #[test]
    fn normalizes_allegro_fs_and_combined_blocks() {
        // Allegro dialect: FS without zero char, FS+MO combined in one block.
        let g = "\
%FSAX55Y55*MOIN*%
%IR0*IPPOS*OFA0.00000B0.00000*%
%ADD10C,0.040000*%
D10*
X0000050000Y0000050000D03*
M02*
";
        let prims = parse_layer(g).unwrap();
        // The flash must survive: FS was patched and the combined block split,
        // so the format spec is set and the coordinate op is honoured.
        let f = prims.iter().find(|p| p.kind == PrimKind::Flash).unwrap();
        if let Shape::Capsule(c) = &f.shape {
            // 5.5 inch format: 0000050000 = 0.5 inch = 12.7 mm.
            assert!((c.ax - 12.7).abs() < 1e-3, "x was {}", c.ax);
        } else {
            panic!("round flash should be a disc");
        }
    }

    #[test]
    fn round_flash_is_disc() {
        let g = "\
%FSLAX46Y46*%
%MOMM*%
%ADD10C,1.000000*%
D10*
X2000000Y3000000D03*
M02*
";
        let prims = parse_layer(g).unwrap();
        let f = prims.iter().find(|p| p.kind == PrimKind::Flash).unwrap();
        if let Shape::Capsule(c) = &f.shape {
            assert!((c.ax - 2.0).abs() < 1e-6 && (c.ay - 3.0).abs() < 1e-6);
            assert!((c.r - 0.5).abs() < 1e-6);
        } else {
            panic!("round flash should be a disc/capsule");
        }
    }
}
