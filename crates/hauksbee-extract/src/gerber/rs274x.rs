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
//!
//! Long-form how-and-why: docs/how-and-why/hauksbee-extract/gerber.md.

use std::io::BufReader;

use gerber_types::{
    Aperture, Command, ExtendedCode, FunctionCode, GCode, Operation, Polarity, StepAndRepeat,
};
use gerber_types::{Circle, Polygon as GPolygon, Rectangular};
use gerber_types::{CoordinateNumber, CoordinateOffset, Coordinates, InterpolationMode};

use super::geo::{point_in_polygon, Capsule, Shape};
use super::macros::instantiate_macro;

/// Arc flattening resolution (matches drc's ARC_SEGMENTS).
const ARC_SEGMENTS: usize = 16;

/// Radius (mm) of the small disc a macro flash falls back to when its aperture
/// macro cannot be instantiated. A fixed physical size, never scaled by the
/// document's unit factor, so an inch-unit board gets the same 0.25 mm anchor
/// as a millimetre one rather than a 6.35 mm blob.
const MACRO_FALLBACK_DISC_MM: f64 = 0.25;

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
    // A well-formed file closes every `%SR%` with `%SR*%`, but tolerate a block
    // left open at end-of-file (M02 without an explicit close) by flushing it.
    plotter.flush_step_repeat();
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
            // An aperture macro (`%AM<name>*<primitive>*...*%`) is a SINGLE
            // statement whose `*`-separated parts are its primitives, not
            // independent extended codes. Splitting it would yield an empty
            // `%AM<name>*%` plus orphan primitive blocks, silently collapsing
            // the pad to a fallback disc. Pass macro blocks through untouched.
            if inner.starts_with("AM") {
                out.push_str(line);
                out.push('\n');
                continue;
            }
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
    /// Inside a G36 region: the accumulated contours. RS-274X 4.10.4 lets one
    /// region carry several closed contours (each begun by a D02 move), an
    /// outer boundary plus holes cut out of it, or several disjoint islands,
    /// so contours are kept SEPARATE; the last entry is the contour currently
    /// being drawn. Flattening them into one ring bridged the pieces with
    /// phantom edges (false shorts across islands, holes filled back in).
    region: Option<Vec<Vec<(f64, f64)>>>,
    /// Polarity in effect when the current region OPENED. A region is a single
    /// primitive, so its whole fill takes the polarity at G36 time; captured
    /// here so a clear (LPC) region is dropped like a clear draw/flash, instead
    /// of being materialized as phantom additive copper.
    region_dark: bool,
    /// Current load polarity. Clear (LPC) primitives are skipped (see header).
    dark: bool,
    /// Arc interpolation quadrant mode. `false` = multi-quadrant (G75, the
    /// modern default and what KiCad emits): I/J are signed vectors to the
    /// centre. `true` = single-quadrant (G74, legacy CAM dialects): I/J are
    /// unsigned magnitudes and the true centre is one of the four ±I,±J
    /// candidates, chosen per RS-274X §4.5.
    single_quadrant: bool,
    /// An open step-and-repeat (`%SR%`) block: the primitives emitted while it
    /// is open form the base cell, replicated across the grid when it closes.
    sr: Option<SrBlock>,
    out: Vec<CopperPrim>,
}

/// State for an open `%SRXnYnInJn*%` step-and-repeat block: the grid to tile the
/// base cell over and the index into `out` where the base cell's primitives
/// begin. Distances are stored in board millimetres.
struct SrBlock {
    start: usize,
    repeat_x: u32,
    repeat_y: u32,
    step_x_mm: f64,
    step_y_mm: f64,
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
            region_dark: true,
            dark: true,
            single_quadrant: false,
            sr: None,
            out: Vec::new(),
        }
    }

    fn coord(&self, c: &Coordinates) -> (f64, f64) {
        let nx =
            c.x.as_ref()
                .map(num)
                .map(|v| v * self.to_mm)
                .unwrap_or(self.x);
        let ny =
            c.y.as_ref()
                .map(num)
                .map(|v| v * self.to_mm)
                .unwrap_or(self.y);
        (nx, ny)
    }

    fn run(&mut self, cmd: &Command) {
        match cmd {
            Command::FunctionCode(FunctionCode::GCode(g)) => match g {
                GCode::InterpolationMode(m) => self.interp = *m,
                GCode::RegionMode(true) => {
                    // A region takes the polarity in effect when it opens.
                    self.region_dark = self.dark;
                    // One (empty) current contour; further contours are opened
                    // by D02 moves while the region is open.
                    self.region = Some(vec![Vec::new()]);
                }
                GCode::RegionMode(false) => self.close_region(),
                GCode::QuadrantMode(m) => {
                    self.single_quadrant = matches!(m, gerber_types::QuadrantMode::Single);
                }
                _ => {}
            },
            Command::FunctionCode(FunctionCode::DCode(d)) => match d {
                gerber_types::DCode::SelectAperture(code) => self.aperture = Some(*code),
                gerber_types::DCode::Operation(op) => self.operation(op),
            },
            Command::ExtendedCode(ExtendedCode::LoadPolarity(p)) => {
                self.dark = matches!(p, Polarity::Dark);
            }
            Command::ExtendedCode(ExtendedCode::StepAndRepeat(sr)) => match sr {
                StepAndRepeat::Open {
                    repeat_x,
                    repeat_y,
                    distance_x,
                    distance_y,
                } => {
                    // A new SR implicitly closes any open one. The primitives
                    // drawn until the matching `%SR*%` are the base cell.
                    self.flush_step_repeat();
                    self.sr = Some(SrBlock {
                        start: self.out.len(),
                        repeat_x: (*repeat_x).max(1),
                        repeat_y: (*repeat_y).max(1),
                        step_x_mm: distance_x * self.to_mm,
                        step_y_mm: distance_y * self.to_mm,
                    });
                }
                StepAndRepeat::Close => self.flush_step_repeat(),
            },
            _ => {}
        }
    }

    /// Close the open step-and-repeat block (if any) by tiling the base cell,
    /// the primitives appended since the block opened, across the `repeat_x` ×
    /// `repeat_y` grid at the I/J step. The base copy at cell (0,0) is already in
    /// `out`; only the other cells are cloned, each translated by its grid
    /// offset. Without this the repeated copies of a panelized/arrayed layer
    /// were silently dropped, losing every pad/track/pour but the first.
    fn flush_step_repeat(&mut self) {
        let Some(block) = self.sr.take() else {
            return;
        };
        if block.start > self.out.len() {
            return;
        }
        let base: Vec<CopperPrim> = self.out[block.start..].to_vec();
        if base.is_empty() {
            return;
        }
        for iy in 0..block.repeat_y {
            for ix in 0..block.repeat_x {
                if ix == 0 && iy == 0 {
                    continue; // the base cell is already emitted in place
                }
                let dx = f64::from(ix) * block.step_x_mm;
                let dy = f64::from(iy) * block.step_y_mm;
                for prim in &base {
                    self.out.push(CopperPrim {
                        shape: prim.shape.translated(dx, dy),
                        kind: prim.kind,
                    });
                }
            }
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
                // Inside a region, a D02 move also TERMINATES the current
                // contour and begins the next one (RS-274X 4.10.4: every
                // contour of a region starts with a D02). The new contour is
                // seeded lazily by its first D01 (which reads the moved-to
                // point as its start), so a redundant D02 before any draw
                // does not leave an empty contour behind.
                if let Some(contours) = self.region.as_mut() {
                    if contours.last().is_some_and(|c| !c.is_empty()) {
                        contours.push(Vec::new());
                    }
                }
            }
            Operation::Interpolate(coord, offset) => {
                let (sx, sy) = (self.x, self.y);
                let (ex, ey) = coord.as_ref().map(|c| self.coord(c)).unwrap_or((sx, sy));
                if self.region.is_some() {
                    // Region contour: collect the boundary vertices. A segment
                    // drawn under circular interpolation (G02/G03) contributes
                    // its flattened arc; the same centre/sweep geometry a
                    // stroked arc sweeps, NOT just its chord: chord-collapsing
                    // turned a round pour drawn as two semicircles into a
                    // zero-area polygon, vanishing its copper entirely.
                    let seg: Vec<(f64, f64)> = match self.interp {
                        InterpolationMode::Linear => vec![(ex, ey)],
                        InterpolationMode::ClockwiseCircular
                        | InterpolationMode::CounterclockwiseCircular => {
                            let (ox, oy) = self.offset_mm(offset);
                            let ccw =
                                matches!(self.interp, InterpolationMode::CounterclockwiseCircular);
                            self.arc_samples(sx, sy, ex, ey, ox, oy, ccw)
                        }
                    };
                    let contour = self
                        .region
                        .as_mut()
                        .and_then(|c| c.last_mut())
                        .expect("an open region always has a current contour");
                    if contour.is_empty() {
                        contour.push((sx, sy));
                    }
                    contour.extend(seg);
                } else if self.dark {
                    // A routed segment of the current aperture's width.
                    let width = self.aperture_line_width();
                    match self.interp {
                        InterpolationMode::Linear => {
                            self.push_capsule(sx, sy, ex, ey, width / 2.0);
                        }
                        InterpolationMode::ClockwiseCircular
                        | InterpolationMode::CounterclockwiseCircular => {
                            let (ox, oy) = self.offset_mm(offset);
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
            // Unknown aperture (polygon/macro/undefined): a fixed 0.1 mm hairline,
            // enough to connect endpoints. This is a physical millimetre size and
            // must NOT be scaled by `to_mm`, on an inch-unit file (`%MOIN%`,
            // to_mm=25.4) `0.1 * to_mm` = 2.54 mm, a fat stroke that union-merges
            // adjacent copper into a false short (the same unit-scaling hazard the
            // MACRO_FALLBACK_DISC_MM constant is documented to avoid).
            _ => 0.1,
        }
    }

    fn flash(&mut self) {
        let Some(code) = self.aperture else { return };
        let Some(ap) = self.doc.apertures.get(&code) else {
            return;
        };
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
            Aperture::Polygon(GPolygon {
                diameter,
                vertices,
                rotation,
                ..
            }) => regular_polygon(
                cx,
                cy,
                diameter * s / 2.0,
                *vertices,
                rotation.unwrap_or(0.0),
            ),
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
                            // still anchors a pad rather than vanishing. The
                            // radius is a fixed physical size (mm); `cx`/`cy` are
                            // already mm, so it must NOT be scaled by `to_mm`,
                            // doing so bloated the anchor to 6.35 mm (0.25 inch)
                            // on an inch-unit file, big enough to merge adjacent
                            // copper into one net.
                            Shape::disc(cx, cy, MACRO_FALLBACK_DISC_MM)
                        }
                    }
                    None => Shape::disc(cx, cy, MACRO_FALLBACK_DISC_MM),
                }
            }
        };
        self.out.push(CopperPrim {
            shape,
            kind: PrimKind::Flash,
        });
    }

    fn push_capsule(&mut self, ax: f64, ay: f64, bx: f64, by: f64, r: f64) {
        self.out.push(CopperPrim {
            shape: Shape::Capsule(Capsule { ax, ay, bx, by, r }),
            kind: PrimKind::Track,
        });
    }

    /// Single-quadrant (G74) centre selection: try the four ±I,±J offsets and
    /// return the one whose start/end radii agree best while keeping the arc
    /// sweep within 90 degrees. `None` if no candidate has a positive radius.
    #[allow(clippy::too_many_arguments)]
    fn single_quadrant_center(
        &self,
        sx: f64,
        sy: f64,
        ex: f64,
        ey: f64,
        ox: f64,
        oy: f64,
        ccw: bool,
    ) -> Option<(f64, f64)> {
        use std::f64::consts::{FRAC_PI_2, TAU};
        let mut best: Option<(f64, f64)> = None;
        let mut best_score = f64::INFINITY;
        for &(sox, soy) in &[(ox, oy), (-ox, oy), (ox, -oy), (-ox, -oy)] {
            let (cx, cy) = (sx + sox, sy + soy);
            let rs = ((sx - cx).powi(2) + (sy - cy).powi(2)).sqrt();
            if rs <= f64::EPSILON {
                continue;
            }
            let re = ((ex - cx).powi(2) + (ey - cy).powi(2)).sqrt();
            let a0 = (sy - cy).atan2(sx - cx);
            let mut a1 = (ey - cy).atan2(ex - cx);
            if ccw {
                while a1 <= a0 {
                    a1 += TAU;
                }
            } else {
                while a1 >= a0 {
                    a1 -= TAU;
                }
            }
            let sweep = (a1 - a0).abs();
            // Consistent radius, and penalise a sweep past the 90-degree
            // single-quadrant limit so the correct centre wins.
            let score = (rs - re).abs() + if sweep > FRAC_PI_2 + 1e-6 { 1e3 } else { 0.0 };
            if score < best_score {
                best_score = score;
                best = Some((cx, cy));
            }
        }
        best
    }

    /// The I/J arc offset of a circular D01, scaled to board millimetres
    /// ((0, 0) when the offset, or either axis, is absent).
    fn offset_mm(&self, offset: &Option<CoordinateOffset>) -> (f64, f64) {
        offset
            .as_ref()
            .map(|o| {
                (
                    o.x.as_ref().map(num).unwrap_or(0.0) * self.to_mm,
                    o.y.as_ref().map(num).unwrap_or(0.0) * self.to_mm,
                )
            })
            .unwrap_or((0.0, 0.0))
    }

    /// Resolve a circular D01's geometry: centre, radius, start angle and
    /// signed sweep, honouring the quadrant mode. This is the ONE place the
    /// centre/sweep math lives; the stroked-arc path (`push_arc`) and the
    /// region-contour path both flatten from these numbers, so a pour boundary
    /// arc lands on byte-identical points to the same arc drawn as a track.
    /// `None` when the radius degenerates (centre on the start point): the
    /// "arc" is then just its chord.
    #[allow(clippy::too_many_arguments)]
    fn arc_params(
        &self,
        sx: f64,
        sy: f64,
        ex: f64,
        ey: f64,
        ox: f64,
        oy: f64,
        ccw: bool,
    ) -> Option<(f64, f64, f64, f64, f64)> {
        let (cx, cy) = if self.single_quadrant {
            // G74: I/J are unsigned magnitudes, so the true centre is one of the
            // four ±I,±J offsets from the start. Per RS-274X §4.5, pick the
            // candidate whose start- and end-radius agree and whose sweep (in
            // the requested direction) is <= 90 degrees; the single-quadrant
            // guarantee. Fall back to the multi-quadrant formula if none fits.
            self.single_quadrant_center(sx, sy, ex, ey, ox, oy, ccw)
                .unwrap_or((sx + ox, sy + oy))
        } else {
            (sx + ox, sy + oy)
        };
        let radius = ((sx - cx) * (sx - cx) + (sy - cy) * (sy - cy)).sqrt();
        if radius <= f64::EPSILON {
            return None;
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
        Some((cx, cy, radius, a0, a1 - a0))
    }

    /// The flattened arc: `ARC_SEGMENTS` points sampled from just past the
    /// start through the endpoint (the start itself is NOT included, so the
    /// samples chain onto a path that already holds it). A degenerate radius
    /// yields just the endpoint; the chord.
    #[allow(clippy::too_many_arguments)]
    fn arc_samples(
        &self,
        sx: f64,
        sy: f64,
        ex: f64,
        ey: f64,
        ox: f64,
        oy: f64,
        ccw: bool,
    ) -> Vec<(f64, f64)> {
        let Some((cx, cy, radius, a0, sweep)) = self.arc_params(sx, sy, ex, ey, ox, oy, ccw) else {
            return vec![(ex, ey)];
        };
        (1..=ARC_SEGMENTS)
            .map(|i| {
                let a = a0 + sweep * (i as f64 / ARC_SEGMENTS as f64);
                (cx + radius * a.cos(), cy + radius * a.sin())
            })
            .collect()
    }

    #[allow(clippy::too_many_arguments)]
    fn push_arc(
        &mut self,
        sx: f64,
        sy: f64,
        ex: f64,
        ey: f64,
        ox: f64,
        oy: f64,
        ccw: bool,
        r: f64,
    ) {
        let mut prev = (sx, sy);
        for p in self.arc_samples(sx, sy, ex, ey, ox, oy, ccw) {
            self.push_capsule(prev.0, prev.1, p.0, p.1, r);
            prev = p;
        }
    }

    fn close_region(&mut self) {
        let Some(contours) = self.region.take() else {
            return;
        };
        // A clear-polarity region is a cut-out, not copper, dropping it
        // matches the draw/flash handling and the module's polarity
        // contract. Materializing it would union nets across a gap.
        if !self.region_dark {
            return;
        }
        // Contours that enclose no area (a stray D02 with no draws, a lone
        // segment) are dropped, as the flat model always did.
        let contours: Vec<Vec<(f64, f64)>> =
            contours.into_iter().filter(|c| c.len() >= 3).collect();
        if contours.is_empty() {
            return;
        }
        // The overwhelmingly common case (KiCad emits one contour per G36
        // block): a plain polygon, exactly as before.
        if contours.len() == 1 {
            let pts = contours.into_iter().next().unwrap();
            self.out.push(CopperPrim {
                shape: Shape::Polygon { pts, r: 0.0 },
                kind: PrimKind::Region,
            });
            return;
        }
        // Several contours in one region (RS-274X 4.10.4): group them into the
        // physically-connected pieces of copper they fill, because the
        // connectivity tracer unions per PRIMITIVE, two disjoint islands
        // sharing one primitive would falsely short their nets. Nesting depth
        // (how many other contours enclose a contour) classifies each one: an
        // even-depth contour is an outer boundary; its own piece of copper,
        // and an odd-depth contour is a hole cut out of its immediate
        // (depth-1) parent. Legal region contours never cross, so any single
        // vertex is a valid containment witness. An island nested inside a
        // hole (depth 2) is an outer again: its copper is electrically
        // separate from the surrounding ring's.
        let n = contours.len();
        // encloses[i] = the contours strictly containing contour i.
        let encloses: Vec<Vec<usize>> = (0..n)
            .map(|i| {
                let (px, py) = contours[i][0];
                (0..n)
                    .filter(|&j| j != i && point_in_polygon(px, py, &contours[j]))
                    .collect()
            })
            .collect();
        // Each contour's emit group: an outer owns itself; a hole belongs to
        // its immediate parent; the DEEPEST contour enclosing it (depth-1,
        // which is even, i.e. an outer).
        let group_of: Vec<usize> = (0..n)
            .map(|i| {
                if encloses[i].len().is_multiple_of(2) {
                    i
                } else {
                    encloses[i]
                        .iter()
                        .copied()
                        .max_by_key(|&j| encloses[j].len())
                        .expect("an odd-depth contour has at least one encloser")
                }
            })
            .collect();
        let mut buckets: Vec<Vec<Vec<(f64, f64)>>> = (0..n).map(|_| Vec::new()).collect();
        for (i, c) in contours.into_iter().enumerate() {
            if group_of[i] == i {
                buckets[i].insert(0, c); // the outer boundary leads its group
            } else {
                buckets[group_of[i]].push(c);
            }
        }
        for bucket in buckets.into_iter().filter(|b| !b.is_empty()) {
            let shape = if bucket.len() == 1 {
                // A hole-less island: a plain polygon, like a lone contour.
                let pts = bucket.into_iter().next().unwrap();
                Shape::Polygon { pts, r: 0.0 }
            } else {
                // Outer + holes: even-odd containment reads the ring as copper
                // and the hole interiors as empty.
                Shape::MultiPolygon { contours: bucket }
            };
            self.out.push(CopperPrim {
                shape,
                kind: PrimKind::Region,
            });
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

    // A G36/G37 square region; body differs only by the leading %LPC*%.
    fn region_layer(polarity: &str) -> &'static str {
        // (returns one of the two consts below)
        if polarity == "clear" {
            "\
%FSLAX46Y46*%
%MOMM*%
%LPC*%
G36*
X0Y0D02*
X5000000Y0D01*
X5000000Y5000000D01*
X0Y5000000D01*
X0Y0D01*
G37*
M02*
"
        } else {
            "\
%FSLAX46Y46*%
%MOMM*%
G36*
X0Y0D02*
X5000000Y0D01*
X5000000Y5000000D01*
X0Y5000000D01*
X0Y0D01*
G37*
M02*
"
        }
    }

    #[test]
    fn clear_polarity_region_is_dropped() {
        // R6: a region drawn under LPC (clear) is a cut-out, not copper; it
        // must not materialize as an additive Region primitive (which would
        // union nets across the gap). A dark region still does.
        let dark = parse_layer(region_layer("dark")).unwrap();
        assert!(
            dark.iter().any(|p| p.kind == PrimKind::Region),
            "a DARK region must materialize as copper"
        );
        let clear = parse_layer(region_layer("clear")).unwrap();
        assert!(
            !clear.iter().any(|p| p.kind == PrimKind::Region),
            "a CLEAR (LPC) region must be dropped, not additive copper"
        );
    }

    #[test]
    fn single_quadrant_g74_arc_uses_correct_center() {
        // R6: a quarter arc from (1,0) to (0,1). Under G74 the I/J offset is an
        // unsigned magnitude (I1 J0); the true centre is the origin (radius 1).
        // The old multi-quadrant formula used centre = start + (I,J) = (2,0),
        // throwing every arc point up to 2 mm off its real position.
        let g = "\
%FSLAX46Y46*%
%MOMM*%
%ADD10C,0.100000*%
G74*
G03*
D10*
X1000000Y0D02*
X0Y1000000I1000000J0D01*
M02*
";
        let prims = parse_layer(g).unwrap();
        let tracks: Vec<_> = prims.iter().filter(|p| p.kind == PrimKind::Track).collect();
        assert!(!tracks.is_empty(), "the arc should produce track segments");
        for p in &tracks {
            if let Shape::Capsule(c) = &p.shape {
                for (x, y) in [(c.ax, c.ay), (c.bx, c.by)] {
                    let radius = (x * x + y * y).sqrt();
                    assert!(
                        (radius - 1.0).abs() < 0.05,
                        "arc point ({x:.3},{y:.3}) is {radius:.3} mm from the origin, \
                         expected ~1 mm (a wrong centre puts it up to 3 mm out)"
                    );
                }
            }
        }
    }

    #[test]
    fn region_arc_segment_is_flattened_not_chorded() {
        // A filled circle drawn as a G36 region of two G03 semicircles (centre
        // at the origin, radius 1 mm). The old region branch recorded only each
        // D01's ENDPOINT, ignoring the circular interpolation mode and the I/J
        // offset, so the contour collapsed to the degenerate chord polygon
        // [(-1,0),(1,0),(-1,0)]: zero area, whole pour vanished from
        // connectivity (false OPEN for anything connecting through it).
        let g = "\
%FSLAX46Y46*%
%MOMM*%
G75*
G36*
X-1000000Y0D02*
G03*
X1000000Y0I1000000J0D01*
X-1000000Y0I-1000000J0D01*
G37*
M02*
";
        let prims = parse_layer(g).unwrap();
        let region = prims
            .iter()
            .find(|p| p.kind == PrimKind::Region)
            .expect("the pour must materialize as a region");
        let Shape::Polygon { pts, .. } = &region.shape else {
            panic!("a single-contour region stays a plain polygon");
        };
        assert!(
            pts.len() > 3,
            "two flattened semicircles carry many vertices, got {} (3 = the chord collapse)",
            pts.len()
        );
        // Every boundary vertex sits on the 1 mm circle (the same centre/sweep
        // math as a stroked arc), and the disc INTERIOR is inside the polygon.
        for &(x, y) in pts {
            let radius = (x * x + y * y).sqrt();
            assert!(
                (radius - 1.0).abs() < 1e-6,
                "contour vertex ({x:.4},{y:.4}) is off the arc circle"
            );
        }
        assert!(
            point_in_polygon(0.0, 0.0, pts),
            "the disc centre must be INSIDE the filled region (a chord polygon has no inside)"
        );
    }

    #[test]
    fn region_disjoint_contours_do_not_bridge() {
        // One G36 region holding TWO disjoint square islands (RS-274X 4.10.4:
        // each contour begins with a D02 move): [0,5]x[0,5] and [95,100]x[0,5],
        // 90 mm apart. The old flat contour vector never split on the second
        // D02, dropped that contour's start vertex, and emitted ONE polygon
        // with a phantom bridge edge, reading two electrically-isolated pads
        // (one per island) onto the same net: a false SHORT.
        let g = "\
%FSLAX46Y46*%
%MOMM*%
G36*
X0Y0D02*
X5000000Y0D01*
X5000000Y5000000D01*
X0Y5000000D01*
X0Y0D01*
X95000000Y0D02*
X100000000Y0D01*
X100000000Y5000000D01*
X95000000Y5000000D01*
X95000000Y0D01*
G37*
M02*
";
        let prims = parse_layer(g).unwrap();
        let regions: Vec<_> = prims
            .iter()
            .filter(|p| p.kind == PrimKind::Region)
            .collect();
        assert_eq!(
            regions.len(),
            2,
            "two disjoint islands are two separate copper pieces, not one bridged polygon"
        );
        // Containment: a point in each square is copper, the 90 mm gap is not.
        let covered = |x: f64, y: f64| {
            regions.iter().any(|p| match &p.shape {
                Shape::Polygon { pts, .. } => point_in_polygon(x, y, pts),
                _ => panic!("hole-less islands stay plain polygons"),
            })
        };
        assert!(covered(2.5, 2.5), "inside the first island");
        assert!(covered(97.5, 2.5), "inside the second island");
        assert!(
            !covered(50.0, 2.5),
            "the gap between the islands is NOT copper"
        );
        // Connectivity: a pad on each island must land on DIFFERENT nets. With
        // the bridged single polygon both pads unioned through the one region
        // primitive onto one net.
        let mut layer: Vec<CopperPrim> = prims.clone();
        layer.push(CopperPrim {
            shape: Shape::disc(2.5, 2.5, 0.5),
            kind: PrimKind::Flash,
        });
        layer.push(CopperPrim {
            shape: Shape::disc(97.5, 2.5, 0.5),
            kind: PrimKind::Flash,
        });
        let (_board, stats) = crate::gerber::connect::reconstruct("t", vec![layer], vec![], vec![]);
        assert_eq!(
            stats.n_nets, 2,
            "one pad per island: two isolated nets, not a false short"
        );
    }

    #[test]
    fn region_hole_contour_is_cut_out_not_filled() {
        // A region with a hole: outer square [0,20]x[0,20], inner square hole
        // [8,12]x[8,12] as a second contour. The ring is copper; the hole
        // interior is NOT. The old flat concatenation bridged the two contours
        // and dropped the hole's start vertex, so the two non-coincident bridge
        // edges enclosed a sliver of RING copper, (0,0)-(12,8)-(8,8), whose
        // parity read OUTSIDE (false open through the ring).
        let g = "\
%FSLAX46Y46*%
%MOMM*%
G36*
X0Y0D02*
X20000000Y0D01*
X20000000Y20000000D01*
X0Y20000000D01*
X0Y0D01*
X8000000Y8000000D02*
X12000000Y8000000D01*
X12000000Y12000000D01*
X8000000Y12000000D01*
X8000000Y8000000D01*
G37*
M02*
";
        let prims = parse_layer(g).unwrap();
        let regions: Vec<_> = prims
            .iter()
            .filter(|p| p.kind == PrimKind::Region)
            .collect();
        assert_eq!(regions.len(), 1, "outer + its hole is ONE piece of copper");
        let Shape::MultiPolygon { contours } = &regions[0].shape else {
            panic!("a region with a hole must carry both contours, not one flat ring");
        };
        assert_eq!(contours.len(), 2, "the outer boundary and its hole");
        use crate::gerber::geo::point_in_contours;
        assert!(
            point_in_contours(2.0, 15.0, contours),
            "a point in the ring is copper"
        );
        assert!(
            point_in_contours(6.0, 4.5, contours),
            "ring copper on the old bridge-edge sliver must still be INSIDE"
        );
        assert!(
            !point_in_contours(10.0, 10.0, contours),
            "the hole interior is NOT copper"
        );
        assert!(
            !point_in_contours(30.0, 30.0, contours),
            "outside the outer boundary is NOT copper"
        );
    }

    #[test]
    fn step_and_repeat_replicates_the_base_cell() {
        // R14: a %SRX2Y1I10J0*% block flashing one pad must produce TWO pads,
        // the base copy at (0,0) and a repeated copy 10 mm along x. The old
        // plotter dropped every StepAndRepeat command, so the repeated copies
        // (all copper/pads but the first) vanished from a panelized layer.
        let g = "\
%FSLAX46Y46*%
%MOMM*%
%ADD10C,0.500000*%
D10*
%SRX2Y1I10.0J0.0*%
X0Y0D03*
%SR*%
M02*
";
        let prims = parse_layer(g).unwrap();
        let flashes: Vec<_> = prims.iter().filter(|p| p.kind == PrimKind::Flash).collect();
        assert_eq!(
            flashes.len(),
            2,
            "SR X2 must emit the base + 1 repeated flash"
        );
        let xs: Vec<f64> = flashes
            .iter()
            .filter_map(|p| match &p.shape {
                Shape::Capsule(c) => Some(c.ax),
                _ => None,
            })
            .collect();
        assert!(
            xs.iter().any(|x| x.abs() < 1e-6),
            "the base flash sits at x≈0, got {xs:?}"
        );
        assert!(
            xs.iter().any(|x| (x - 10.0).abs() < 1e-6),
            "the repeated flash sits 10 mm along x, got {xs:?}"
        );
    }

    #[test]
    fn inch_unit_macro_fallback_disc_is_a_fixed_physical_size() {
        // R14: when an aperture macro can't be instantiated (here a Circle whose
        // diameter references an undefined variable), the flash falls back to a
        // small anchor disc. Its radius is a fixed 0.25 mm, NOT scaled by the
        // document's unit factor. On an inch board (to_mm = 25.4) the old
        // `0.25 * to_mm` bloated it to 6.35 mm, big enough to merge nets.
        let g = "\
%FSLAX46Y46*%
%MOIN*%
%AMBADX*1,1,$1,0,0*%
%ADD10BADX*%
D10*
X0Y0D03*
M02*
";
        let prims = parse_layer(g).unwrap();
        let flash = prims
            .iter()
            .find(|p| p.kind == PrimKind::Flash)
            .expect("a fallback flash");
        let r = match &flash.shape {
            Shape::Capsule(c) => c.r,
            _ => panic!("fallback should be a disc/capsule"),
        };
        assert!(
            (r - MACRO_FALLBACK_DISC_MM).abs() < 1e-9,
            "fallback disc radius must be a fixed {MACRO_FALLBACK_DISC_MM} mm regardless of \
             inch units, got {r} mm (0.25*25.4 = 6.35 was the bug)"
        );
    }

    #[test]
    fn inch_unit_unknown_aperture_stroke_is_a_fixed_hairline() {
        // R40: the fallback stroke width for a non-circle/rect/obround aperture was
        // `0.1 * to_mm`. On an inch board (to_mm = 25.4) that is 2.54 mm, a fat
        // capsule (1.27 mm radius) that union-merges adjacent copper into a false
        // short, not the intended 0.1 mm hairline. The width is a fixed physical
        // mm and must not be unit-scaled (same rule as MACRO_FALLBACK_DISC_MM).
        // A polygon aperture (P) hits the unknown-aperture fallback arm.
        let g = "\
%FSLAX46Y46*%
%MOIN*%
%ADD10P,0.5X4*%
D10*
X0Y0D02*
X0100000Y0D01*
M02*
";
        let prims = parse_layer(g).unwrap();
        let track = prims
            .iter()
            .find(|p| p.kind == PrimKind::Track)
            .expect("a stroked track");
        let r = match &track.shape {
            Shape::Capsule(c) => c.r,
            _ => panic!("a stroke should be a capsule"),
        };
        // width 0.1 mm -> radius 0.05 mm, regardless of inch units.
        assert!(
            (r - 0.05).abs() < 1e-9,
            "inch-unit fallback stroke radius must be a fixed 0.05 mm, got {r} mm (0.1*25.4/2 = 1.27 was the bug)"
        );
    }

    #[test]
    fn single_line_aperture_macro_survives_normalization() {
        // A single-line aperture macro packs its primitives with '*'
        // separators. The block splitter must not treat them as independent
        // extended codes and collapse the macro to an empty def, that shrinks
        // the pad to a fallback disc. AMBOX is a 2×2 CenterLine rectangle.
        let g = "\
%FSLAX46Y46*%
%MOMM*%
%AMBOX*21,1,2,2,0,0,0*%
%ADD10BOX*%
D10*
X0Y0D03*
M02*
";
        let prims = parse_layer(g).unwrap();
        let f = prims
            .iter()
            .find(|p| p.kind == PrimKind::Flash)
            .expect("flash");
        if let Shape::Polygon { pts, .. } = &f.shape {
            let (minx, maxx) = pts
                .iter()
                .fold((f64::MAX, f64::MIN), |(a, b), p| (a.min(p.0), b.max(p.0)));
            assert!(
                (maxx - minx - 2.0).abs() < 1e-3,
                "macro rect width {} (expected ~2 mm; the macro was destroyed if it collapsed)",
                maxx - minx
            );
        } else {
            panic!("macro flash must instantiate a polygon, not a fallback disc");
        }
    }
}
