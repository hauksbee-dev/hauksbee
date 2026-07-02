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
}

/// Derive a padstack from a pad's shape + size, in units, returning a stable
/// name so identical pads share one padstack definition.
fn padstack_for(shape: &str, size_mm: (f64, f64), through: bool) -> PadStack {
    let layer_tag = if through { "A" } else { "T" }; // All-layer vs Top
    match shape {
        "circle" | "oval" if (size_mm.0 - size_mm.1).abs() < 1e-6 => {
            let d = to_units(size_mm.0.max(0.1));
            PadStack {
                name: format!("Round[{layer_tag}]Pad_{d}"),
                body: PadBody::Circle(d),
                through,
            }
        }
        _ => {
            let hx = to_units(size_mm.0.max(0.1) * 0.5);
            let hy = to_units(size_mm.1.max(0.1) * 0.5);
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
    // image name -> Vec<DsnPad in image-local coordinates>
    let mut images: Vec<(String, Vec<(String, (f64, f64), String)>)> = Vec::new();
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
        let side = if fp.layer().starts_with("B.") { "back" } else { "front" };

        let mut local_pins: Vec<(String, (f64, f64), String)> = Vec::new();
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
            let ps = padstack_for(&pad.shape(), pad.size(), through);
            padstacks.entry(ps.name.clone()).or_insert_with(|| ps.clone());
            // Pad position relative to the footprint origin, un-rotated (DSN
            // applies the component rotation itself at placement time).
            let (lx, ly, _) = pad.at();
            local_pins.push((num.clone(), (lx, ly), ps.name.clone()));

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
            norm_rot(*rot)
        );
        let _ = writeln!(s, "    )");
    }
    let _ = writeln!(s, "  )");
    // library
    let _ = writeln!(s, "  (library");
    for (image, pins) in &images {
        let _ = writeln!(s, "    (image {image}");
        for (pin, at, ps) in pins {
            let _ = writeln!(
                s,
                "      (pin \"{}\" {} {} {})",
                ps,
                pin,
                to_units(at.0),
                to_units(at.1)
            );
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
        }
    }
    let _ = writeln!(s, "      (attach off)");
    let _ = writeln!(s, "    )");
}

/// Freerouting wants rotation in degrees, normalised to [0, 360).
fn norm_rot(r: f64) -> i64 {
    let mut v = r % 360.0;
    if v < 0.0 {
        v += 360.0;
    }
    v.round() as i64
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
