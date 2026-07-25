//! Production autorouting via **freerouting** over Specctra DSN/SES.
//!
//! Routing a real board is a hard, well-solved problem; the right move is to
//! hand the placed board to [freerouting](https://github.com/freerouting/freerouting)
//! (the standard open-source autorouter the KiCad ecosystem already uses) rather
//! than grow the in-tree grid A* into a half-baked router. This module is that
//! hand-off:
//!
//! ```text
//! Pcb (placed) ──write_dsn──▶ board.dsn
//!                                │  java -jar freerouting -de board.dsn -do board.ses
//!                                ▼
//!                           board.ses ──parse_ses──▶ tracks + vias
//!                                │  merge_ses_into_pcb
//!                                ▼
//!                           Pcb (routed) ──▶ .kicad_pcb
//! ```
//!
//! The DSN writer emits a self-contained design: the board boundary (from the
//! `Edge.Cuts` outline, or the pad bounding box if absent), one image per
//! footprint with its pads as pins, a padstack per distinct pad/via geometry,
//! the net list, and a default width/clearance rule. The SES reader pulls the
//! routed `wire`/`via` records back and [`merge_ses_into_pcb`] writes them onto
//! the board as copper segments and vias on the correct nets.
//!
//! The grid A* router in [`crate::layout`] stays as a documented fallback for
//! when freerouting (or a JRE) is not installed; the engine prefers freerouting
//! when it is present.

use crate::dsl::Outline;
use forge_model::{Pcb, PadKind};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Routing error: a self-contained string-backed error so this crate stays
/// dependency-light (no anyhow).
#[derive(Debug, Clone)]
pub struct RouteError(pub String);

impl std::fmt::Display for RouteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for RouteError {}

impl From<std::io::Error> for RouteError {
    fn from(e: std::io::Error) -> Self {
        RouteError(e.to_string())
    }
}

type Result<T> = std::result::Result<T, RouteError>;

macro_rules! rbail {
    ($($a:tt)*) => { return Err(RouteError(format!($($a)*))) };
}

/// SES/DSN resolution: 1 unit = 0.1 micron (`(resolution um 10)`). So
/// `mm * UM_PER_MM * 10` = units, and `units / 10000.0` = mm.
const RES_UNITS_PER_MM: f64 = 10_000.0;

fn to_units(mm: f64) -> i64 {
    (mm * RES_UNITS_PER_MM).round() as i64
}

/// Convert SES units to mm given the SES's units-per-mm scale. freerouting
/// 1.9.0 writes its SES output at 10x the declared input resolution (a known
/// quirk), so the scale is detected empirically rather than assumed; see
/// [`detect_ses_scale`].
fn ses_to_mm(units: f64, units_per_mm: f64) -> f64 {
    units / units_per_mm
}

/// Default copper width and clearance (mm) for the routing rule when the board
/// carries none. Conservative 2-layer defaults.
#[derive(Debug, Clone, Copy)]
pub struct RouteRules {
    pub track_width_mm: f64,
    pub clearance_mm: f64,
    pub via_diameter_mm: f64,
    pub via_drill_mm: f64,
}

impl Default for RouteRules {
    fn default() -> Self {
        RouteRules {
            track_width_mm: 0.25,
            clearance_mm: 0.2,
            via_diameter_mm: 0.6,
            via_drill_mm: 0.3,
        }
    }
}

// ---------------------------------------------------------------------------
// DSN export
// ---------------------------------------------------------------------------

/// A padstack definition (a named pad geometry).
#[derive(PartialEq, Eq, PartialOrd, Ord, Clone)]
struct PadStack {
    name: String,
    /// Body: either a circle of this diameter, or a rect half-extents.
    body: PadBody,
    through: bool,
}

#[derive(PartialEq, Eq, PartialOrd, Ord, Clone)]
enum PadBody {
    /// Circle diameter in units.
    Circle(i64),
    /// Rectangle half-width / half-height in units.
    Rect(i64, i64),
    /// Stadium/obround pad: a Specctra `path` of one straight span stroked with
    /// a round aperture. `aperture` is the pad's MINOR dimension (the stroke
    /// diameter) and the two path endpoints sit `half_span` either side of the
    /// pad centre along the major axis, so the swept outline is exactly the
    /// KiCad oval of `major x minor`. `horizontal` picks the axis.
    Capsule {
        aperture: i64,
        half_span: i64,
        horizontal: bool,
    },
}

/// Derive a padstack from a pad's shape + size, in units, returning a stable
/// name so identical pads share one padstack definition.
///
/// `size_mm` is the pad's extent in the frame the padstack is written in, so a
/// dimension-swapped (90/270) pad arrives here already swapped and gets its own
/// distinct padstack.
fn padstack_for(shape: &str, size_mm: (f64, f64), through: bool) -> PadStack {
    let layer_tag = if through { "A" } else { "T" }; // All-layer vs Top
    let (w, h) = (size_mm.0.max(0.1), size_mm.1.max(0.1));
    match shape {
        "circle" | "oval" if (w - h).abs() < 1e-6 => {
            let d = to_units(w);
            PadStack {
                name: format!("Round[{layer_tag}]Pad_{d}"),
                body: PadBody::Circle(d),
                through,
            }
        }
        // An ELONGATED oval is a stadium, not a rectangle. Falling through to
        // Rect here (the old behaviour) hands freerouting a pad with square
        // corners it must keep clear of, shrinking the usable channel beside
        // every SOIC/resistor pad and, on the audit side, claiming copper is
        // inside a pad when it sits in a corner the real pad never reaches.
        "oval" => {
            let horizontal = w >= h;
            let (major, minor) = if horizontal { (w, h) } else { (h, w) };
            let aperture = to_units(minor);
            let half_span = to_units((major - minor) * 0.5);
            let (uw, uh) = (to_units(w), to_units(h));
            PadStack {
                name: format!("Oval[{layer_tag}]Pad_{uw}x{uh}"),
                body: PadBody::Capsule {
                    aperture,
                    half_span,
                    horizontal,
                },
                through,
            }
        }
        _ => {
            let hx = to_units(w * 0.5);
            let hy = to_units(h * 0.5);
            PadStack {
                name: format!("Rect[{layer_tag}]Pad_{hx}x{hy}"),
                body: PadBody::Rect(hx, hy),
                through,
            }
        }
    }
}

/// Compute the board boundary rectangle in mm: the `Edge.Cuts` bbox if present,
/// else the supplied DSL outline, else the pad bounding box plus a margin.
fn board_boundary(pcb: &Pcb, outline: Option<Outline>) -> (f64, f64, f64, f64) {
    // 1. Edge.Cuts geometry.
    let mut min = (f64::MAX, f64::MAX);
    let mut max = (f64::MIN, f64::MIN);
    let mut any = false;
    for gl in pcb.gr_lines() {
        if gl.layer() == "Edge.Cuts" {
            for (x, y) in [gl.start(), gl.end()] {
                min.0 = min.0.min(x);
                min.1 = min.1.min(y);
                max.0 = max.0.max(x);
                max.1 = max.1.max(y);
                any = true;
            }
        }
    }
    if any && max.0 > min.0 {
        return (min.0, min.1, max.0, max.1);
    }
    // 2. DSL outline.
    if let Some(o) = outline {
        return (o.min_x, o.min_y, o.max_x, o.max_y);
    }
    // 3. Pad bounding box + margin.
    let mut min = (f64::MAX, f64::MAX);
    let mut max = (f64::MIN, f64::MIN);
    for fp in pcb.footprints() {
        for pad in fp.pads() {
            let (x, y) = pad.absolute_pos(&fp);
            min.0 = min.0.min(x);
            min.1 = min.1.min(y);
            max.0 = max.0.max(x);
            max.1 = max.1.max(y);
        }
    }
    if min.0 > max.0 {
        return (0.0, 0.0, 10.0, 10.0);
    }
    let m = 5.0;
    (min.0 - m, min.1 - m, max.0 + m, max.1 + m)
}

/// Serialise a placed [`Pcb`] to Specctra DSN text for freerouting.
pub fn write_dsn(pcb: &Pcb, outline: Option<Outline>, rules: &RouteRules) -> String {
    let (bx0, by0, bx1, by1) = board_boundary(pcb, outline);

    // Collect padstacks and per-footprint images/pads.
    let mut padstacks: BTreeMap<String, PadStack> = BTreeMap::new();
    // image name -> Vec<(pin name, image-local position, padstack, pin rotate)>
    let mut images: Vec<(String, Vec<(String, (f64, f64), String, f64)>)> = Vec::new();
    // (image name, reference, place x, y, rot, side)
    let mut placements: Vec<(String, String, (f64, f64), f64, String)> = Vec::new();
    // net name -> Vec<"ref-pin">
    let mut nets: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for (idx, fp) in pcb.footprints().iter().enumerate() {
        let reference = fp.reference().unwrap_or_else(|| format!("FP{idx}"));
        // Key every image AND placed component by the footprint *index*, not the
        // reference designator: some corpus boards carry duplicate references
        // (two parts both called "C2"), which would otherwise collapse into one
        // component placed twice and corrupt the net topology. The index is
        // always unique, so each physical footprint is its own DSN component.
        let inst = format!("C{idx}");
        let image_name = format!("img_{idx}");
        let (fx, fy, frot) = fp.at();
        let back = fp.layer().starts_with("B.");
        let side = if back { "back" } else { "front" };

        let mut local_pins: Vec<(String, (f64, f64), String, f64)> = Vec::new();
        let mut pin_seq = 0usize;
        for pad in fp.pads() {
            pin_seq += 1;
            // Pad numbers may repeat within a footprint (e.g. a multi-contact
            // shield pad "S1"), so the DSN pin name must be unique per physical
            // pad. Use the pad sequence index only, with an ANSI-safe name
            // (freerouting's tokenizer rejects `#` and other punctuation, which
            // silently corrupts the net list). The board-side number is not
            // needed by the router; the net wiring is what matters.
            let num = format!("p{pin_seq}");
            let through = matches!(pad.kind(), PadKind::ThruHole | PadKind::NpThruHole);
            // np_thru_hole pads (mounting holes) carry no net: skip as pins.
            if matches!(pad.kind(), PadKind::NpThruHole) {
                continue;
            }
            // Pad position relative to the footprint origin, un-rotated (DSN
            // applies the component rotation itself at placement time). Back
            // images are pre-flipped to a TOP view (x negated), because
            // freerouting mirrors every back image about the y axis on import;
            // the double flip restores the file-frame offsets. KiCad's own
            // exporter does the same (FlipFOOTPRINTs before writing images).
            let (lx, ly, prot) = pad.at();
            let ex = if back { -lx } else { lx };
            // The padstack is emitted in the pad's own frame, NEVER dimension-
            // swapped: freerouting rotates the padstack shape by the placement
            // angle itself (verified empirically on 1.9.0 and 2.2.4, see
            // docs/record/FREEROUTING_DSN_SEMANTICS.md), so a swapped padstack
            // would double-rotate. The pad's own rotation rides on a per-pin
            // `(rotate A)` instead, exactly as KiCad's DSN exporter does; this
            // also handles non-quarter-turn pad angles.
            //
            // Angle algebra, in this writer's mirrored (KiCad y-down) frame
            // where every emitted angle is the negated KiCad angle: the pad's
            // serialised `at` angle is ABSOLUTE (KiCad folds the footprint
            // rotation in when writing the file), the shape must come out at
            // -prot after freerouting composes place and pin rotation, and the
            // place angle is -frot. Front composes R(place)*R(pin), so the pin
            // rotate is frot - prot. Back composes R(place)*Mirror*R(pin)
            // (mirror BEFORE pin rotation is undone by it acting on the bare
            // padstack), which flips the pin term's sign: prot - frot.
            let rel = pad_image_angle_deg(frot, prot);
            let pin_rot = norm_deg(if back { rel } else { -rel });
            let ps = padstack_for(&pad.shape(), pad.size(), through);
            padstacks.entry(ps.name.clone()).or_insert_with(|| ps.clone());
            local_pins.push((num.clone(), (ex, ly), ps.name.clone(), pin_rot));

            if let Some((_, net)) = pad.net() {
                if !net.is_empty() {
                    nets.entry(net).or_default().push(format!("{inst}-{num}"));
                }
            }
        }
        let _ = reference;
        if local_pins.is_empty() {
            continue;
        }
        images.push((image_name.clone(), local_pins));
        placements.push((image_name, inst, (fx, fy), frot, side.to_string()));
    }

    // Via padstack.
    let via_name = format!("Via[0-1]_{}", to_units(rules.via_diameter_mm));
    padstacks.insert(
        via_name.clone(),
        PadStack {
            name: via_name.clone(),
            body: PadBody::Circle(to_units(rules.via_diameter_mm)),
            through: true,
        },
    );

    // ---- render ----
    let mut s = String::new();
    let _ = writeln!(s, "(pcb hauksbee.dsn");
    let _ = writeln!(s, "  (parser");
    let _ = writeln!(s, "    (string_quote \")");
    let _ = writeln!(s, "    (space_in_quoted_tokens on)");
    let _ = writeln!(s, "    (host_cad \"hauksbee\")");
    let _ = writeln!(s, "    (host_version \"1\")");
    let _ = writeln!(s, "  )");
    let _ = writeln!(s, "  (resolution um 10)");
    let _ = writeln!(s, "  (unit um)");
    // structure
    let _ = writeln!(s, "  (structure");
    let _ = writeln!(s, "    (layer F.Cu (type signal) (property (index 0)))");
    let _ = writeln!(s, "    (layer B.Cu (type signal) (property (index 1)))");
    let _ = writeln!(
        s,
        "    (boundary (rect pcb {} {} {} {}))",
        to_units(bx0),
        to_units(by0),
        to_units(bx1),
        to_units(by1)
    );
    let _ = writeln!(s, "    (via \"{via_name}\")");
    let _ = writeln!(
        s,
        "    (rule (width {}) (clearance {}))",
        to_units(rules.track_width_mm),
        to_units(rules.clearance_mm)
    );
    let _ = writeln!(s, "  )");
    // placement
    let _ = writeln!(s, "  (placement");
    // Component/image/pin names are ANSI-safe identifiers (no spaces or
    // punctuation), so they are emitted *unquoted*: freerouting's `(pins ...)`
    // reader mishandles quoted pin tokens and silently truncates the net list.
    // Net and padstack names can contain `/`, `+`, `(`, `)` and so stay quoted.
    for (image, reference, at, rot, side) in &placements {
        let _ = writeln!(s, "    (component {image}");
        let _ = writeln!(
            s,
            "      (place {} {} {} {} {})",
            reference,
            to_units(at.0),
            to_units(at.1),
            side,
            specctra_place_angle(*rot)
        );
        let _ = writeln!(s, "    )");
    }
    let _ = writeln!(s, "  )");
    // library
    let _ = writeln!(s, "  (library");
    for (image, pins) in &images {
        let _ = writeln!(s, "    (image {image}");
        for (pin, at, ps, rot) in pins {
            // Per-pin rotation carries the pad's own angle (relative to the
            // image), exactly as KiCad's exporter emits it. Zero is omitted so
            // the common case stays byte-stable.
            if *rot == 0.0 {
                let _ = writeln!(
                    s,
                    "      (pin \"{}\" {} {} {})",
                    ps,
                    pin,
                    to_units(at.0),
                    to_units(at.1)
                );
            } else {
                let _ = writeln!(
                    s,
                    "      (pin \"{}\" (rotate {}) {} {} {})",
                    ps,
                    fmt_angle(*rot),
                    pin,
                    to_units(at.0),
                    to_units(at.1)
                );
            }
        }
        let _ = writeln!(s, "    )");
    }
    for ps in padstacks.values() {
        emit_padstack(&mut s, ps);
    }
    let _ = writeln!(s, "  )");
    // network
    let _ = writeln!(s, "  (network");
    for (net, pins) in &nets {
        if pins.len() < 2 {
            continue; // single-pad nets need no routing
        }
        let _ = writeln!(s, "    (net \"{net}\"");
        let _ = write!(s, "      (pins");
        for p in pins {
            let _ = write!(s, " {p}");
        }
        let _ = writeln!(s, ")");
        let _ = writeln!(s, "    )");
    }
    // A class binding every net to the default rule.
    let _ = write!(s, "    (class default");
    for net in nets.keys() {
        if nets[net].len() >= 2 {
            let _ = write!(s, " \"{net}\"");
        }
    }
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "      (rule (width {}) (clearance {}))",
        to_units(rules.track_width_mm),
        to_units(rules.clearance_mm)
    );
    let _ = writeln!(s, "    )");
    let _ = writeln!(s, "  )");
    let _ = writeln!(s, "  (wiring");
    let _ = writeln!(s, "  )");
    let _ = writeln!(s, ")");
    s
}

fn emit_padstack(s: &mut String, ps: &PadStack) {
    let _ = writeln!(s, "    (padstack \"{}\"", ps.name);
    let layers: &[&str] = if ps.through {
        &["F.Cu", "B.Cu"]
    } else {
        &["F.Cu"]
    };
    for l in layers {
        match ps.body {
            PadBody::Circle(d) => {
                let _ = writeln!(s, "      (shape (circle {l} {d}))");
            }
            PadBody::Rect(hx, hy) => {
                let _ = writeln!(
                    s,
                    "      (shape (rect {l} {} {} {} {}))",
                    -hx, -hy, hx, hy
                );
            }
            PadBody::Capsule {
                aperture,
                half_span,
                horizontal,
            } => {
                let (x0, y0, x1, y1) = if horizontal {
                    (-half_span, 0, half_span, 0)
                } else {
                    (0, -half_span, 0, half_span)
                };
                let _ = writeln!(
                    s,
                    "      (shape (path {l} {aperture} {x0} {y0} {x1} {y1}))"
                );
            }
        }
    }
    let _ = writeln!(s, "      (attach off)");
    let _ = writeln!(s, "    )");
}

/// Freerouting wants rotation in degrees, normalised to [0, 360).
///
/// Round FIRST, then take the modulus. Normalising before rounding lets a value
/// just under a full turn (359.6) survive the modulus untouched and then round
/// up to a literal `360`, which is outside the range this promises.
fn norm_rot(r: f64) -> i64 {
    (r.round() as i64).rem_euclid(360)
}

/// Normalise an angle into [0, 360) WITHOUT rounding, for per-pin rotations
/// that may legitimately be non-integral (a 22.5 degree pad stays 22.5).
fn norm_deg(r: f64) -> f64 {
    let v = r.rem_euclid(360.0);
    if (v - 360.0).abs() < 1e-9 { 0.0 } else { v }
}

/// Render an angle for the DSN: integral values print bare (`90`), anything
/// else keeps its fraction (`22.5`).
fn fmt_angle(a: f64) -> String {
    if (a - a.round()).abs() < 1e-9 {
        format!("{}", a.round() as i64)
    } else {
        format!("{a}")
    }
}

/// The angle to emit in a Specctra `(place ...)` record for a footprint at KiCad
/// rotation `kicad_rot`.
///
/// Specctra `front`/`back` placement rotates the OPPOSITE way to KiCad, so the
/// exported angle is `(360 - a) mod 360`. Emitting the raw KiCad angle mirrors
/// every rotated footprint in the router's view: on a symmetric 2-pad part pads
/// 1/2 swap, so the router wires the wrong terminal of every rotated decoupler.
/// Verified against KiCad's own DSN exporter: a footprint KiCad places at rot 90
/// must be written `front 270` for its pins to land where KiCad puts them.
fn specctra_place_angle(kicad_rot: f64) -> i64 {
    norm_rot(360.0 - kicad_rot)
}

// ---------------------------------------------------------------------------
// freerouting invocation
// ---------------------------------------------------------------------------

/// How to find and run freerouting.
#[derive(Debug, Clone)]
pub struct FreeroutingConfig {
    /// Path to the freerouting `.jar` (overrides the env/search).
    pub jar: Option<PathBuf>,
    /// `java` binary (default: `java` on PATH).
    pub java: PathBuf,
    /// Maximum auto-router passes (`-mp`). Fewer = faster, less optimal.
    pub max_passes: u32,
    /// Hard wall-clock budget; the process is killed if it overruns.
    pub timeout: Duration,
}

impl Default for FreeroutingConfig {
    fn default() -> Self {
        FreeroutingConfig {
            jar: None,
            java: PathBuf::from("java"),
            max_passes: 10,
            timeout: Duration::from_secs(180),
        }
    }
}

/// Locate the freerouting jar: explicit config, then `FREEROUTING_JAR`, then a
/// few conventional locations relative to the workspace and home.
pub fn find_freerouting_jar(cfg: &FreeroutingConfig) -> Option<PathBuf> {
    if let Some(j) = &cfg.jar {
        if j.exists() {
            return Some(j.clone());
        }
    }
    if let Ok(j) = std::env::var("FREEROUTING_JAR") {
        let p = PathBuf::from(j);
        if p.exists() {
            return Some(p);
        }
    }
    // Common spots: a `tools/` dir up the tree, or ~/.local/share.
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        let mut dir = Some(cwd.as_path());
        while let Some(d) = dir {
            candidates.push(d.join("tools"));
            dir = d.parent();
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        candidates.push(PathBuf::from(&home).join(".local/share/freerouting"));
        candidates.push(PathBuf::from(home).join("freerouting"));
    }
    // Among found jars, prefer a 1.x release: freerouting 1.9.0's headless
    // batch mode reliably writes the SES and exits even on a partially-routed
    // board, whereas 2.2.x stalls without writing output unless the board is
    // 100% routed. So 1.9.0 is the dependable production jar here.
    let mut found: Vec<PathBuf> = Vec::new();
    for c in candidates {
        if let Ok(rd) = std::fs::read_dir(&c) {
            for e in rd.flatten() {
                let p = e.path();
                if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                    if name.starts_with("freerouting") && name.ends_with(".jar") {
                        found.push(p);
                    }
                }
            }
        }
    }
    if found.is_empty() {
        return None;
    }
    found.sort_by_key(|p| {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        // Rank: 1.x jars first (0), then everything else (1), then by name.
        let major_one = name.contains("-1.");
        (if major_one { 0 } else { 1 }, name.to_string())
    });
    found.into_iter().next()
}

/// Whether freerouting is usable on this machine (a jar and a JRE exist).
pub fn freerouting_available(cfg: &FreeroutingConfig) -> bool {
    if find_freerouting_jar(cfg).is_none() {
        return false;
    }
    Command::new(&cfg.java)
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Outcome of a freerouting run.
#[derive(Debug, Clone)]
pub struct FreeroutingRun {
    /// Path to the produced SES file.
    pub ses_path: PathBuf,
    /// Wall-clock seconds the router took.
    pub elapsed_secs: f64,
}

/// Run freerouting headlessly on a DSN file, writing the SES beside it. Runs as
/// a child process, polled on an interval, and **killed if it exceeds the
/// configured timeout** (autorouting a large board can otherwise run away).
pub fn run_freerouting(
    dsn_path: &Path,
    ses_path: &Path,
    cfg: &FreeroutingConfig,
) -> Result<FreeroutingRun> {
    let jar = find_freerouting_jar(cfg)
        .ok_or_else(|| RouteError("freerouting jar not found (set FREEROUTING_JAR)".into()))?;

    let start = Instant::now();
    // Version-specific flags: freerouting 2.x understands `-da` (disable the
    // analytics/telemetry call, which otherwise blocks the process); 1.x does
    // not accept it and would error. Detect from the jar file name.
    let jar_name = jar.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let is_v2 = jar_name.contains("-2.");

    let mut cmd = Command::new(&cfg.java);
    cmd.arg("-jar")
        .arg(&jar)
        .arg("-de")
        .arg(dsn_path)
        .arg("-do")
        .arg(ses_path)
        .arg("-mp")
        .arg(cfg.max_passes.to_string());
    if is_v2 {
        cmd.arg("-da");
    }
    // headless + no GUI; freerouting goes to CLI mode when -de/-do given.
    let mut child: Child = cmd
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| RouteError(format!("spawning freerouting: {e}")))?;

    // Poll until the child exits or the deadline passes.
    loop {
        match child.try_wait()? {
            Some(status) => {
                if !status.success() {
                    rbail!("freerouting exited with {status}");
                }
                break;
            }
            None => {
                if start.elapsed() > cfg.timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    rbail!(
                        "freerouting exceeded {}s timeout; killed",
                        cfg.timeout.as_secs()
                    );
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        }
    }

    if !ses_path.exists() {
        rbail!("freerouting finished but produced no SES at {}", ses_path.display());
    }
    Ok(FreeroutingRun {
        ses_path: ses_path.to_path_buf(),
        elapsed_secs: start.elapsed().as_secs_f64(),
    })
}

// ---------------------------------------------------------------------------
// SES import
// ---------------------------------------------------------------------------

/// A routed wire from the SES file.
#[derive(Debug, Clone)]
pub struct SesWire {
    pub net: String,
    pub layer: String,
    pub width_mm: f64,
    /// Polyline points in mm.
    pub points: Vec<(f64, f64)>,
}

/// A routed via from the SES file.
#[derive(Debug, Clone)]
pub struct SesVia {
    pub net: String,
    pub at: (f64, f64),
}

/// Parsed routing result.
#[derive(Debug, Clone, Default)]
pub struct SesRoutes {
    pub wires: Vec<SesWire>,
    pub vias: Vec<SesVia>,
}

/// Detect the SES coordinate scale (units per mm) by comparing a placed
/// component's echoed SES position against the board's true footprint position.
///
/// freerouting 1.9.0 writes the SES at 10x the declared input resolution, so a
/// hard-coded scale would misplace every track by 10x. We instead anchor the
/// scale to ground truth: find the first `(place CN x y ...)` in the SES whose
/// component id `CN` maps to footprint index N, and divide the SES coordinate by
/// the footprint's known mm position. Falls back to the declared resolution if
/// no anchor is found.
fn detect_ses_scale(text: &str, pcb: &Pcb) -> f64 {
    let footprints = pcb.footprints();
    // Map "C<idx>" -> footprint mm position.
    let toks = lex(text);
    let mut i = 0;
    while i + 4 < toks.len() {
        if let (Some(Tok::Open), Some(Tok::Sym(h)), Some(Tok::Sym(comp))) =
            (toks.get(i), toks.get(i + 1), toks.get(i + 2))
        {
            if h == "place" {
                if let Some(idx) = comp.strip_prefix('C').and_then(|s| s.parse::<usize>().ok()) {
                    if let (Some(Tok::Num(sx)), Some(fp)) = (toks.get(i + 3), footprints.get(idx)) {
                        let (fx, _, _) = fp.at();
                        if fx.abs() > 1.0 && sx.abs() > 1.0 {
                            // units_per_mm = ses_units / mm
                            let scale = sx.abs() / fx.abs();
                            // Snap to the nearest power-of-ten-ish sane scale.
                            return scale;
                        }
                    }
                }
            }
        }
        i += 1;
    }
    // Fallback: declared `um 10` => 10000 units/mm.
    RES_UNITS_PER_MM
}

/// Parse a freerouting SES file into wires and vias (a small, tolerant
/// s-expression scan; SES is a strict subset we only read a few records from).
/// `units_per_mm` is the SES coordinate scale (see [`detect_ses_scale`]).
pub fn parse_ses(text: &str, units_per_mm: f64) -> SesRoutes {
    let toks = lex(text);
    let mut routes = SesRoutes::default();
    // Walk the token stream tracking the current `(net "name" ...)` context.
    let mut i = 0;
    let mut net_stack: Vec<String> = Vec::new();
    let mut depth_of_net: Vec<usize> = Vec::new();
    let mut depth = 0usize;

    while i < toks.len() {
        match &toks[i] {
            Tok::Open => {
                depth += 1;
                // Look at the head symbol.
                if let Some(Tok::Sym(head)) = toks.get(i + 1) {
                    match head.as_str() {
                        "net" => {
                            if let Some(Tok::Sym(name)) | Some(Tok::Str(name)) = toks.get(i + 2) {
                                net_stack.push(name.clone());
                                depth_of_net.push(depth);
                            }
                        }
                        "wire" => {
                            if let Some(w) = parse_wire(&toks, i, net_stack.last(), units_per_mm) {
                                routes.wires.push(w);
                            }
                        }
                        "via" => {
                            if let Some(v) = parse_via(&toks, i, net_stack.last(), units_per_mm) {
                                routes.vias.push(v);
                            }
                        }
                        _ => {}
                    }
                }
                i += 1;
            }
            Tok::Close => {
                if let Some(&d) = depth_of_net.last() {
                    if d == depth {
                        net_stack.pop();
                        depth_of_net.pop();
                    }
                }
                depth = depth.saturating_sub(1);
                i += 1;
            }
            _ => i += 1,
        }
    }
    routes
}

/// Parse a `(wire (path LAYER WIDTH x1 y1 x2 y2 ...) ...)` starting at the
/// `(` token index `open`.
fn parse_wire(
    toks: &[Tok],
    open: usize,
    net: Option<&String>,
    units_per_mm: f64,
) -> Option<SesWire> {
    // Find the inner `(path ...)`.
    let mut i = open + 2;
    while i < toks.len() {
        if matches!(toks.get(i), Some(Tok::Open)) {
            if let Some(Tok::Sym(h)) = toks.get(i + 1) {
                if h == "path" || h == "polyline_path" {
                    let layer = match toks.get(i + 2) {
                        Some(Tok::Sym(l)) | Some(Tok::Str(l)) => l.clone(),
                        _ => return None,
                    };
                    let width = match toks.get(i + 3) {
                        Some(Tok::Num(w)) => ses_to_mm(*w, units_per_mm),
                        _ => return None,
                    };
                    let mut pts = Vec::new();
                    let mut j = i + 4;
                    let mut coords: Vec<f64> = Vec::new();
                    while let Some(Tok::Num(n)) = toks.get(j) {
                        coords.push(*n);
                        j += 1;
                    }
                    let mut k = 0;
                    while k + 1 < coords.len() {
                        pts.push((
                            ses_to_mm(coords[k], units_per_mm),
                            ses_to_mm(coords[k + 1], units_per_mm),
                        ));
                        k += 2;
                    }
                    if pts.len() < 2 {
                        return None;
                    }
                    return Some(SesWire {
                        net: net.cloned().unwrap_or_default(),
                        layer,
                        width_mm: width,
                        points: pts,
                    });
                }
            }
        }
        // Stop at the close of the wire.
        if matches!(toks.get(i), Some(Tok::Close)) {
            break;
        }
        i += 1;
    }
    None
}

/// Parse a `(via PADSTACK x y ...)` record.
fn parse_via(
    toks: &[Tok],
    open: usize,
    net: Option<&String>,
    units_per_mm: f64,
) -> Option<SesVia> {
    // (via "padstack" x y [net?])
    // skip head (open+1) and padstack name (open+2), read two numbers.
    let mut nums: Vec<f64> = Vec::new();
    let mut i = open + 2;
    while i < toks.len() && nums.len() < 2 {
        match toks.get(i) {
            Some(Tok::Num(n)) => nums.push(*n),
            Some(Tok::Close) => break,
            _ => {}
        }
        i += 1;
    }
    if nums.len() == 2 {
        Some(SesVia {
            net: net.cloned().unwrap_or_default(),
            at: (ses_to_mm(nums[0], units_per_mm), ses_to_mm(nums[1], units_per_mm)),
        })
    } else {
        None
    }
}

/// Merge parsed SES routes onto the board as copper segments and vias, mapping
/// net names to the board's numeric net ids. Returns the number of wire
/// segments and vias added.
pub fn merge_ses_into_pcb(
    pcb: &mut Pcb,
    routes: &SesRoutes,
    rules: &RouteRules,
) -> (usize, usize) {
    // net name -> id
    let mut net_id = std::collections::HashMap::new();
    for n in pcb.nets() {
        net_id.insert(n.name.clone(), n.id);
    }

    let mut seg_count = 0;
    for w in &routes.wires {
        let id = net_id.get(&w.net).copied();
        let width = if w.width_mm > 0.0 {
            w.width_mm
        } else {
            rules.track_width_mm
        };
        for pair in w.points.windows(2) {
            pcb.add_segment(pair[0], pair[1], width, &w.layer, id);
            seg_count += 1;
        }
    }
    let mut via_count = 0;
    for v in &routes.vias {
        let id = net_id.get(&v.net).copied();
        pcb.add_via(
            v.at,
            rules.via_diameter_mm,
            rules.via_drill_mm,
            &["F.Cu", "B.Cu"],
            id,
        );
        via_count += 1;
    }
    (seg_count, via_count)
}

// ---------------------------------------------------------------------------
// Tiny s-expression lexer (read-only, for SES)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Tok {
    Open,
    Close,
    Sym(String),
    Str(String),
    Num(f64),
}

fn lex(text: &str) -> Vec<Tok> {
    let mut out = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(&c) = chars.peek() {
        match c {
            '(' => {
                out.push(Tok::Open);
                chars.next();
            }
            ')' => {
                out.push(Tok::Close);
                chars.next();
            }
            c if c.is_whitespace() => {
                chars.next();
            }
            '"' => {
                chars.next();
                let mut s = String::new();
                while let Some(&c) = chars.peek() {
                    if c == '"' {
                        chars.next();
                        break;
                    }
                    s.push(c);
                    chars.next();
                }
                out.push(Tok::Str(s));
            }
            _ => {
                let mut s = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_whitespace() || c == '(' || c == ')' || c == '"' {
                        break;
                    }
                    s.push(c);
                    chars.next();
                }
                // Numeric?
                if let Ok(n) = s.parse::<f64>() {
                    out.push(Tok::Num(n));
                } else {
                    out.push(Tok::Sym(s));
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// High-level driver
// ---------------------------------------------------------------------------

/// Result of a full route.
#[derive(Debug, Clone)]
pub struct RouteOutcome {
    /// Nets that had >=2 pads and so needed routing.
    pub nets_to_route: usize,
    /// Nets with at least one routed wire after the run.
    pub nets_routed: usize,
    pub segments: usize,
    pub vias: usize,
    pub elapsed_secs: f64,
}

/// Route a placed board end to end with freerouting: write DSN, run the router
/// (background + polled + timed out), parse the SES, and merge the result back
/// onto `pcb`. The intermediate `.dsn`/`.ses` are written into `workdir`.
pub fn route_with_freerouting(
    pcb: &mut Pcb,
    outline: Option<Outline>,
    rules: &RouteRules,
    cfg: &FreeroutingConfig,
    workdir: &Path,
) -> Result<RouteOutcome> {
    std::fs::create_dir_all(workdir)?;
    let dsn = workdir.join("board.dsn");
    let ses = workdir.join("board.ses");
    let dsn_text = write_dsn(pcb, outline, rules);
    std::fs::write(&dsn, &dsn_text)?;

    // Count nets that need routing (>=2 connected pads) for the report.
    let mut net_pads: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for fp in pcb.footprints() {
        for pad in fp.pads() {
            if let Some((_, n)) = pad.net() {
                if !n.is_empty() {
                    *net_pads.entry(n).or_default() += 1;
                }
            }
        }
    }
    let nets_to_route = net_pads.values().filter(|&&c| c >= 2).count();

    let run = run_freerouting(&dsn, &ses, cfg)?;
    let ses_text = std::fs::read_to_string(&ses)?;
    let scale = detect_ses_scale(&ses_text, pcb);
    let routes = parse_ses(&ses_text, scale);

    let routed_nets: std::collections::HashSet<&str> =
        routes.wires.iter().map(|w| w.net.as_str()).collect();
    let nets_routed = routed_nets.iter().filter(|n| !n.is_empty()).count();

    let (segments, vias) = merge_ses_into_pcb(pcb, &routes, rules);

    Ok(RouteOutcome {
        nets_to_route,
        nets_routed,
        segments,
        vias,
        elapsed_secs: run.elapsed_secs,
    })
}

// ---------------------------------------------------------------------------
// Post-merge correctness checks (honest connectivity + endpoint-net assertion)
// ---------------------------------------------------------------------------
//
// These run on the MERGED board (after freerouting or the grid fallback wrote
// copper back onto it), so they judge the real result rather than a router's
// self-report. They need only `forge_model`, so they are testable here without
// a JRE and reusable by the engine's routing report.

/// Two board points are the "same" copper node when within this many mm. Merged
/// polyline segments share exact vertices; a via and the tracks meeting it snap
/// to the same point, so a small epsilon suffices.
const COINCIDE_EPS_MM: f64 = 0.05;
/// Slack (mm) when testing whether a copper point sits inside a pad.
const PAD_EPS_MM: f64 = 1e-3;
/// Cell size (mm) of the uniform spatial hash the audit buckets copper nodes
/// into. Big enough that a typical pad or track junction touches a handful of
/// cells, small enough that a dense ground net does not collapse into one.
const GRID_CELL_MM: f64 = 2.0;

/// The board's copper layers, indexed, so a copper item's layer membership can
/// be carried as a bitmask instead of a string compared in an inner loop.
///
/// Layer identity is what makes the audit honest: an `F.Cu` track ending where a
/// `B.Cu` track ends is NOT a connection unless a via joins them, and an `F.Cu`
/// track ending over a `B.Cu`-only pad is NOT touching that pad.
struct LayerIndex {
    /// Copper layer names in board order, capped at the 32 KiCad allows so the
    /// mask always fits a `u32`.
    names: Vec<String>,
}

impl LayerIndex {
    fn build(pcb: &Pcb) -> Self {
        let mut names: Vec<String> = Vec::new();
        // The declared stackup first, so F.Cu/B.Cu keep their usual indices.
        for l in pcb.layers() {
            Self::push(&mut names, &l.name);
        }
        // Then anything a copper item actually references: a hand-written or
        // synthesised board may carry no `(layers ...)` block at all, and the
        // audit must still tell its layers apart.
        for s in pcb.segments() {
            Self::push(&mut names, &s.layer());
        }
        for v in pcb.vias() {
            for l in v.layers() {
                Self::push(&mut names, &l);
            }
        }
        for fp in pcb.footprints() {
            for pad in fp.pads() {
                for l in pad.layers() {
                    Self::push(&mut names, &l);
                }
            }
        }
        if names.is_empty() {
            names = vec!["F.Cu".to_string(), "B.Cu".to_string()];
        }
        LayerIndex { names }
    }

    /// Record `name` if it is a concrete copper layer we have not seen yet.
    fn push(names: &mut Vec<String>, name: &str) {
        if !Self::is_concrete_copper(name) || names.len() >= 32 {
            return;
        }
        if !names.iter().any(|n| n == name) {
            names.push(name.to_string());
        }
    }

    /// `F.Cu`, `B.Cu`, `In3.Cu` are concrete; `*.Cu` and `F&B.Cu` are wildcards
    /// that stand for a set and so never become layers of their own.
    fn is_concrete_copper(name: &str) -> bool {
        name.ends_with(".Cu") && !name.starts_with('*') && !name.contains('&')
    }

    fn all_mask(&self) -> u32 {
        if self.names.len() >= 32 {
            u32::MAX
        } else {
            (1u32 << self.names.len()) - 1
        }
    }

    /// Bitmask for one layer token, expanding the KiCad wildcards.
    fn mask_one(&self, name: &str) -> u32 {
        match name {
            "*.Cu" => self.all_mask(),
            "F&B.Cu" => self.mask_one("F.Cu") | self.mask_one("B.Cu"),
            _ => self
                .names
                .iter()
                .position(|n| n == name)
                .map(|i| 1u32 << i)
                .unwrap_or(0),
        }
    }

    fn mask_many(&self, names: &[String]) -> u32 {
        names.iter().fold(0u32, |m, n| m | self.mask_one(n))
    }
}

/// A pad's copper outline in board coordinates.
enum PadShape {
    Circle {
        r: f64,
    },
    /// Rectangle half-extents with the pad's board-frame rotation.
    Rect {
        half: (f64, f64),
        angle: f64,
    },
    /// Stadium/obround: a segment of half-length `half_span` along the pad's
    /// major axis, stroked with radius `r` (half the minor dimension). `angle`
    /// is the major axis in the board frame.
    Capsule {
        half_span: f64,
        r: f64,
        angle: f64,
    },
}

/// A pad in absolute board coordinates, for point-in-pad connectivity + endpoint
/// tests. Keyed by numeric net id (segments carry only the id after a merge).
struct PlacedPadGeo {
    net: i64,
    center: (f64, f64),
    shape: PadShape,
    /// Copper layers this pad is present on. An SMD pad occupies exactly one; a
    /// through-hole pad bridges every copper layer.
    layers: u32,
    /// Radius of a circle around `center` that encloses the pad, for the coarse
    /// spatial-hash query that precedes the exact containment test.
    bound_r: f64,
}

impl PlacedPadGeo {
    /// Whether board point `p` lies inside this pad. Layer membership is the
    /// caller's business; this is pure geometry.
    fn contains(&self, p: (f64, f64)) -> bool {
        let dx = p.0 - self.center.0;
        let dy = p.1 - self.center.1;
        match self.shape {
            PadShape::Circle { r } => {
                let rr = r + PAD_EPS_MM;
                dx * dx + dy * dy <= rr * rr
            }
            PadShape::Rect { half, angle } => {
                let (lx, ly) = Self::to_local(dx, dy, angle);
                lx.abs() <= half.0 + PAD_EPS_MM && ly.abs() <= half.1 + PAD_EPS_MM
            }
            PadShape::Capsule { half_span, r, angle } => {
                let (lx, ly) = Self::to_local(dx, dy, angle);
                // Distance to the capsule's spine, which runs along local x.
                let nearest = lx.clamp(-half_span, half_span);
                let ex = lx - nearest;
                let rr = r + PAD_EPS_MM;
                ex * ex + ly * ly <= rr * rr
            }
        }
    }

    /// Inverse of the y-down placement rotation used by `Pad::absolute_pos`
    /// (which maps local `(px, py)` to `(px*c + py*s, -px*s + py*c)`).
    fn to_local(dx: f64, dy: f64, angle: f64) -> (f64, f64) {
        let (s, c) = angle.sin_cos();
        (c * dx - s * dy, s * dx + c * dy)
    }
}

/// The pad's outline orientation in BOARD space, in degrees.
///
/// In a `.kicad_pcb` the pad `at` angle is the pad's ABSOLUTE board orientation:
/// KiCad folds the footprint rotation into it when serialising, so `frot` must
/// NOT be added again. Verified three ways:
///
///  * pcbnew-written corpus boards. Every footprint at `-90` in
///    `frontend/public/samples/watchy.kicad_pcb` carries pads at `270`, every
///    footprint at `90` carries pads at `90`, every one at `180` carries `180`.
///  * `forge_model::Pad::absolute_pos`, which rotates the pad OFFSET by `frot`
///    and documents the `at` angle as absolute and therefore positionless.
///  * `hauksbee-extract`'s DRC pad outline transform, which rotates the outline
///    by the pad angle alone for the same stated reason.
///
/// Our own two producers happen to write `0` here: the builder hard-codes a zero
/// pad angle (`forge_model::FootprintBuilder::add_pad`) and the decompiler drops
/// the pad angle when it lowers a board to code. That is self-consistent under
/// the absolute reading (an unrotated body on a rotated placement), so the
/// generated path is unaffected while imported KiCad boards now read correctly.
fn pad_board_angle_deg(_frot: f64, prot: f64) -> f64 {
    prot
}

/// The pad's outline orientation RELATIVE to its footprint, in degrees: what a
/// Specctra image, which is written in un-rotated footprint space, must carry.
fn pad_image_angle_deg(frot: f64, prot: f64) -> f64 {
    pad_board_angle_deg(frot, prot) - frot
}

/// Absolute geometry of every routable pad on the board (skips `np_thru_hole`
/// mounting holes, which carry no net).
fn placed_pads(pcb: &Pcb, li: &LayerIndex) -> Vec<PlacedPadGeo> {
    let mut out = Vec::new();
    for fp in pcb.footprints() {
        let (_, _, frot) = fp.at();
        for pad in fp.pads() {
            if matches!(pad.kind(), PadKind::NpThruHole) {
                continue;
            }
            let net = pad.net().map(|(id, _)| id).unwrap_or(0);
            let center = pad.absolute_pos(&fp);
            let (w, h) = (pad.size().0.max(0.01), pad.size().1.max(0.01));
            let (_, _, prot) = pad.at();
            let deg = pad_board_angle_deg(frot, prot);
            let angle = deg.to_radians();
            let shape_name = pad.shape();
            let round = shape_name == "circle" || shape_name == "oval";
            let shape = if round && (w - h).abs() < 1e-6 {
                PadShape::Circle { r: w * 0.5 }
            } else if shape_name == "oval" {
                // The major axis is local x when the pad is wider than tall,
                // else local y, which is the same capsule turned a quarter turn.
                let horizontal = w >= h;
                let (major, minor) = if horizontal { (w, h) } else { (h, w) };
                PadShape::Capsule {
                    half_span: (major - minor) * 0.5,
                    r: minor * 0.5,
                    angle: if horizontal { angle } else { angle + std::f64::consts::FRAC_PI_2 },
                }
            } else {
                PadShape::Rect {
                    half: (w * 0.5, h * 0.5),
                    angle,
                }
            };
            // A through-hole pad's copper is on every layer whatever its
            // `layers` token says; an SMD pad is only on the ones it lists.
            let layers = match pad.kind() {
                PadKind::ThruHole | PadKind::NpThruHole => li.all_mask(),
                _ => {
                    let m = li.mask_many(&pad.layers());
                    // A pad that names no layer we know of would be invisible to
                    // the audit; fall back to the footprint's own side.
                    if m == 0 {
                        li.mask_one(if fp.layer().starts_with("B.") {
                            "B.Cu"
                        } else {
                            "F.Cu"
                        })
                    } else {
                        m
                    }
                }
            };
            out.push(PlacedPadGeo {
                net,
                center,
                shape,
                layers,
                bound_r: (w * w + h * h).sqrt() * 0.5,
            });
        }
    }
    out
}

/// A uniform spatial hash over board points, so the audit's containment and
/// junction sweeps are proportional to what is actually nearby rather than to
/// the square of the net size. A ground net with a few hundred pads and a
/// thousand track endpoints made the old all-pairs loop the slowest thing in the
/// route pipeline.
struct PointGrid {
    cells: std::collections::HashMap<(i32, i32), Vec<usize>>,
}

impl PointGrid {
    fn key(p: (f64, f64)) -> (i32, i32) {
        (
            (p.0 / GRID_CELL_MM).floor() as i32,
            (p.1 / GRID_CELL_MM).floor() as i32,
        )
    }

    fn build(points: &[(f64, f64)]) -> Self {
        let mut cells: std::collections::HashMap<(i32, i32), Vec<usize>> =
            std::collections::HashMap::new();
        for (i, p) in points.iter().enumerate() {
            cells.entry(Self::key(*p)).or_default().push(i);
        }
        PointGrid { cells }
    }

    /// Every point index whose cell overlaps the axis-aligned box, appended to
    /// `out`. A superset of the true answer; callers still test exactly.
    fn query_box(&self, min: (f64, f64), max: (f64, f64), out: &mut Vec<usize>) {
        let (x0, y0) = Self::key(min);
        let (x1, y1) = Self::key(max);
        for cx in x0..=x1 {
            for cy in y0..=y1 {
                if let Some(v) = self.cells.get(&(cx, cy)) {
                    out.extend_from_slice(v);
                }
            }
        }
    }

    fn query_radius(&self, center: (f64, f64), r: f64, out: &mut Vec<usize>) {
        self.query_box((center.0 - r, center.1 - r), (center.0 + r, center.1 + r), out);
    }
}

/// Squared distance from `p` to the segment `a`-`b`.
fn point_seg_dist2(p: (f64, f64), a: (f64, f64), b: (f64, f64)) -> f64 {
    let (vx, vy) = (b.0 - a.0, b.1 - a.1);
    let len2 = vx * vx + vy * vy;
    let t = if len2 <= f64::EPSILON {
        0.0
    } else {
        (((p.0 - a.0) * vx + (p.1 - a.1) * vy) / len2).clamp(0.0, 1.0)
    };
    dist2(p, (a.0 + t * vx, a.1 + t * vy))
}

/// Honest routed-connections summary derived from the merged board.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Connectivity {
    /// Total rat-line connections the board needs: sum over nets of `pads - 1`.
    pub total: usize,
    /// Connections actually made: `total - unrouted`.
    pub routed: usize,
    /// Connections still open: sum over nets of `components - 1`, where a net's
    /// pads are split into connected components under the merged copper.
    pub unrouted: usize,
}

/// What a copper node in the per-net walk IS, which decides the join rules that
/// can apply to it beyond plain coincidence.
enum NodeKind {
    /// Index into the placed-pads list.
    Pad(usize),
    /// Endpoint of the net-local segment with this index.
    SegEnd(usize),
    /// A via with this copper radius.
    Via(f64),
}

/// Compute the routed-connections summary: for each net, split its pads into
/// connected components joined by the merged tracks and vias, and count each
/// extra component as one remaining rat-line. A fully routed net collapses to a
/// single component (0 unrouted); a net with no copper leaves every connection
/// open. This is the metric the engine reports, replacing the old "nets with at
/// least one wire" count that called a shorted board 100% routed.
///
/// The walk is LAYER-AWARE: two copper points only join when they share a
/// copper layer. An F.Cu track ending where a B.Cu track ends is not a
/// connection without a via there; an F.Cu endpoint over a B.Cu-only pad is not
/// touching that pad. Through-hole pads bridge every layer; a via bridges
/// exactly the span its `(layers ...)` declares.
///
/// Joins recognised, all same-net and layer-gated:
///  * coincident points (within [`COINCIDE_EPS_MM`]);
///  * a copper point inside a pad's outline;
///  * a T-junction: a segment endpoint (or via) landing on another segment's
///    INTERIOR within half the combined widths, which freerouting emits
///    routinely and the old walk miscounted as unrouted;
///  * a segment passing through a pad (nearest point of the span inside the
///    outline) even when neither endpoint is.
///
/// OUT OF SCOPE, deliberately: `(arc ...)` tracks and `(zone ...)` copper fills
/// are not modelled, so a net stitched only by an arc or a filled zone reads as
/// unrouted; and two segments crossing interior-to-interior with no endpoint on
/// either (an X, not a T) are not joined. Neither router merge path emits any
/// of those.
pub fn connectivity(pcb: &Pcb) -> Connectivity {
    use std::collections::{HashMap, HashSet};

    let li = LayerIndex::build(pcb);
    let pads = placed_pads(pcb, &li);
    let mut net_pads: HashMap<i64, Vec<usize>> = HashMap::new();
    for (i, p) in pads.iter().enumerate() {
        if p.net != 0 {
            net_pads.entry(p.net).or_default().push(i);
        }
    }
    // Per net: segment (start, end, width, layer mask). A segment naming a
    // layer the index cannot resolve keeps the old layer-blind behaviour (full
    // mask) rather than silently dropping out of the walk.
    type NetSeg = ((f64, f64), (f64, f64), f64, u32);
    let mut net_seg: HashMap<i64, Vec<NetSeg>> = HashMap::new();
    for s in pcb.segments() {
        let id = s.net_id();
        if id != 0 {
            let mask = match li.mask_one(&s.layer()) {
                0 => li.all_mask(),
                m => m,
            };
            net_seg
                .entry(id)
                .or_default()
                .push((s.start(), s.end(), s.width(), mask));
        }
    }
    // Per net: via (at, copper radius, layer-span mask). A bare `(via ...)`
    // with no layer list is a through via.
    let mut net_via: HashMap<i64, Vec<((f64, f64), f64, u32)>> = HashMap::new();
    for v in pcb.vias() {
        let id = v.net_id();
        if id != 0 {
            let mask = match li.mask_many(&v.layers()) {
                0 => li.all_mask(),
                m => m,
            };
            net_via
                .entry(id)
                .or_default()
                .push((v.at(), v.size() * 0.5, mask));
        }
    }

    let eps2 = COINCIDE_EPS_MM * COINCIDE_EPS_MM;
    let mut total = 0usize;
    let mut unrouted = 0usize;

    for (net, pad_idxs) in &net_pads {
        let np = pad_idxs.len();
        if np < 2 {
            continue;
        }
        total += np - 1;

        // Node list: pad nodes [0, np), then two nodes per segment (its
        // endpoints), then one per via. Parallel arrays carry each node's
        // position, layer mask, and kind.
        let mut points: Vec<(f64, f64)> = Vec::with_capacity(np);
        let mut masks: Vec<u32> = Vec::with_capacity(np);
        let mut kinds: Vec<NodeKind> = Vec::with_capacity(np);
        for &pi in pad_idxs {
            points.push(pads[pi].center);
            masks.push(pads[pi].layers);
            kinds.push(NodeKind::Pad(pi));
        }
        let seg_base = points.len();
        let segs = net_seg.get(net).cloned().unwrap_or_default();
        for (si, (a, b, _w, mask)) in segs.iter().enumerate() {
            points.push(*a);
            masks.push(*mask);
            kinds.push(NodeKind::SegEnd(si));
            points.push(*b);
            masks.push(*mask);
            kinds.push(NodeKind::SegEnd(si));
        }
        for (at, r, mask) in net_via.get(net).cloned().unwrap_or_default() {
            points.push(at);
            masks.push(mask);
            kinds.push(NodeKind::Via(r));
        }

        let n = points.len();
        let mut uf = UnionFind::new(n);
        // A segment's two endpoints are one copper item.
        let mut k = seg_base;
        for _ in &segs {
            uf.union(k, k + 1);
            k += 2;
        }

        let grid = PointGrid::build(&points);
        let mut cand: Vec<usize> = Vec::new();

        // The largest join reach any single node can have, so one inflated grid
        // query per probe is a superset of every exact test below.
        let mut reach = COINCIDE_EPS_MM;
        for kind in &kinds {
            let r = match kind {
                NodeKind::Pad(pi) => pads[*pi].bound_r,
                NodeKind::SegEnd(si) => segs[*si].2 * 0.5,
                NodeKind::Via(r) => *r,
            };
            reach = reach.max(r + PAD_EPS_MM);
        }

        // Pass 1: point-level joins. Coincidence, and pads swallowing any
        // copper point that lies inside their outline. Layer-gated throughout.
        for i in 0..n {
            cand.clear();
            grid.query_radius(points[i], reach + COINCIDE_EPS_MM, &mut cand);
            for &j in &cand {
                if j <= i || masks[i] & masks[j] == 0 {
                    continue;
                }
                let mut join = dist2(points[i], points[j]) <= eps2;
                if !join {
                    if let NodeKind::Pad(pi) = kinds[i] {
                        join = pads[pi].contains(points[j]);
                    }
                }
                if !join {
                    if let NodeKind::Pad(pj) = kinds[j] {
                        join = pads[pj].contains(points[i]);
                    }
                }
                if join {
                    uf.union(i, j);
                }
            }
        }

        // Pass 2: span-level joins against each segment's INTERIOR. An endpoint
        // or via within half the combined widths of the span is a T-junction; a
        // pad whose outline the span passes through is connected even when no
        // endpoint falls inside it.
        for (si, (a, b, w, smask)) in segs.iter().enumerate() {
            let own0 = seg_base + 2 * si;
            let own1 = own0 + 1;
            let half_w = w * 0.5;
            let min = (a.0.min(b.0) - half_w - reach, a.1.min(b.1) - half_w - reach);
            let max = (a.0.max(b.0) + half_w + reach, a.1.max(b.1) + half_w + reach);
            cand.clear();
            grid.query_box(min, max, &mut cand);
            for &j in &cand {
                if j == own0 || j == own1 || masks[j] & smask == 0 {
                    continue;
                }
                let join = match kinds[j] {
                    NodeKind::SegEnd(sj) => {
                        let thr = half_w + segs[sj].2 * 0.5;
                        point_seg_dist2(points[j], *a, *b) <= thr * thr
                    }
                    NodeKind::Via(r) => {
                        let thr = half_w + r;
                        point_seg_dist2(points[j], *a, *b) <= thr * thr
                    }
                    NodeKind::Pad(pi) => {
                        let p = &pads[pi];
                        // Nearest point of the span to the pad centre; inside
                        // the outline means the spine crosses the pad.
                        let (vx, vy) = (b.0 - a.0, b.1 - a.1);
                        let len2 = vx * vx + vy * vy;
                        let t = if len2 <= f64::EPSILON {
                            0.0
                        } else {
                            (((p.center.0 - a.0) * vx + (p.center.1 - a.1) * vy) / len2)
                                .clamp(0.0, 1.0)
                        };
                        p.contains((a.0 + t * vx, a.1 + t * vy))
                    }
                };
                if join {
                    uf.union(j, own0);
                }
            }
        }

        let mut roots: HashSet<usize> = HashSet::new();
        for idx in 0..np {
            roots.insert(uf.find(idx));
        }
        unrouted += roots.len().saturating_sub(1);
    }

    Connectivity {
        total,
        routed: total - unrouted,
        unrouted,
    }
}

/// Count merged copper endpoints (segment ends and vias) that fall inside a pad
/// of a DIFFERENT net. A correct route never does this; the placement-rotation
/// bug mirrors footprints so the router terminates wires in the wrong-net pad,
/// which this catches in one point-in-pad sweep. Each offending endpoint is
/// counted once.
///
/// Layer-aware: an F.Cu endpoint above a B.Cu-only pad of another net is NOT a
/// violation, it never touches that copper. A via only violates a pad it shares
/// a layer with.
pub fn endpoint_net_violations(pcb: &Pcb) -> usize {
    let li = LayerIndex::build(pcb);
    let pads = placed_pads(pcb, &li);
    let centers: Vec<(f64, f64)> = pads.iter().map(|p| p.center).collect();
    let grid = PointGrid::build(&centers);
    let max_r = pads
        .iter()
        .map(|p| p.bound_r)
        .fold(0.0_f64, f64::max);

    let mut cand: Vec<usize> = Vec::new();
    let mut in_foreign_pad = |pt: (f64, f64), net: i64, mask: u32| -> bool {
        cand.clear();
        grid.query_radius(pt, max_r + PAD_EPS_MM, &mut cand);
        cand.iter().any(|&pi| {
            let p = &pads[pi];
            p.net != 0 && p.net != net && p.layers & mask != 0 && p.contains(pt)
        })
    };

    let mut viol = 0usize;
    for s in pcb.segments() {
        let net = s.net_id();
        if net == 0 {
            continue;
        }
        let mask = match li.mask_one(&s.layer()) {
            0 => li.all_mask(),
            m => m,
        };
        if in_foreign_pad(s.start(), net, mask) {
            viol += 1;
        }
        if in_foreign_pad(s.end(), net, mask) {
            viol += 1;
        }
    }
    for v in pcb.vias() {
        let net = v.net_id();
        if net == 0 {
            continue;
        }
        let mask = match li.mask_many(&v.layers()) {
            0 => li.all_mask(),
            m => m,
        };
        if in_foreign_pad(v.at(), net, mask) {
            viol += 1;
        }
    }
    viol
}

fn dist2(a: (f64, f64), b: (f64, f64)) -> f64 {
    let dx = a.0 - b.0;
    let dy = a.1 - b.1;
    dx * dx + dy * dy
}

/// Minimal union-find for the per-net connectivity walk.
struct UnionFind {
    parent: Vec<usize>,
    rank: Vec<u8>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        UnionFind {
            parent: (0..n).collect(),
            rank: vec![0; n],
        }
    }

    fn find(&mut self, x: usize) -> usize {
        let mut root = x;
        while self.parent[root] != root {
            root = self.parent[root];
        }
        let mut cur = x;
        while self.parent[cur] != root {
            let next = self.parent[cur];
            self.parent[cur] = root;
            cur = next;
        }
        root
    }

    fn union(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra == rb {
            return;
        }
        match self.rank[ra].cmp(&self.rank[rb]) {
            std::cmp::Ordering::Less => self.parent[ra] = rb,
            std::cmp::Ordering::Greater => self.parent[rb] = ra,
            std::cmp::Ordering::Equal => {
                self.parent[rb] = ra;
                self.rank[ra] += 1;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use forge_model::Pcb;
    use std::collections::BTreeMap;
    use std::f64::consts::PI;

    /// A 3-pad SOT-like footprint with ASYMMETRIC pad positions (so a mirrored
    /// placement is detectable) and ASYMMETRIC rect pads 0.9 x 0.8 (so a wrong
    /// shape orientation is detectable). Nets 1/2/3 keep each pad
    /// distinguishable. `pad_rot` is the pad `at` angle exactly as serialised:
    /// KiCad writes the pad's ABSOLUTE board angle there (footprint rotation
    /// folded in), while hauksbee's own builder writes 0.
    fn sot_on(reference: &str, x: f64, y: f64, rot: f64, layer: &str, pad_rot: f64) -> String {
        format!(
            "  (footprint \"Package_TO_SOT_SMD:SOT-23\" (layer \"{layer}\")\n\
             \x20   (at {x} {y} {rot})\n\
             \x20   (property \"Reference\" \"{reference}\" (at 0 0 0) (layer \"F.SilkS\"))\n\
             \x20   (pad \"1\" smd rect (at -1.0 -1.0 {pad_rot}) (size 0.9 0.8) (layers \"{layer}\") (net 1 \"N1\"))\n\
             \x20   (pad \"2\" smd rect (at 1.0 -1.0 {pad_rot}) (size 0.9 0.8) (layers \"{layer}\") (net 2 \"N2\"))\n\
             \x20   (pad \"3\" smd rect (at 0.0 1.1 {pad_rot}) (size 0.9 0.8) (layers \"{layer}\") (net 3 \"N3\"))\n\
             \x20 )\n"
        )
    }

    fn sot(reference: &str, x: f64, y: f64, rot: f64) -> String {
        sot_on(reference, x, y, rot, "F.Cu", 0.0)
    }

    fn board_header() -> String {
        let mut s = String::from("(kicad_pcb (version 20241229) (generator test)\n");
        s.push_str("  (net 0 \"\")\n  (net 1 \"N1\")\n  (net 2 \"N2\")\n  (net 3 \"N3\")\n");
        s
    }

    /// The four rotations with builder-style pads (serialised pad angle 0).
    fn board_four_rotations() -> String {
        let mut s = board_header();
        s.push_str(&sot("U1", 50.0, 40.0, 0.0));
        s.push_str(&sot("U2", 72.0, 143.0, 90.0));
        s.push_str(&sot("U3", 90.0, 60.0, 180.0));
        s.push_str(&sot("U4", 120.0, 80.0, 270.0));
        s.push_str(")\n");
        s
    }

    /// The four rotations with KiCad-style pads: the serialised pad angle is
    /// ABSOLUTE (equals the footprint rotation for an unrotated pad body), which
    /// is what every pcbnew-written board carries. The old writer added the
    /// footprint rotation on top of this and double-counted.
    fn board_four_rotations_absolute() -> String {
        let mut s = board_header();
        s.push_str(&sot_on("U1", 50.0, 40.0, 0.0, "F.Cu", 0.0));
        s.push_str(&sot_on("U2", 72.0, 143.0, 90.0, "F.Cu", 90.0));
        s.push_str(&sot_on("U3", 90.0, 60.0, 180.0, "F.Cu", 180.0));
        s.push_str(&sot_on("U4", 120.0, 80.0, 270.0, "F.Cu", 270.0));
        s.push_str(")\n");
        s
    }

    /// The asymmetric footprint on the BACK at all four rotations (KiCad-style
    /// absolute pad angles), plus one back footprint whose pads are serialised
    /// at 0 under a 90 degree placement, so the emitted pin rotate is nonzero.
    fn board_back_rotations() -> String {
        let mut s = board_header();
        s.push_str(&sot_on("U1", 50.0, 40.0, 0.0, "B.Cu", 0.0));
        s.push_str(&sot_on("U2", 72.0, 143.0, 90.0, "B.Cu", 90.0));
        s.push_str(&sot_on("U3", 90.0, 60.0, 180.0, "B.Cu", 180.0));
        s.push_str(&sot_on("U4", 120.0, 80.0, 270.0, "B.Cu", 270.0));
        s.push_str(&sot_on("U5", 150.0, 40.0, 90.0, "B.Cu", 0.0));
        s.push_str(")\n");
        s
    }

    struct ParsedDsn {
        /// footprint index -> (x, y, place angle, side)
        places: BTreeMap<usize, (i64, i64, i64, String)>,
        /// footprint index -> (padstack, x, y, pin rotate)
        pins: BTreeMap<usize, Vec<(String, i64, i64, f64)>>,
        rect_padstacks: BTreeMap<String, (i64, i64)>,
    }

    fn parse_dsn(dsn: &str) -> ParsedDsn {
        let mut places = BTreeMap::new();
        let mut pins: BTreeMap<usize, Vec<(String, i64, i64, f64)>> = BTreeMap::new();
        let mut rect_padstacks = BTreeMap::new();
        let mut cur_image: Option<usize> = None;
        let mut cur_padstack: Option<String> = None;

        for line in dsn.lines() {
            let t = line.trim();
            if let Some(rest) = t.strip_prefix("(place ") {
                let rest = rest.trim_end_matches(')');
                let f: Vec<&str> = rest.split_whitespace().collect();
                let idx = f[0].trim_start_matches('C').parse::<usize>().unwrap();
                let x = f[1].parse::<i64>().unwrap();
                let y = f[2].parse::<i64>().unwrap();
                let side = f[3].to_string();
                let ang = f[4].parse::<i64>().unwrap();
                places.insert(idx, (x, y, ang, side));
            } else if let Some(rest) = t.strip_prefix("(image ") {
                let name = rest.trim_end_matches(')').trim();
                let idx = name.trim_start_matches("img_").parse::<usize>().unwrap();
                cur_image = Some(idx);
            } else if let Some(rest) = t.strip_prefix("(pin ") {
                if let Some(idx) = cur_image {
                    // (pin "PS" [(rotate A)] pN x y)
                    let rest = rest.trim().strip_prefix('"').expect("quoted padstack");
                    let q = rest.find('"').expect("padstack close quote");
                    let name = rest[..q].to_string();
                    let mut tail = rest[q + 1..].trim().to_string();
                    let mut rot = 0.0;
                    if let Some(i) = tail.find("(rotate ") {
                        let after = &tail[i + 8..];
                        let k = after.find(')').expect("rotate close paren");
                        rot = after[..k].trim().parse::<f64>().unwrap();
                        tail = format!("{}{}", &tail[..i], &after[k + 1..]);
                    }
                    let f: Vec<&str> = tail.trim_end_matches(')').split_whitespace().collect();
                    let lx = f[1].parse::<i64>().unwrap();
                    let ly = f[2].parse::<i64>().unwrap();
                    pins.entry(idx).or_default().push((name, lx, ly, rot));
                }
            } else if let Some(rest) = t.strip_prefix("(padstack ") {
                cur_padstack = Some(rest.trim().trim_matches('"').to_string());
            } else if t.starts_with("(shape (rect") {
                if let Some(name) = &cur_padstack {
                    let nums: Vec<i64> = t
                        .replace('(', " ")
                        .replace(')', " ")
                        .split_whitespace()
                        .filter_map(|s| s.parse::<i64>().ok())
                        .collect();
                    if nums.len() >= 4 {
                        rect_padstacks.insert(name.clone(), (nums[2].abs(), nums[3].abs()));
                    }
                }
            } else if t == ")" {
                cur_image = None;
                cur_padstack = None;
            }
        }
        ParsedDsn {
            places,
            pins,
            rect_padstacks,
        }
    }

    /// Difference between two angles in degrees, folded into [0, 180].
    fn ang_diff(a: f64, b: f64) -> f64 {
        let d = (a - b).rem_euclid(360.0);
        d.min(360.0 - d)
    }

    /// Shared assertions for a front-side board: every pin's absolute position
    /// (place + the DSN's own placement rotation of the image offset) must equal
    /// the KiCad pad position; the referenced padstack must carry the RAW
    /// (never swapped) extents; and the shape orientation freerouting composes
    /// (place angle + pin rotate) must land the padstack at the pad's serialised
    /// ABSOLUTE angle, which in this writer's mirrored frame means
    /// `place + rotate + prot = 0 (mod 360)`.
    ///
    /// Freerouting rotates the padstack shape by the placement angle itself and
    /// honours per-pin `(rotate)`, on BOTH 1.9.0 and 2.2.4 (verified by the
    /// channel experiment in docs/record/FREEROUTING_DSN_SEMANTICS.md), so pad
    /// rotation must ride on `(rotate)` and never on swapped padstack extents.
    fn assert_front_board(text: &str) {
        let pcb = Pcb::parse(text).expect("parse board");
        let dsn = write_dsn(&pcb, None, &RouteRules::default());
        let parsed = parse_dsn(&dsn);

        let fps = pcb.footprints();
        assert_eq!(fps.len(), 4);
        for (idx, fp) in fps.iter().enumerate() {
            let (px, py, angle, side) = &parsed.places[&idx];
            assert_eq!(side, "front", "fp {idx} side");
            let a = (*angle as f64) * PI / 180.0;
            let (c, sn) = (a.cos(), a.sin());
            let pins = &parsed.pins[&idx];
            let pads = fp.pads();
            assert_eq!(pins.len(), pads.len(), "pin count for fp {idx}");

            let rot = fp.at().2;
            for (k, pad) in pads.iter().enumerate() {
                let (name, lx, ly, pin_rot) = &pins[k];
                // Specctra front angle uses a standard CCW rotation; with the
                // emitted (360 - kicad) angle this must reproduce absolute_pos.
                let rx = (*lx as f64) * c - (*ly as f64) * sn;
                let ry = (*lx as f64) * sn + (*ly as f64) * c;
                let got = (*px as f64 + rx, *py as f64 + ry);

                let (kx, ky) = pad.absolute_pos(fp);
                let exp = (to_units(kx) as f64, to_units(ky) as f64);
                assert!(
                    (got.0 - exp.0).abs() <= 3.0 && (got.1 - exp.1).abs() <= 3.0,
                    "fp {idx} (rot {rot}) pin {k}: DSN pos {got:?} != KiCad {exp:?}"
                );

                // The padstack always carries the pad's own raw extents.
                let (w, h) = pad.size();
                let (hx, hy) = parsed.rect_padstacks[name];
                assert_eq!(
                    (hx, hy),
                    (to_units(w * 0.5), to_units(h * 0.5)),
                    "fp {idx} (rot {rot}) pin {k}: padstack {name} extents must be raw"
                );

                // Shape orientation: place + rotate must cancel the pad's
                // absolute serialised angle in the mirrored frame.
                let prot = pad.at().2;
                assert!(
                    ang_diff(*angle as f64 + pin_rot + prot, 0.0) < 1e-6,
                    "fp {idx} (rot {rot}) pin {k}: place {angle} + rotate {pin_rot} + pad angle {prot} != 0 mod 360"
                );
            }
        }
    }

    /// Builder-style boards (pad angle serialised as 0): positions match KiCad
    /// at every rotation and the pin rotate compensates the place angle so the
    /// shape stays at its serialised absolute angle.
    #[test]
    fn dsn_rotation_and_pin_rotate_match_kicad() {
        assert_front_board(&board_four_rotations());
    }

    /// KiCad-style boards (pad angle serialised ABSOLUTE): the emitted pin
    /// rotate must be ZERO for an unrotated pad body, not footprint + pad
    /// (which double-counts and was the masked bug), and positions still match.
    #[test]
    fn dsn_absolute_pad_angles_do_not_double_count() {
        let text = board_four_rotations_absolute();
        assert_front_board(&text);
        // The double-count is invisible to the composed-angle identity only if
        // the identity is wrong, so pin down the raw value too: relative angle
        // zero means no (rotate) at all.
        let pcb = Pcb::parse(&text).expect("parse board");
        let dsn = write_dsn(&pcb, None, &RouteRules::default());
        let parsed = parse_dsn(&dsn);
        for (idx, pins) in &parsed.pins {
            for (name, _, _, rot) in pins {
                assert_eq!(
                    *rot, 0.0,
                    "fp {idx} pin {name}: absolute pad angle must emit no pin rotate"
                );
            }
        }
    }

    /// Back-side placements: the image is pre-flipped to a TOP view (x negated)
    /// and freerouting mirrors it back on import, so running the emitted pins
    /// through freerouting's own back chain, mirror about the y axis THEN the
    /// placement rotation (the order verified on both jars), must reproduce
    /// every KiCad pad position. Shape identity on the back is
    /// `place - rotate + pad angle = 0 (mod 360)` because the mirror flips the
    /// pin rotation's sense.
    #[test]
    fn dsn_back_side_pins_match_kicad() {
        let text = board_back_rotations();
        let pcb = Pcb::parse(&text).expect("parse board");
        let dsn = write_dsn(&pcb, None, &RouteRules::default());
        let parsed = parse_dsn(&dsn);

        let fps = pcb.footprints();
        assert_eq!(fps.len(), 5);
        for (idx, fp) in fps.iter().enumerate() {
            let (px, py, angle, side) = &parsed.places[&idx];
            assert_eq!(side, "back", "fp {idx} side");
            let a = (*angle as f64) * PI / 180.0;
            let (c, sn) = (a.cos(), a.sin());
            let pins = &parsed.pins[&idx];
            let pads = fp.pads();
            assert_eq!(pins.len(), pads.len(), "pin count for fp {idx}");

            let rot = fp.at().2;
            for (k, pad) in pads.iter().enumerate() {
                let (name, lx, ly, pin_rot) = &pins[k];
                // freerouting back chain: mirror the image offset about the y
                // axis, then rotate by the place angle, then translate.
                let (mx, my) = (-(*lx as f64), *ly as f64);
                let rx = mx * c - my * sn;
                let ry = mx * sn + my * c;
                let got = (*px as f64 + rx, *py as f64 + ry);

                let (kx, ky) = pad.absolute_pos(fp);
                let exp = (to_units(kx) as f64, to_units(ky) as f64);
                assert!(
                    (got.0 - exp.0).abs() <= 3.0 && (got.1 - exp.1).abs() <= 3.0,
                    "fp {idx} (rot {rot}) pin {k}: DSN back pos {got:?} != KiCad {exp:?}"
                );

                let (w, h) = pad.size();
                let (hx, hy) = parsed.rect_padstacks[name];
                assert_eq!(
                    (hx, hy),
                    (to_units(w * 0.5), to_units(h * 0.5)),
                    "fp {idx} (rot {rot}) pin {k}: padstack {name} extents must be raw"
                );

                let prot = pad.at().2;
                assert!(
                    ang_diff(*angle as f64 - pin_rot + prot, 0.0) < 1e-6,
                    "fp {idx} (rot {rot}) pin {k}: place {angle} - rotate {pin_rot} + pad angle {prot} != 0 mod 360"
                );
            }
        }
    }

    /// Every `(rect ...)` the writer emits (boundary and padstacks) must be
    /// x-ascending and y-ascending: freerouting normalises only the y order,
    /// and an x-descending rect is silently an EMPTY pad (IntBox::is_empty),
    /// invisible to the router with no warning.
    #[test]
    fn dsn_rects_are_always_ascending() {
        for text in [
            board_four_rotations(),
            board_four_rotations_absolute(),
            board_back_rotations(),
        ] {
            let pcb = Pcb::parse(&text).expect("parse board");
            let dsn = write_dsn(&pcb, None, &RouteRules::default());
            let mut seen = 0;
            for line in dsn.lines() {
                let t = line.trim();
                if !t.contains("(rect ") {
                    continue;
                }
                let nums: Vec<i64> = t
                    .replace('(', " ")
                    .replace(')', " ")
                    .split_whitespace()
                    .filter_map(|s| s.parse::<i64>().ok())
                    .collect();
                assert!(nums.len() >= 4, "rect with fewer than 4 numbers: {t}");
                let n = nums.len();
                let (x0, y0, x1, y1) = (nums[n - 4], nums[n - 3], nums[n - 2], nums[n - 1]);
                assert!(x0 < x1, "x-descending rect (silent empty pad): {t}");
                assert!(y0 < y1, "y-descending rect: {t}");
                seen += 1;
            }
            assert!(seen >= 2, "expected boundary + padstack rects, saw {seen}");
        }
    }

    /// Elongated oval pads are stadiums: one `(path ...)` span stroked with the
    /// minor dimension, not a square-cornered rect.
    #[test]
    fn dsn_oval_pads_emit_capsule_paths() {
        let mut s = board_header();
        s.push_str(
            "  (footprint \"R0805\" (layer \"F.Cu\")\n\
             \x20   (at 50.0 40.0 0)\n\
             \x20   (pad \"1\" smd oval (at -1.0 0.0) (size 1.6 0.8) (layers \"F.Cu\") (net 1 \"N1\"))\n\
             \x20 )\n",
        );
        s.push_str(")\n");
        let pcb = Pcb::parse(&s).expect("parse board");
        let dsn = write_dsn(&pcb, None, &RouteRules::default());
        assert!(
            dsn.contains("(padstack \"Oval[T]Pad_16000x8000\""),
            "missing oval padstack:\n{dsn}"
        );
        assert!(
            dsn.contains("(shape (path F.Cu 8000 -4000 0 4000 0))"),
            "missing capsule path span:\n{dsn}"
        );
    }

    /// The emitted place angle is (360 - kicad) mod 360, never the raw angle.
    #[test]
    fn place_angle_is_specctra_inverse() {
        assert_eq!(specctra_place_angle(0.0), 0);
        assert_eq!(specctra_place_angle(90.0), 270);
        assert_eq!(specctra_place_angle(180.0), 180);
        assert_eq!(specctra_place_angle(270.0), 90);
        assert_eq!(specctra_place_angle(-90.0), 90);
    }

    fn one_pad_fp_on(
        reference: &str,
        x: f64,
        y: f64,
        net_id: i64,
        net_name: &str,
        layer: &str,
        thru: bool,
    ) -> String {
        let (kind, layers, drill) = if thru {
            ("thru_hole circle", "*.Cu".to_string(), " (drill 0.6)")
        } else {
            ("smd rect", layer.to_string(), "")
        };
        format!(
            "  (footprint \"R\" (layer \"{layer}\")\n\
             \x20   (at {x} {y} 0)\n\
             \x20   (property \"Reference\" \"{reference}\" (at 0 0 0) (layer \"F.SilkS\"))\n\
             \x20   (pad \"1\" {kind} (at 0.0 0.0) (size 0.9 0.9){drill} (layers \"{layers}\") (net {net_id} \"{net_name}\"))\n\
             \x20 )\n"
        )
    }

    fn one_pad_fp(reference: &str, x: f64, y: f64, net_id: i64, net_name: &str) -> String {
        one_pad_fp_on(reference, x, y, net_id, net_name, "F.Cu", false)
    }

    fn pad_board(fps: &[String]) -> Pcb {
        let mut s = String::from("(kicad_pcb (version 20241229) (generator test)\n");
        s.push_str("  (net 0 \"\")\n  (net 1 \"N1\")\n  (net 2 \"N2\")\n");
        s.push_str("  (layers (0 \"F.Cu\" signal) (2 \"B.Cu\" signal))\n");
        for fp in fps {
            s.push_str(fp);
        }
        s.push_str(")\n");
        Pcb::parse(&s).expect("parse pad board")
    }

    fn two_pad_board() -> Pcb {
        pad_board(&[
            one_pad_fp("A1", 10.0, 10.0, 1, "N1"),
            one_pad_fp("A2", 20.0, 10.0, 1, "N1"),
            one_pad_fp("A3", 30.0, 10.0, 2, "N2"),
        ])
    }

    #[test]
    fn connectivity_counts_unrouted_and_routed() {
        // Net 1 has two pads (one connection); net 2 has a single pad (none).
        let mut pcb = two_pad_board();
        let before = connectivity(&pcb);
        assert_eq!(before.total, 1, "one connection needed on net 1");
        assert_eq!(before.unrouted, 1, "no copper yet");
        assert_eq!(before.routed, 0);

        // Route it: a track from pad A1 to pad A2 on net 1.
        pcb.add_segment((10.0, 10.0), (20.0, 10.0), 0.25, "F.Cu", Some(1));
        let after = connectivity(&pcb);
        assert_eq!(after.total, 1);
        assert_eq!(after.unrouted, 0, "net 1 now fully connected");
        assert_eq!(after.routed, 1);
    }

    #[test]
    fn endpoint_net_violation_flags_wrong_net_pad() {
        let mut pcb = two_pad_board();
        // A net-1 track whose far end lands inside pad A3 (net 2): the exact
        // signature of a mirrored footprint. One violation.
        pcb.add_segment((10.0, 10.0), (30.0, 10.0), 0.25, "F.Cu", Some(1));
        assert_eq!(endpoint_net_violations(&pcb), 1);

        // A clean net-1 track (both ends on net-1 pads) is not a violation.
        let mut clean = two_pad_board();
        clean.add_segment((10.0, 10.0), (20.0, 10.0), 0.25, "F.Cu", Some(1));
        assert_eq!(endpoint_net_violations(&clean), 0);
    }

    /// An F.Cu endpoint directly over a B.Cu-only pad of another net never
    /// touches that copper: not a violation. The same geometry with the pad on
    /// F.Cu is one.
    #[test]
    fn endpoint_net_violation_is_layer_aware() {
        let mut pcb = pad_board(&[
            one_pad_fp("A1", 10.0, 10.0, 1, "N1"),
            one_pad_fp("A2", 20.0, 10.0, 1, "N1"),
            one_pad_fp_on("A3", 30.0, 10.0, 2, "N2", "B.Cu", false),
        ]);
        // Net-1 F.Cu track ends exactly over the net-2 B.Cu pad: no touch.
        pcb.add_segment((10.0, 10.0), (30.0, 10.0), 0.25, "F.Cu", Some(1));
        assert_eq!(endpoint_net_violations(&pcb), 0, "cross-layer is no touch");

        // A through-hole foreign pad is on every layer: the same endpoint hits.
        let mut th = pad_board(&[
            one_pad_fp("A1", 10.0, 10.0, 1, "N1"),
            one_pad_fp("A2", 20.0, 10.0, 1, "N1"),
            one_pad_fp_on("A3", 30.0, 10.0, 2, "N2", "F.Cu", true),
        ]);
        th.add_segment((10.0, 10.0), (30.0, 10.0), 0.25, "F.Cu", Some(1));
        assert_eq!(endpoint_net_violations(&th), 1, "through-hole pad bridges");
    }

    /// An F.Cu track ending exactly where a B.Cu track ends is NOT a
    /// connection; a via at the meeting point is what joins the layers.
    #[test]
    fn cross_layer_endpoints_need_a_via() {
        let mk = || {
            pad_board(&[
                one_pad_fp("A1", 10.0, 10.0, 1, "N1"),
                one_pad_fp_on("A2", 20.0, 10.0, 1, "N1", "B.Cu", false),
            ])
        };
        let mut pcb = mk();
        pcb.add_segment((10.0, 10.0), (15.0, 10.0), 0.25, "F.Cu", Some(1));
        pcb.add_segment((15.0, 10.0), (20.0, 10.0), 0.25, "B.Cu", Some(1));
        let no_via = connectivity(&pcb);
        assert_eq!(no_via.total, 1);
        assert_eq!(no_via.unrouted, 1, "coincident cross-layer ends are open");

        let mut with_via = mk();
        with_via.add_segment((10.0, 10.0), (15.0, 10.0), 0.25, "F.Cu", Some(1));
        with_via.add_segment((15.0, 10.0), (20.0, 10.0), 0.25, "B.Cu", Some(1));
        with_via.add_via((15.0, 10.0), 0.6, 0.3, &["F.Cu", "B.Cu"], Some(1));
        assert_eq!(connectivity(&with_via).unrouted, 0, "via closes the span");
    }

    /// An F.Cu track ending over a B.Cu-only pad of its OWN net has not reached
    /// it either; the same track to a through-hole pad has.
    #[test]
    fn pad_touch_is_layer_aware() {
        let mut pcb = pad_board(&[
            one_pad_fp("A1", 10.0, 10.0, 1, "N1"),
            one_pad_fp_on("A2", 20.0, 10.0, 1, "N1", "B.Cu", false),
        ]);
        pcb.add_segment((10.0, 10.0), (20.0, 10.0), 0.25, "F.Cu", Some(1));
        assert_eq!(connectivity(&pcb).unrouted, 1, "wrong side of the board");

        let mut th = pad_board(&[
            one_pad_fp("A1", 10.0, 10.0, 1, "N1"),
            one_pad_fp_on("A2", 20.0, 10.0, 1, "N1", "F.Cu", true),
        ]);
        th.add_segment((10.0, 10.0), (20.0, 10.0), 0.25, "F.Cu", Some(1));
        assert_eq!(connectivity(&th).unrouted, 0, "through-hole reachable on F");
        // And reachable from the back too: bridge two through-hole pads on B.Cu.
        let mut th2 = pad_board(&[
            one_pad_fp_on("A1", 10.0, 10.0, 1, "N1", "F.Cu", true),
            one_pad_fp_on("A2", 20.0, 10.0, 1, "N1", "F.Cu", true),
        ]);
        th2.add_segment((10.0, 10.0), (20.0, 10.0), 0.25, "B.Cu", Some(1));
        assert_eq!(connectivity(&th2).unrouted, 0, "through-hole bridges to B");
    }

    /// A segment ending on another same-net same-layer segment's INTERIOR
    /// within half the combined width is a T-junction and a real connection;
    /// the old walk called this unrouted and --route-strict rejected good
    /// boards over it.
    #[test]
    fn t_junction_counts_as_connected() {
        let mut pcb = pad_board(&[
            one_pad_fp("A1", 10.0, 10.0, 1, "N1"),
            one_pad_fp("A2", 20.0, 10.0, 1, "N1"),
            one_pad_fp("A3", 15.0, 15.0, 1, "N1"),
        ]);
        pcb.add_segment((10.0, 10.0), (20.0, 10.0), 0.25, "F.Cu", Some(1));
        pcb.add_segment((15.0, 15.0), (15.0, 10.0), 0.25, "F.Cu", Some(1));
        let c = connectivity(&pcb);
        assert_eq!(c.total, 2);
        assert_eq!(c.unrouted, 0, "T-junction joins the spur");
    }

    /// The T-junction join threshold is half the combined width: a 0.3 mm gap
    /// to the spine is open at widths 0.25/0.25 (threshold 0.25) and closed
    /// when the spur widens to 0.55 (threshold 0.4).
    #[test]
    fn t_junction_respects_half_combined_width() {
        let mk = |spur_width: f64| {
            let mut pcb = pad_board(&[
                one_pad_fp("A1", 10.0, 10.0, 1, "N1"),
                one_pad_fp("A2", 20.0, 10.0, 1, "N1"),
                one_pad_fp("A3", 15.0, 15.0, 1, "N1"),
            ]);
            pcb.add_segment((10.0, 10.0), (20.0, 10.0), 0.25, "F.Cu", Some(1));
            pcb.add_segment((15.0, 15.0), (15.0, 10.3), spur_width, "F.Cu", Some(1));
            pcb
        };
        assert_eq!(connectivity(&mk(0.25)).unrouted, 1, "0.3 gap > 0.25 reach");
        assert_eq!(connectivity(&mk(0.55)).unrouted, 0, "0.3 gap <= 0.4 reach");
    }

    /// A T whose endpoint coincides in x/y with the other segment but sits on
    /// the other copper layer is NOT a junction. The spur's pad is through-hole
    /// so the only missing link is the cross-layer T itself.
    #[test]
    fn t_junction_is_layer_gated() {
        let mut pcb = pad_board(&[
            one_pad_fp("A1", 10.0, 10.0, 1, "N1"),
            one_pad_fp("A2", 20.0, 10.0, 1, "N1"),
            one_pad_fp_on("A3", 15.0, 15.0, 1, "N1", "F.Cu", true),
        ]);
        pcb.add_segment((10.0, 10.0), (20.0, 10.0), 0.25, "F.Cu", Some(1));
        // The spur runs on B.Cu from the through-hole pad to the F.Cu spine.
        pcb.add_segment((15.0, 15.0), (15.0, 10.0), 0.25, "B.Cu", Some(1));
        assert_eq!(
            connectivity(&pcb).unrouted,
            1,
            "a cross-layer T is not a connection"
        );
    }

    /// A segment passing straight THROUGH a pad connects it even though
    /// neither endpoint lies inside the outline.
    #[test]
    fn segment_through_pad_connects() {
        let mut pcb = pad_board(&[
            one_pad_fp("A1", 10.0, 10.0, 1, "N1"),
            one_pad_fp("A2", 20.0, 10.0, 1, "N1"),
        ]);
        pcb.add_segment((10.0, 10.0), (30.0, 10.0), 0.25, "F.Cu", Some(1));
        assert_eq!(connectivity(&pcb).unrouted, 0, "span crosses the pad");
    }
}
