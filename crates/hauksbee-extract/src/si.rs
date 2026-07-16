//! Signal-integrity / physics static checks (the `--si` surface).
//!
//! Four pure-arithmetic checks over data hauksbee already extracts (copper
//! geometry, netlists, part values) plus a small table of datasheet constants.
//! Each corresponds to a bug class that really ships:
//!
//! 1. [`check_crystal_load_cap`] - crystal CL spec vs the board's two load caps
//!    and stray capacitance (`CL = C1*C2/(C1+C2) + Cstray`). Wrong caps pull the
//!    oscillator off-frequency or stop it starting.
//! 2. [`check_i2c_rise_time`] - pull-up resistance vs computed bus capacitance vs
//!    the I2C spec rise-time limit (`t_r ~ 0.8473*R*C`). Upgrades the existing
//!    "is a pull-up present" lint to "is the pull-up *sufficient*".
//! 3. [`check_antenna_keepout`] - copper / ground poured inside a chip-antenna or
//!    integrated-module-antenna keepout region (per the antenna/module
//!    datasheet). The Watchy / Inkplate-6 bad-WiFi class.
//! 4. [`check_usb_diff_pair`] - intra-pair routed-length skew on a USB D+/D- pair
//!    vs the full-speed / high-speed limit, plus width/gap consistency.
//!
//! ## Discipline (read before trusting any fire)
//!
//! These follow the same calibration rule as the rest of hauksbee
//! (`docs/record/FAMOUS_SWEEP.md`, `docs/record/KNOWN_FAULTS_VALIDATION.md`): **zero false
//! positives on known-good corpus, or the check does not fire.** Every check has
//! an explicit "unknown -> info, never a fire" path, so a missing datasheet
//! constant produces silence (or an informational note), never a confident
//! false positive. Severity thresholds are set from physics and the corpus
//! distribution, documented per check below and in `docs/SI_CHECKS.md`.
//!
//! The module is split between a pure-physics layer (the `cl_parallel`,
//! `i2c_rise_time_ns`, `routed_length_mm` helpers, all hand-checked in the unit
//! tests) and the board-level audits that attribute the physics to real parts.
//! Geometry is read from the same `.kicad_pcb` s-expression the DRC parses,
//! reusing nothing private to `drc.rs` (this module re-derives only the narrow
//! slice it needs, exactly as `trace_current.rs` does).
//!
//! Long-form how-and-why: docs/how-and-why/hauksbee-extract/si.md.

use std::collections::HashMap;

use forge_sexpr::List;

use crate::{Component, ExtractedBoard};

// ===========================================================================
// Shared reporting types.
// ===========================================================================

/// Severity of an SI finding. Mirrors the netlint severity ladder so the `--si`
/// surface reads the same way as `--lint`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SiSeverity {
    /// Would ship a functional failure (oscillator will not start, bus cannot
    /// meet spec even at the most lenient mode, antenna detuned by ground under
    /// it).
    High,
    /// Degrades margin / robustness; works in the nominal case (frequency error
    /// within pullable range, rise time over fast-mode but under standard-mode).
    Medium,
    /// Ugly / worth noting, unlikely to bite. Also the band for width/gap
    /// inconsistency that is cosmetic.
    Low,
    /// Not a defect at all: a value we could compute and want on the record
    /// (the board's CL, the bus rise time, the pair skew) so the negative is
    /// auditable. Never counts toward "findings".
    Info,
}

impl SiSeverity {
    pub fn as_str(self) -> &'static str {
        match self {
            SiSeverity::High => "high",
            SiSeverity::Medium => "medium",
            SiSeverity::Low => "low",
            SiSeverity::Info => "info",
        }
    }
    /// Info notes are observations, not findings.
    pub fn is_finding(self) -> bool {
        !matches!(self, SiSeverity::Info)
    }
}

/// Which SI check produced a finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SiCheck {
    CrystalLoadCap,
    I2cRiseTime,
    AntennaKeepout,
    UsbDiffPair,
    ControlledImpedance,
    /// IPC-2221 trace-ampacity: a routed trace too narrow for the current
    /// attributed to its net. Surfaced into `--si` from the engine layer (the
    /// attribution needs the bound DB models), see
    /// `hauksbee_engine::checks::ampacity`.
    TraceAmpacity,
    /// Input bulk-capacitor ripple-current overstress on a switching converter.
    /// Also surfaced from the engine layer, see
    /// `hauksbee_engine::checks::ripple`.
    InputCapRipple,
}

impl SiCheck {
    pub fn as_str(self) -> &'static str {
        match self {
            SiCheck::CrystalLoadCap => "crystal_load_cap",
            SiCheck::I2cRiseTime => "i2c_rise_time",
            SiCheck::AntennaKeepout => "antenna_keepout",
            SiCheck::UsbDiffPair => "usb_diff_pair",
            SiCheck::ControlledImpedance => "controlled_impedance",
            SiCheck::TraceAmpacity => "trace_ampacity",
            SiCheck::InputCapRipple => "input_cap_ripple",
        }
    }
}

/// One SI finding (or info note), severity-tagged, with the evidence to
/// reproduce it.
#[derive(Debug, Clone)]
pub struct SiFinding {
    pub check: SiCheck,
    pub severity: SiSeverity,
    pub message: String,
    pub refs: Vec<String>,
    pub nets: Vec<String>,
}

/// The full SI report.
#[derive(Debug, Clone, Default)]
pub struct SiReport {
    pub findings: Vec<SiFinding>,
}

impl SiReport {
    /// Only true findings (excludes informational notes).
    pub fn findings_only(&self) -> impl Iterator<Item = &SiFinding> {
        self.findings.iter().filter(|f| f.severity.is_finding())
    }
    /// Count of true findings (excludes info notes).
    pub fn finding_count(&self) -> usize {
        self.findings_only().count()
    }
    pub fn is_clean(&self) -> bool {
        self.finding_count() == 0
    }
    pub fn of_check(&self, c: SiCheck) -> impl Iterator<Item = &SiFinding> {
        self.findings.iter().filter(move |f| f.check == c)
    }
}

impl ExtractedBoard {
    /// Run all four signal-integrity checks. The raw `.kicad_pcb` text is needed
    /// for the geometry-bearing checks (antenna keepout, USB length skew); when
    /// it is absent or not a KiCad layout, those checks are inert (they report
    /// nothing rather than guessing), while the netlist-only checks (crystal CL
    /// from values, I2C rise time) still run.
    pub fn si_checks(&self, pcb_text: Option<&str>) -> SiReport {
        let mut report = SiReport::default();
        check_crystal_load_cap(self, &mut report);
        check_i2c_rise_time(self, &mut report);

        // Geometry checks need the layout s-expression.
        let root = pcb_text
            .filter(|t| t.contains("(kicad_pcb"))
            .and_then(|t| forge_sexpr::parse(t).ok());
        if let Some(doc) = &root {
            if let Some(r) = doc.root() {
                check_antenna_keepout(self, r, &mut report);
                check_usb_diff_pair(self, r, &mut report);
                impedance::check_controlled_impedance(self, r, &mut report);
            }
        }
        report
    }
}

/// Controlled-impedance signal-integrity check (single-ended microstrip /
/// stripline Z0 and differential pair impedance from geometry + stackup).
pub mod impedance;

// ===========================================================================
// Shared net / part helpers (kept local so the module is self-contained, like
// trace_current.rs; deliberately not pulling private items out of netlint.rs).
// ===========================================================================

/// Normalise a net name: trim, keep the leaf of a hierarchical path, uppercase.
fn norm(name: &str) -> String {
    let n = name.trim();
    let leaf = n.rsplit('/').next().unwrap_or(n);
    leaf.trim().to_ascii_uppercase()
}

fn is_ground(name: &str) -> bool {
    let n = norm(name);
    matches!(
        n.as_str(),
        "GND" | "GNDA" | "GNDD" | "AGND" | "DGND" | "PGND" | "VSS" | "GNDIO" | "0"
    ) || n.starts_with("GND")
}

/// Power-rail nominal voltage by net name (only the rails the I2C check needs to
/// know the high level of). `None` for non-rail nets.
fn rail_voltage(name: &str) -> Option<f64> {
    let n = norm(name);
    match n.as_str() {
        "+5V" | "5V" | "VCC" | "VDD" | "+VCC" | "VBUS" | "+5V0" => Some(5.0),
        "+3V3" | "3V3" | "+3.3V" | "3.3V" | "VCC3V3" | "VDD3V3" | "VDD3P3" | "+3V3A" => Some(3.3),
        "+3V" | "3V" | "+3V0" | "3V0" => Some(3.0),
        "+1V8" | "1V8" | "1.8V" | "VDD1V8" => Some(1.8),
        "+2V8" | "2V8" => Some(2.8),
        "VBAT" | "VBATT" | "VSYS" | "VIN" | "+VBAT" | "VDDIO" | "VIO" => Some(3.7),
        _ => {
            if n.contains("3V3") || n.contains("3.3V") || n.contains("3P3") {
                Some(3.3)
            } else if n.contains("1V8") {
                Some(1.8)
            } else if n.contains("VBAT") || n.contains("VSYS") {
                Some(3.7)
            } else {
                None
            }
        }
    }
}

/// Distinct pad numbers that carry a net. Counts *distinct* pad numbers, not raw
/// pin entries: footprints add net-less mechanical pads (the round-1 0201 bug),
/// and the Eagle `.brd` extractor lists each pad once per signal contact, so a
/// two-terminal part can show four pin entries (pad 1 x2, pad 2 x2). Both must
/// resolve to "two terminals".
fn connected_pads(c: &Component) -> usize {
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for p in &c.pins {
        if p.net.is_some() {
            seen.insert(p.number.as_str());
        }
    }
    seen.len()
}

/// The reference designator with a single mirror-prefix stripped, uppercased.
/// Split-keyboard layouts (Corne / crkbd, Lily58) duplicate the right half with
/// a lowercase `r` prefix (`rC2`, `rY1`, `rR3`, `rU1`), so the type classifiers
/// must see `C2`/`Y1`/`R3`/`U1` underneath. Only a lowercase `r` immediately
/// before an uppercase designator letter is treated as the mirror prefix, so a
/// genuine `R5` / `RV1` is untouched.
fn ref_designator(c: &Component) -> String {
    let r = c.reference.trim();
    let bytes = r.as_bytes();
    if bytes.len() >= 2 && bytes[0] == b'r' && bytes[1].is_ascii_uppercase() {
        return r[1..].to_ascii_uppercase();
    }
    r.to_ascii_uppercase()
}

/// A plain two-terminal resistor (the kind that can be a pull-up).
fn is_resistor(c: &Component) -> bool {
    let r = ref_designator(c);
    let lib = c.lib_id.to_ascii_lowercase();
    let is_r_ref = r.starts_with('R')
        && !r.starts_with("RV")
        && !r.starts_with("RT")
        && !r.starts_with("RN")
        && !r.starts_with("RP")
        && !r.starts_with("RM");
    is_r_ref && connected_pads(c) == 2 && !lib.contains("ferrite") && !lib.contains("inductor")
}

/// A two-terminal capacitor (ref C*, two connected pads), the kind used as a
/// crystal load cap.
fn is_capacitor(c: &Component) -> bool {
    let r = ref_designator(c);
    r.starts_with('C') && !r.starts_with("CN") && !r.starts_with("CON") && connected_pads(c) == 2
}

/// A net is rail-like if its name is a rail, or it structurally behaves like a
/// local supply (a bypass cap to ground sits on it). Used by the I2C check to
/// recognise pull-ups to CAD-auto-named local rails (the round-1 lesson).
fn net_is_raillike(board: &ExtractedBoard, net_id: i64) -> bool {
    if let Some(n) = board.net(net_id) {
        if rail_voltage(&n.name).is_some() {
            return true;
        }
    }
    board.net_members(net_id).iter().any(|(c, _)| {
        is_capacitor(c)
            && c.pins.iter().any(|op| {
                op.net
                    .filter(|id| *id != net_id)
                    .and_then(|id| board.net(id))
                    .map(|on| is_ground(&on.name))
                    .unwrap_or(false)
            })
    })
}

fn is_unconnected_net(name: &str) -> bool {
    name.trim_start_matches('/').starts_with("unconnected-")
}

/// Parse a resistor value string ("330", "1k", "4k7", "2.2k/R0603", "0R") to
/// ohms via the single canonical parser in `hauksbee-models`.
///
/// This used to be a hand-rolled parser that drifted from `value::parse_value`
/// and from net-lint's copy — reading lowercase-`m` milliohms as MEGohms (a 1e9
/// error), rejecting leading-`R` shunt marks ("R47") and inline annotations
/// ("10k 1%"), and missing unicode/SPICE forms. Delegating kills that whole
/// drift class: the canonical parser handles µ/Ω/ohm-sign glyphs, MEG/GIG,
/// milli-`m`, the R/K/M-decimal form, "/footprint" qualifiers, chip-size codes,
/// and trailing tolerance annotations, all in one tested place. Accept only an
/// ohmic magnitude (no unit, or an explicit Ω) so a stray farad/volt value here
/// still reads as "not a resistor".
fn parse_ohms(v: &str) -> Option<f64> {
    hauksbee_models::value::parse_value(v)
        .filter(|p| matches!(p.unit.as_deref(), None | Some("Ω")))
        .map(|p| p.si)
}

/// Parse a capacitor value string to farads. Handles "15p", "18pF", "1n", "0.1uF",
/// "100nF", "22u". Returns `None` for a non-numeric / placeholder value ("TBD",
/// "DNP", "NA").
fn parse_farads(v: &str) -> Option<f64> {
    let s = v.split('/').next().unwrap_or(v).trim().to_ascii_uppercase();
    let s = s.trim_end_matches('F');
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // Unit suffix scales the number. Recognise BOTH micro glyphs: the micro sign
    // (U+00B5, "µ") and the Greek small-letter mu (U+03BC, "μ") — component
    // libraries write "4.7μF" with either, and uppercasing leaves both untouched.
    for (suffix, mult) in [
        ("P", 1e-12),
        ("N", 1e-9),
        ("U", 1e-6),
        ("\u{00b5}", 1e-6),
        ("\u{03bc}", 1e-6),
    ] {
        if let Some(idx) = s.find(suffix) {
            let (a, b) = s.split_at(idx);
            let b = &b[suffix.len()..];
            let a: f64 = a.trim().parse().ok()?;
            if b.is_empty() {
                return Some(a * mult);
            }
            // "4p7" style: suffix as decimal point.
            let frac: f64 = format!("0.{}", b.trim()).parse().ok()?;
            return Some((a + frac) * mult);
        }
    }
    // A bare number with no unit on a crystal load cap is implausibly farads;
    // treat as picofarads only when it is a small integer-ish value, else reject.
    let n: f64 = s.parse().ok()?;
    if (1.0..=100.0).contains(&n) {
        Some(n * 1e-12)
    } else {
        None
    }
}

// ===========================================================================
// Check 1: crystal load capacitance.
// ===========================================================================
//
// Physics. A parallel-resonant crystal is specified for a particular load
// capacitance CL; the board must present that CL across the crystal terminals
// for it to run on frequency. The two external load caps C1, C2 (one per
// terminal to ground) and the parasitic stray Cs combine as
//     CL_board = (C1 * C2) / (C1 + C2) + Cstray
// (the two caps are in series across the crystal because both return to ground;
// the stray adds in parallel). A common rule of thumb folds the IC's own pin
// capacitance into Cstray (~3-5 pF). If CL_board deviates from the crystal's
// spec the oscillation frequency shifts by roughly
//     df/f ~ -C1_motional / (2 * (C0 + CL)^... )
// but the design-rule form we use is simpler and honest: flag only when the
// *load presented* is far enough from the *spec* that the pullability of a
// normal crystal (typically +-20..30 ppm trim range, a few pF of CL slack)
// cannot absorb it, or when caps are absent on a discrete oscillator that needs
// them.
//
// Reach / honesty. The crystal's CL spec is almost never in the netlist: most
// boards put only the frequency in the value field ("12MHz", "8MHz"). We can
// derive CL only from a recognised part-number value (a small datasheet table
// below), and otherwise emit an INFO note carrying the computed CL_board with no
// fire. RTC chips that integrate their own load caps (PCF8523, RV-8263, RV-3028)
// are recognised and never flagged for "missing caps". This keeps the check at
// zero false positives on the corpus, where almost every crystal is value="freq".

/// The stray capacitance assumed per oscillator (pF), folding PCB trace
/// parasitics and the MCU/IC pin capacitance. 4 pF is the textbook midpoint
/// (2-3 pF of short trace + ~1-2 pF pin); the error band is roughly +-3 pF,
/// which is why the firing threshold is set wide (see `CL_TOLERANCE_PF`).
const CSTRAY_PF: f64 = 4.0;

/// CL deviation (pF) beyond which the load is flagged. Set from the physics: a
/// typical MHz crystal trims +-20..30 ppm, which for a CL near 18 pF corresponds
/// to roughly +-4..6 pF of CL error before the frequency leaves the pullable
/// band; combined with the +-3 pF stray-model error, we only fire past 8 pF of
/// deviation so the model uncertainty can never produce the finding on its own.
const CL_TOLERANCE_PF: f64 = 8.0;

/// A crystal whose datasheet CL spec we know, keyed by an uppercase substring of
/// the value / part-number field. Only parts whose datasheet pins the CL are
/// listed; each entry cites its source. Anything not here -> CL unknown -> info.
///
/// `(value-substring, CL_pF, citation)`.
const KNOWN_CRYSTAL_CL: &[(&str, f64, &str)] = &[
    // Abracon ABM8-272 series: the "-272" code is the 18 pF CL option on the
    // ABM8 3.2x2.5 mm SMD crystal (Abracon ABM8 datasheet, CL ordering code
    // table). The RP2040 minimal reference design (Y1 = ABM8-272-T3) uses it
    // with 15 pF caps -> board CL ~ 7.5 + stray, the documented RP2040 hint.
    ("ABM8-272", 18.0, "Abracon ABM8 datasheet, -272 = 18 pF CL"),
    // Abracon ABM8G frequency-stamped parts (used on MNT Reform) are sold in
    // multiple CL options; without the full ordering code the CL is NOT known,
    // so ABM8G alone is deliberately absent here (info, not a guess).
];

/// A ceramic resonator (Murata CSTxE / CERALOCK, ZTT, 3-terminal) integrates its
/// own load caps (the centre pin is the common cap node to ground). It needs no
/// external caps, so a resonator with no external caps is correct, never a
/// finding. Recognised by value family or a "RESONATOR" footprint.
fn is_ceramic_resonator(c: &Component) -> bool {
    let v = c.value.to_ascii_uppercase();
    let fp = c.footprint.to_ascii_uppercase();
    fp.contains("RESONATOR")
        || ["CSTCE", "CSTNE", "CSTLS", "CERALOCK", "ZTTCS", "ZTTCC"]
            .iter()
            .any(|k| v.contains(k))
}

/// RTC / oscillator parts that integrate their own load caps, so external load
/// caps are optional and their absence is never a finding.
fn has_integrated_xtal_caps(value: &str) -> bool {
    let v = value.to_ascii_uppercase();
    [
        "PCF8523",
        "PCF8563",
        "PCF85063",
        "RV-8263",
        "RV8263",
        "RV-3028",
        "RV3028",
        "DS3231",
        "ABRACON AB18",
    ]
    .iter()
    .any(|p| v.contains(p))
}

/// Look up a crystal's datasheet CL (pF) from its value/part-number string.
fn known_crystal_cl(value: &str) -> Option<(f64, &'static str)> {
    let v = value.to_ascii_uppercase();
    KNOWN_CRYSTAL_CL
        .iter()
        .find(|(k, _, _)| v.contains(k))
        .map(|(_, cl, cite)| (*cl, *cite))
}

/// Is this component a discrete crystal/resonator (ref Y*/X*/XTAL, or a crystal
/// footprint/lib)? Excludes connectors (X is sometimes a connector prefix) by
/// requiring a crystal footprint or a 2/4-pin shape.
fn is_crystal(c: &Component) -> bool {
    let r = ref_designator(c);
    let lib = c.lib_id.to_ascii_lowercase();
    let fp = c.footprint.to_ascii_lowercase();
    let crystal_fp = lib.contains("crystal") || fp.contains("crystal") || fp.contains("xtal");
    // Y is the unambiguous crystal designator. X is overloaded (connectors), so
    // an X-ref only counts as a crystal with a crystal footprint.
    if r.starts_with('Y')
        && r.chars()
            .nth(1)
            .map(|c| c.is_ascii_digit())
            .unwrap_or(false)
    {
        return true;
    }
    crystal_fp && connected_pads(c) >= 2 && !r.starts_with('J') && !r.starts_with("CN")
}

/// `CL = (C1 * C2) / (C1 + C2)` series combination, in the caps' own unit.
pub fn cl_series(c1: f64, c2: f64) -> f64 {
    if c1 <= 0.0 || c2 <= 0.0 {
        return 0.0;
    }
    (c1 * c2) / (c1 + c2)
}

/// Board-presented load capacitance (pF): the series combination of the two load
/// caps plus the stray.
pub fn cl_board_pf(c1_pf: f64, c2_pf: f64, cstray_pf: f64) -> f64 {
    cl_series(c1_pf, c2_pf) + cstray_pf
}

/// On a crystal terminal net, find the load cap to ground: a capacitor with one
/// pad on `net` and the other on a ground net. Returns its capacitance in pF.
fn load_cap_on_net(board: &ExtractedBoard, net_id: i64) -> Option<f64> {
    for (c, _p) in board.net_members(net_id) {
        if !is_capacitor(c) {
            continue;
        }
        let to_ground = c.pins.iter().any(|op| {
            op.net
                .filter(|id| *id != net_id)
                .and_then(|id| board.net(id))
                .map(|on| is_ground(&on.name))
                .unwrap_or(false)
        });
        if to_ground {
            if let Some(f) = parse_farads(&c.value) {
                return Some(f * 1e12);
            }
        }
    }
    None
}

/// The two signal nets of a crystal: its non-ground pads. A 2-pin crystal has
/// exactly two signal pads; a 4-pin crystal has two signal pads and two ground
/// pads. We pick the (up to two) distinct non-ground nets the crystal touches.
fn crystal_signal_nets(board: &ExtractedBoard, xtal: &Component) -> Vec<i64> {
    let mut nets: Vec<i64> = Vec::new();
    for p in &xtal.pins {
        let Some(id) = p.net else { continue };
        let is_gnd = board.net(id).map(|n| is_ground(&n.name)).unwrap_or(false);
        if is_gnd {
            continue;
        }
        if !nets.contains(&id) {
            nets.push(id);
        }
    }
    nets
}

fn check_crystal_load_cap(board: &ExtractedBoard, report: &mut SiReport) {
    for xtal in &board.components {
        if xtal.dnp || !is_crystal(xtal) {
            continue;
        }
        // A ceramic resonator carries its own load caps (the 3-terminal centre
        // pin is the integrated cap node to ground); external caps are not
        // required, so it is never flagged for "missing caps". (Arduino Uno Y2
        // CSTCE16M0V53, SparkFun RedBoard Y1 RESONATOR-SMD.)
        if is_ceramic_resonator(xtal) {
            continue;
        }
        let sig_nets = crystal_signal_nets(board, xtal);
        if sig_nets.len() != 2 {
            // A crystal with other than two signal terminals is an RTC module,
            // a placeholder, or something we cannot model. Skip.
            continue;
        }

        // Find a load cap on each terminal. The RP2040 series-resistor topology
        // puts the second cap on a `Net-(Cx-Pad1)` node bridged through the 1k
        // damping resistor; walk one resistor hop to find it if the direct net
        // has no cap.
        let c1 = load_cap_on_net(board, sig_nets[0])
            .or_else(|| load_cap_through_resistor(board, sig_nets[0]));
        let c2 = load_cap_on_net(board, sig_nets[1])
            .or_else(|| load_cap_through_resistor(board, sig_nets[1]));

        // Is the crystal driven by an RTC/IC that integrates its own caps? Then
        // missing external caps is correct, not a finding.
        let driver_integrates = board
            .net_members(sig_nets[0])
            .iter()
            .chain(board.net_members(sig_nets[1]).iter())
            .any(|(c, _)| has_integrated_xtal_caps(&c.value));

        let net_names: Vec<String> = sig_nets
            .iter()
            .filter_map(|id| board.net(*id).map(|n| n.name.clone()))
            .collect();

        match (c1, c2) {
            (Some(c1), Some(c2)) => {
                let cl = cl_board_pf(c1, c2, CSTRAY_PF);
                if let Some((spec, cite)) = known_crystal_cl(&xtal.value) {
                    let dev = (cl - spec).abs();
                    if dev > CL_TOLERANCE_PF {
                        let sev = if cl < spec * 0.5 || cl > spec * 1.6 {
                            SiSeverity::High
                        } else {
                            SiSeverity::Medium
                        };
                        report.findings.push(SiFinding {
                            check: SiCheck::CrystalLoadCap,
                            severity: sev,
                            message: format!(
                                "{} ({}): board presents CL ~ {:.1} pF (C1={:.0}p, C2={:.0}p, +{:.0}p stray) \
                                 but the crystal specs CL = {:.0} pF [{}]; deviation {:.1} pF exceeds {:.0} pF",
                                xtal.reference, xtal.value, cl, c1, c2, CSTRAY_PF, spec, cite, dev,
                                CL_TOLERANCE_PF
                            ),
                            refs: vec![xtal.reference.clone()],
                            nets: net_names.clone(),
                        });
                    } else {
                        report.findings.push(SiFinding {
                            check: SiCheck::CrystalLoadCap,
                            severity: SiSeverity::Info,
                            message: format!(
                                "{} ({}): board CL ~ {:.1} pF vs spec {:.0} pF (within {:.0} pF) - ok",
                                xtal.reference, xtal.value, cl, spec, CL_TOLERANCE_PF
                            ),
                            refs: vec![xtal.reference.clone()],
                            nets: net_names.clone(),
                        });
                    }
                } else {
                    // CL spec unknown: report the computed load as info only.
                    report.findings.push(SiFinding {
                        check: SiCheck::CrystalLoadCap,
                        severity: SiSeverity::Info,
                        message: format!(
                            "{} ({}): board presents CL ~ {:.1} pF (C1={:.0}p, C2={:.0}p, +{:.0}p stray); \
                             crystal CL spec unknown from value, no judgement",
                            xtal.reference, xtal.value, cl, c1, c2, CSTRAY_PF
                        ),
                        refs: vec![xtal.reference.clone()],
                        nets: net_names.clone(),
                    });
                }
            }
            _ => {
                // One or both load caps missing.
                if driver_integrates {
                    continue; // RTC integrates caps: correct, not a finding.
                }
                // A discrete MHz crystal with no load caps either way is a real
                // omission, but only fire when BOTH are absent (a single missing
                // cap is often the series-R topology we failed to trace, so stay
                // conservative and emit info).
                if c1.is_none() && c2.is_none() {
                    report.findings.push(SiFinding {
                        check: SiCheck::CrystalLoadCap,
                        severity: SiSeverity::Medium,
                        message: format!(
                            "{} ({}): discrete crystal with no load capacitors to ground on either \
                             terminal; a parallel-resonant crystal needs two load caps to start on \
                             frequency (unless the driver integrates them)",
                            xtal.reference, xtal.value
                        ),
                        refs: vec![xtal.reference.clone()],
                        nets: net_names,
                    });
                } else {
                    report.findings.push(SiFinding {
                        check: SiCheck::CrystalLoadCap,
                        severity: SiSeverity::Info,
                        message: format!(
                            "{} ({}): only one load cap traced (other terminal cap not found, \
                             possibly behind a series resistor); no judgement",
                            xtal.reference, xtal.value
                        ),
                        refs: vec![xtal.reference.clone()],
                        nets: net_names,
                    });
                }
            }
        }
    }
}

/// Walk one resistor hop from a crystal terminal net to find a load cap (the
/// RP2040 series-damping-resistor topology: XOUT -> R(1k) -> node carrying the
/// second load cap and the crystal's far pad).
fn load_cap_through_resistor(board: &ExtractedBoard, net_id: i64) -> Option<f64> {
    for (c, _p) in board.net_members(net_id) {
        if !is_resistor(c) {
            continue;
        }
        for op in &c.pins {
            if op.net == Some(net_id) {
                continue;
            }
            if let Some(oid) = op.net {
                if let Some(cap) = load_cap_on_net(board, oid) {
                    return Some(cap);
                }
            }
        }
    }
    None
}

// ===========================================================================
// Check 2: I2C rise time (pull-up sufficiency).
// ===========================================================================
//
// Physics. An I2C bus is open-drain: the pull-up resistor charges the bus
// capacitance, and the rising edge time constant gives the 30%->70% rise as
//     t_r = 0.8473 * Rpull * Cbus
// (the 0.8473 factor is ln(0.7/0.3) for an RC charge between the I2C VIL/VIH
// thresholds). The I2C spec caps t_r at 1000 ns (standard mode, 100 kHz) and
// 300 ns (fast mode, 400 kHz). Too-weak a pull-up (R too high) or too much bus
// capacitance (too many devices / long traces) blows the limit, and the bus
// either fails outright or only works slow.
//
// Bus capacitance is the sum of: per-device pin capacitance (datasheet ~10 pF
// default per I2C pin) + trace capacitance (a microstrip over a plane is
// ~0.3-1.0 pF/mm; we use a documented 1.0 pF/mm conservative figure times the
// net's routed length, OR a flat per-device allowance when geometry is absent).
//
// Mode inference is conservative: assume STANDARD mode (1000 ns) unless the net
// name encodes fast mode. So we only ever fire when even the most lenient mode
// is violated, or when the margin is severe - which is exactly what keeps it at
// zero false positives on the proven-good corpus buses (Olimex UEXT 2.2k,
// ZSWatch 1.8k/3.3k), all of which sit far under 1000 ns.

/// I2C rise-time constant: t_r = K_RISE * R * C (ln(0.7/0.3) = 0.8473).
pub const K_RISE: f64 = 0.8473;
/// Standard-mode (100 kHz) rise-time limit (ns).
pub const T_R_STANDARD_NS: f64 = 1000.0;
/// Fast-mode (400 kHz) rise-time limit (ns).
pub const T_R_FAST_NS: f64 = 300.0;
/// Default capacitance per I2C device pin (pF), a common datasheet figure.
const C_PIN_PF: f64 = 10.0;

/// Rise time (ns) for a pull-up R (ohms) charging a bus capacitance C (pF).
pub fn i2c_rise_time_ns(r_ohm: f64, c_pf: f64) -> f64 {
    // t = 0.8473 * R[ohm] * C[F]; with C in pF (1e-12) and t in ns (1e9):
    // 0.8473 * R * (C*1e-12) * 1e9 = 0.8473 * R * C * 1e-3.
    K_RISE * r_ohm * c_pf * 1e-3
}

/// I2C role (SDA/SCL) of a net, by leaf name token. Mirrors the netlint matcher.
fn i2c_role(name: &str) -> Option<&'static str> {
    let n = norm(name);
    let toks: Vec<&str> = n.split(|c: char| !c.is_ascii_alphanumeric()).collect();
    let has = |needle: &str| {
        toks.iter().any(|t| {
            let t = t.strip_prefix('A').unwrap_or(t);
            let t = t.trim_end_matches(|c: char| c.is_ascii_digit());
            t == needle
        })
    };
    if has("SDA") {
        Some("SDA")
    } else if has("SCL") {
        Some("SCL")
    } else {
        None
    }
}

fn is_connector_like(c: &Component) -> bool {
    let r = c.reference.to_ascii_uppercase();
    let lib = c.lib_id.to_ascii_lowercase();
    let fp = c.footprint.to_ascii_lowercase();
    r.starts_with('J')
        || r.starts_with("CN")
        || r.starts_with("CON")
        || r.starts_with("TP")
        || r.starts_with("UEXT")
        || r.starts_with("EXT")
        || lib.contains("connector")
        || lib.contains("header")
        || fp.contains("header")
        || fp.contains("connector")
        || fp.contains("uext")
}

/// Find the effective pull-up resistance on an I2C net: every resistor with one
/// pad on the net and the other on a rail-like node. Multiple pull-ups on one
/// net sit in PARALLEL, so the effective R is the reciprocal of the summed
/// conductances (1/(Σ 1/Rᵢ)) — strictly SMALLER than any single resistor, i.e.
/// a faster bus. Returns ohms, or `None` if the net carries no pull-up.
fn pullup_ohms(board: &ExtractedBoard, net_id: i64) -> Option<f64> {
    let mut conductance = 0.0_f64; // Σ 1/Rᵢ (siemens)
    for (c, _p) in board.net_members(net_id) {
        if !is_resistor(c) {
            continue;
        }
        let to_rail = c.pins.iter().any(|op| {
            op.net
                .filter(|id| *id != net_id)
                .map(|id| net_is_raillike(board, id))
                .unwrap_or(false)
        });
        if to_rail {
            if let Some(r) = parse_ohms(&c.value) {
                if r > 0.0 {
                    conductance += 1.0 / r;
                }
            }
        }
    }
    (conductance > 0.0).then(|| 1.0 / conductance)
}

/// Estimate bus capacitance (pF) on an I2C net from device count plus optional
/// routed trace length. Devices = non-resistor, non-connector parts with a pin
/// on the net (each ~C_PIN_PF). Trace length, when known, adds 1.0 pF/mm.
fn bus_capacitance_pf(
    board: &ExtractedBoard,
    net_id: i64,
    trace_len_mm: Option<f64>,
) -> (f64, usize) {
    let mut devices = 0usize;
    for (c, _p) in board.net_members(net_id) {
        if is_resistor(c) || is_connector_like(c) {
            continue;
        }
        devices += 1;
    }
    let c_dev = devices as f64 * C_PIN_PF;
    let c_trace = trace_len_mm.unwrap_or(0.0) * 1.0; // 1.0 pF/mm, documented.
    (c_dev + c_trace, devices)
}

fn check_i2c_rise_time(board: &ExtractedBoard, report: &mut SiReport) {
    for net in &board.nets {
        if net.id == 0 || is_unconnected_net(&net.name) {
            continue;
        }
        let Some(role) = i2c_role(&net.name) else {
            continue;
        };
        let mem = board.net_members(net.id);
        if mem.len() < 2 {
            continue;
        }
        // Only audit a bus that actually has a pull-up (the presence check is
        // netlint's job; we audit *sufficiency*). No pull -> not our finding.
        let Some(r) = pullup_ohms(board, net.id) else {
            continue;
        };
        // Trace length is not parsed here (it needs the geometry root); the
        // device-count model is the conservative floor. Geometry refinement is a
        // documented future improvement.
        let (c_bus, devices) = bus_capacitance_pf(board, net.id, None);
        if c_bus <= 0.0 {
            continue;
        }
        let t_r = i2c_rise_time_ns(r, c_bus);

        // Conservative: judge against STANDARD mode unless the name says fast.
        let fast = norm(&net.name).contains("FM") || norm(&net.name).contains("FAST");
        let limit = if fast { T_R_FAST_NS } else { T_R_STANDARD_NS };

        if t_r > limit {
            // Even the assumed (lenient) mode fails: a real finding.
            let sev = if t_r > limit * 1.5 {
                SiSeverity::High
            } else {
                SiSeverity::Medium
            };
            report.findings.push(SiFinding {
                check: SiCheck::I2cRiseTime,
                severity: sev,
                message: format!(
                    "I2C {role} '{}': pull-up {:.0} ohm x bus ~{:.0} pF ({} devices) gives t_r ~ {:.0} ns, \
                     over the {} limit {:.0} ns",
                    net.name, r, c_bus, devices, t_r,
                    if fast { "fast-mode" } else { "standard-mode" },
                    limit
                ),
                refs: mem.iter().map(|(c, _)| c.reference.clone()).collect(),
                nets: vec![net.name.clone()],
            });
        } else {
            report.findings.push(SiFinding {
                check: SiCheck::I2cRiseTime,
                severity: SiSeverity::Info,
                message: format!(
                    "I2C {role} '{}': pull-up {:.0} ohm x ~{:.0} pF ({} dev) -> t_r ~ {:.0} ns (< {:.0} ns) - ok",
                    net.name, r, c_bus, devices, t_r, limit
                ),
                refs: vec![],
                nets: vec![net.name.clone()],
            });
        }
    }
}

// ===========================================================================
// Check 3: antenna keepout.
// ===========================================================================
//
// Physics. A PCB-trace antenna (chip antenna, or the integrated antenna of an
// ESP32-WROOM / nRF module) needs a copper-free, ground-free region around and
// beyond it. Ground plane or routed copper inside the keepout detunes the
// antenna and absorbs radiated power: the symptom is poor range / sensitivity
// (the Watchy / Inkplate-6 bad-WiFi class). The module/antenna datasheet
// specifies a keepout rectangle, usually extending past the antenna end by a
// fixed distance across the module's full width.
//
// Method. We locate antenna-bearing parts (recognised modules / chip antennas in
// the table below), project the datasheet keepout rectangle into board
// coordinates using the part's placement (x, y, rotation), and ask whether any
// OTHER net's copper (segments, vias, zone fills, foreign pads) falls inside it.
// Module pads themselves and the antenna's own net are excluded.
//
// Honesty. The keepout geometry is only known for parts in the table (each
// cited). An unknown module -> no keepout -> no fire. A board-edge antenna with
// the keepout hanging off the board is correctly quiet (no copper there). The
// check reports the intrusion area / nearest offending primitive so a fire can
// be chased to the file.

/// A keepout rectangle in the part's local frame (mm), measured from the
/// footprint origin. The antenna sits at one end of the module; the keepout
/// extends from `y_min` to `y_max` (the +y direction is "off the antenna end")
/// and spans `x_min..x_max` (the module width). Coordinates follow KiCad's
/// footprint frame (y down); we rotate by the placement angle.
#[derive(Debug, Clone, Copy)]
struct KeepoutRect {
    x_min: f64,
    x_max: f64,
    y_min: f64,
    y_max: f64,
}

/// Antenna/module keepout table. Each entry is matched by an uppercase substring
/// of the component value OR footprint, and carries the datasheet keepout
/// rectangle in the footprint-local frame. The rectangle is intentionally the
/// *copper-free* zone the datasheet draws; coordinates are relative to the
/// footprint origin as KiCad places it.
///
/// `(match-substring, KeepoutRect, citation)`.
fn antenna_keepout(c: &Component) -> Option<(KeepoutRect, &'static str)> {
    let hay = format!("{} {}", c.value, c.footprint).to_ascii_uppercase();
    // ESP32-WROOM-32 / 32E / 32D / 32U: the module is 18 x 25.5 mm; the PCB
    // antenna occupies the top ~6 mm, and Espressif's hardware design guidelines
    // require a keepout of >= 15 mm beyond the antenna edge, spanning the module
    // width, with NO copper or ground on any layer. We model the footprint origin
    // at the module centre (KiCad's ESP-WROOM footprints place it so), antenna at
    // the -y (top) edge, and the keepout as the band just beyond the antenna.
    // (Espressif ESP32-WROOM-32 datasheet, "Peripheral Schematic / Keepout
    // zone"; ESP-WROOM hardware design guidelines: 15 mm keepout.)
    if hay.contains("WROOM") || hay.contains("ESP-WROOM") || hay.contains("ESP32-WROOM") {
        // The ESP32-WROOM-32 is 18 x 25.5 mm. The PCB antenna is at the "top"
        // end (the edge with no castellated pads); the module's pads occupy the
        // body and stop ~5.3 mm short of the footprint origin on the antenna
        // side (verified empirically against the OLIMEX ESP-WROOM-32 footprint:
        // pad local-y spans [-5.31, +12.30], so the antenna edge is at local
        // y ~ -5.3). Espressif's hardware design guidelines require a >= 15 mm
        // keepout beyond the antenna, module-wide, clear of copper on all layers.
        // We model the band from the antenna edge (local y -5.3) outward by 15 mm
        // (to -20.3), spanning the module half-width (+-9 mm).
        //
        // Calibration note: on the only corpus board carrying this module (Olimex
        // ESP32-EVB), the module is mounted at the board's top edge so this
        // keepout band lies OFF the board (in free space), and the check is
        // correctly silent - the textbook-correct edge placement. The check
        // therefore ships as a calibrated guard that fires only on a genuine
        // copper intrusion under/near the antenna, never on this corpus.
        return Some((
            KeepoutRect {
                x_min: -9.0,
                x_max: 9.0,
                y_min: -20.3,
                y_max: -5.3,
            },
            "Espressif ESP32-WROOM-32 hardware design guidelines: 15 mm antenna keepout",
        ));
    }
    // NOTE: other integrated-antenna modules (u-blox NORA-B1 / NORA-B106 SiP,
    // chip antennas) are deliberately NOT in this table. Their datasheet keepout
    // rectangle and the footprint-origin convention could not be verified to the
    // precision this check needs, and a guessed rectangle on the ZSWatch
    // NORA-B106 produced dozens of spurious intrusions (a textbook false
    // positive). Per the calibration discipline (do not fabricate constants), an
    // unverified module gets no keepout and therefore never fires. Adding one
    // back requires the cited datasheet rectangle plus a placement-verified
    // corpus board to calibrate against.
    None
}

/// Rotate a local-frame point by the placement angle (degrees, KiCad CCW with
/// y-down) and translate to board coordinates. Mirrors `pcb::extract_footprint`.
fn local_to_board(fx: f64, fy: f64, frot_deg: f64, lx: f64, ly: f64) -> (f64, f64) {
    let (sin, cos) = frot_deg.to_radians().sin_cos();
    (fx + lx * cos + ly * sin, fy - lx * sin + ly * cos)
}

/// Board-coordinate polygon (the four rotated corners) of a keepout rect.
fn keepout_polygon(pos: (f64, f64, f64), k: &KeepoutRect) -> [(f64, f64); 4] {
    let (fx, fy, rot) = pos;
    [
        local_to_board(fx, fy, rot, k.x_min, k.y_min),
        local_to_board(fx, fy, rot, k.x_max, k.y_min),
        local_to_board(fx, fy, rot, k.x_max, k.y_max),
        local_to_board(fx, fy, rot, k.x_min, k.y_max),
    ]
}

/// Point-in-convex-polygon test (the keepout is a rotated rectangle, convex).
fn point_in_poly(px: f64, py: f64, poly: &[(f64, f64)]) -> bool {
    // Winding sign test: the point is inside iff it is on the same side of every
    // edge. For a convex CCW/CW polygon all cross products share a sign.
    let n = poly.len();
    let mut pos = false;
    let mut neg = false;
    for i in 0..n {
        let (ax, ay) = poly[i];
        let (bx, by) = poly[(i + 1) % n];
        let cross = (bx - ax) * (py - ay) - (by - ay) * (px - ax);
        if cross > 1e-9 {
            pos = true;
        } else if cross < -1e-9 {
            neg = true;
        }
        if pos && neg {
            return false;
        }
    }
    true
}

/// Axis-aligned bounds of a polygon, for a cheap pre-filter.
fn poly_bounds(poly: &[(f64, f64)]) -> (f64, f64, f64, f64) {
    let mut minx = f64::INFINITY;
    let mut miny = f64::INFINITY;
    let mut maxx = f64::NEG_INFINITY;
    let mut maxy = f64::NEG_INFINITY;
    for &(x, y) in poly {
        minx = minx.min(x);
        miny = miny.min(y);
        maxx = maxx.max(x);
        maxy = maxy.max(y);
    }
    (minx, miny, maxx, maxy)
}

/// A copper point intruding into a keepout, with its net id and a label.
struct Intrusion {
    net: i64,
    x: f64,
    y: f64,
    kind: &'static str,
}

fn check_antenna_keepout(board: &ExtractedBoard, root: &List, report: &mut SiReport) {
    // Net id -> name, for excluding the antenna's own net and naming intrusions.
    let net_name = |id: i64| board.net(id).map(|n| n.name.clone()).unwrap_or_default();

    for ant in &board.components {
        if ant.dnp {
            continue;
        }
        let Some((k, cite)) = antenna_keepout(ant) else {
            continue;
        };
        let Some(pos) = ant.position else { continue };
        let poly = keepout_polygon(pos, &k);
        let poly_v: Vec<(f64, f64)> = poly.to_vec();
        let (bminx, bminy, bmaxx, bmaxy) = poly_bounds(&poly_v);

        // Nets owned by the antenna part itself are not intrusions (its own RF
        // feed / ground pads live at its edge by design).
        let own_nets: std::collections::HashSet<i64> =
            ant.pins.iter().filter_map(|p| p.net).collect();

        // Resolve name-only net refs (KiCad 10 `(net "GND")`) via the board's
        // declarations; without this every track/via/zone on such a board has
        // `arg_i64(0) == None` and is skipped, yielding a confident false all-clear.
        let by_name = net_name_index(root);

        let mut intrusions: Vec<Intrusion> = Vec::new();
        let in_box = |x: f64, y: f64| x >= bminx && x <= bmaxx && y >= bminy && y <= bmaxy;

        // 1. Track segments / arcs: sample their endpoints (a track crossing the
        //    keepout has at least an endpoint near it for typical short routes;
        //    we also test the midpoint to catch a track passing straight through).
        for kw in ["segment", "arc"] {
            for seg in root.find_all(kw) {
                let layer = seg.find_value("layer").unwrap_or_default();
                if !layer.ends_with(".Cu") {
                    continue;
                }
                let Some(id) = elem_net_id(seg, &by_name) else {
                    continue;
                };
                if own_nets.contains(&id) {
                    continue;
                }
                let (Some(start), Some(end)) = (seg.find("start"), seg.find("end")) else {
                    continue;
                };
                let (sx, sy) = (
                    start.arg_f64(0).unwrap_or(0.0),
                    start.arg_f64(1).unwrap_or(0.0),
                );
                let (ex, ey) = (end.arg_f64(0).unwrap_or(0.0), end.arg_f64(1).unwrap_or(0.0));
                let mx = (sx + ex) / 2.0;
                let my = (sy + ey) / 2.0;
                for (x, y) in [(sx, sy), (ex, ey), (mx, my)] {
                    if in_box(x, y) && point_in_poly(x, y, &poly_v) {
                        intrusions.push(Intrusion {
                            net: id,
                            x,
                            y,
                            kind: "track",
                        });
                        break;
                    }
                }
            }
        }

        // 2. Vias.
        for via in root.find_all("via") {
            let Some(id) = elem_net_id(via, &by_name) else {
                continue;
            };
            if own_nets.contains(&id) {
                continue;
            }
            let Some(at) = via.find("at") else { continue };
            let (x, y) = (at.arg_f64(0).unwrap_or(0.0), at.arg_f64(1).unwrap_or(0.0));
            if in_box(x, y) && point_in_poly(x, y, &poly_v) {
                intrusions.push(Intrusion {
                    net: id,
                    x,
                    y,
                    kind: "via",
                });
            }
        }

        // 3. Foreign pads (another component's copper sitting in the keepout).
        for c in &board.components {
            if c.reference == ant.reference {
                continue;
            }
            for p in &c.pins {
                let Some((x, y)) = p.position else { continue };
                let Some(id) = p.net else { continue };
                if own_nets.contains(&id) {
                    continue;
                }
                if in_box(x, y) && point_in_poly(x, y, &poly_v) {
                    intrusions.push(Intrusion {
                        net: id,
                        x,
                        y,
                        kind: "pad",
                    });
                }
            }
        }

        // 4. Zone (ground / power pour) fill polygons crossing the keepout.
        for zone in root.find_all("zone") {
            let Some(id) = elem_net_id(zone, &by_name) else {
                continue;
            };
            if own_nets.contains(&id) {
                continue;
            }
            // On a copper layer?
            let on_cu = zone
                .find("layers")
                .map(|l| {
                    (0..)
                        .map_while(|i| l.arg_value(i))
                        .any(|n| n.ends_with(".Cu"))
                })
                .unwrap_or(false)
                || zone
                    .find_value("layer")
                    .map(|l| l.ends_with(".Cu"))
                    .unwrap_or(false);
            if !on_cu {
                continue;
            }
            // A pour intrudes the keepout in either of two ways: (a) a fill
            // vertex lands inside the keepout (partial overlap), or (b) the pour
            // ENGULFS the keepout — a board-wide ground plane covering the whole
            // antenna region has ALL its fill vertices outside the small keepout
            // rectangle, so vertex-only sampling missed it and reported a false
            // all-clear. Also test each keepout corner against the fill polygon to
            // catch containment (this is the exact bad-WiFi failure the check
            // exists to catch).
            let keepout_corners = [
                (bminx, bminy),
                (bmaxx, bminy),
                (bmaxx, bmaxy),
                (bminx, bmaxy),
            ];
            let mut hit: Option<(f64, f64)> = None;
            for fp in zone.find_all("filled_polygon") {
                if let Some(pts) = fp.find("pts") {
                    let fill: Vec<(f64, f64)> = pts
                        .find_all("xy")
                        .map(|xy| (xy.arg_f64(0).unwrap_or(0.0), xy.arg_f64(1).unwrap_or(0.0)))
                        .collect();
                    // (a) fill vertex inside the keepout.
                    for &(x, y) in &fill {
                        if in_box(x, y) && point_in_poly(x, y, &poly_v) {
                            hit = Some((x, y));
                            break;
                        }
                    }
                    // (b) keepout corner inside the pour (containment / engulf).
                    // A real KiCad pour outline is deeply NON-convex (it weaves
                    // around every via / pad / thermal relief), so the convex
                    // `point_in_poly` winding test returns false for interior
                    // points the moment two edges disagree — silently missing the
                    // engulf it was written to catch. Use the even-odd ray cast,
                    // which is correct for arbitrary (non-convex) polygons.
                    if hit.is_none() && fill.len() >= 3 {
                        for &(kx, ky) in &keepout_corners {
                            if crate::gerber::geo::point_in_polygon(kx, ky, &fill) {
                                hit = Some((kx, ky));
                                break;
                            }
                        }
                    }
                }
                if hit.is_some() {
                    break;
                }
            }
            if let Some((x, y)) = hit {
                intrusions.push(Intrusion {
                    net: id,
                    x,
                    y,
                    kind: "zone",
                });
            }
        }

        if intrusions.is_empty() {
            report.findings.push(SiFinding {
                check: SiCheck::AntennaKeepout,
                severity: SiSeverity::Info,
                message: format!(
                    "{} ({}): antenna keepout [{}] is clear of foreign copper - ok",
                    ant.reference, ant.value, cite
                ),
                refs: vec![ant.reference.clone()],
                nets: vec![],
            });
            continue;
        }

        // Group intrusions by net for a readable message; ground in the keepout
        // is the worst (detunes hardest), so a ground intrusion is High.
        let mut nets: Vec<i64> = intrusions.iter().map(|i| i.net).collect();
        nets.sort_unstable();
        nets.dedup();
        let any_ground = nets.iter().any(|id| is_ground(&net_name(*id)));
        let kinds: std::collections::HashSet<&str> = intrusions.iter().map(|i| i.kind).collect();
        let sev = if any_ground {
            SiSeverity::High
        } else {
            SiSeverity::Medium
        };
        let net_names: Vec<String> = nets.iter().map(|id| net_name(*id)).collect();
        let sample = &intrusions[0];
        report.findings.push(SiFinding {
            check: SiCheck::AntennaKeepout,
            severity: sev,
            message: format!(
                "{} ({}): {} foreign copper intrusion(s) inside the antenna keepout [{}]: \
                 nets {:?}, primitive kinds {:?}, e.g. {} on net '{}' at ({:.2}, {:.2}) mm",
                ant.reference,
                ant.value,
                intrusions.len(),
                cite,
                net_names,
                kinds,
                sample.kind,
                net_name(sample.net),
                sample.x,
                sample.y
            ),
            refs: vec![ant.reference.clone()],
            nets: net_names,
        });
    }
}

// ===========================================================================
// Check 4: USB differential pair skew.
// ===========================================================================
//
// Physics. A USB D+/D- pair must be length-matched so the two edges arrive
// together; intra-pair skew converts differential signal into common-mode,
// eroding the eye. Full-speed (12 Mbps) is very tolerant (the 8.33 ns bit time
// swamps any sane board mismatch; USB-IF guidance is "match within a few mm");
// high-speed (480 Mbps) requires tight matching, commonly <= 1.25 mm (the
// USB-IF / typical 5 ps skew budget). We compute each leg's routed copper length
// per layer (summing segment / arc lengths on the net) and compare.
//
// Honesty. We measure routed discrete-trace length only (vias add a small
// out-of-plane length we approximate as zero - documented). FS skew is reported
// info-level (lenient); HS-class skew over 1.25 mm is a finding, but since we
// cannot always tell FS from HS from the netlist, the *default* is the lenient
// FS limit and we fire only on a gross mismatch, then report width/gap as info.

/// Full-speed intra-pair skew limit (mm) - very lenient; over this is worth a
/// low-severity note, not a hard finding.
pub const USB_SKEW_FS_MM: f64 = 15.0;
/// High-speed intra-pair skew limit (mm) - the tight matching budget.
pub const USB_SKEW_HS_MM: f64 = 1.25;

/// Build a net-name → id index from a board's `(net id "name")` declarations.
/// Lets a track/via/zone that cites its net by NAME (the KiCad-10 name-only form
/// `(net "GND")`) resolve to the numeric id, instead of being silently skipped
/// because `arg_i64(0)` is `None` on a string token. Mirrors the resolver in
/// `trace_current` / `drc::NetResolver`.
fn net_name_index(root: &List) -> HashMap<String, i64> {
    let mut by_name = HashMap::new();
    for n in root.find_all("net") {
        if let (Some(id), Some(name)) = (n.arg_i64(0), n.arg_value(1)) {
            by_name.entry(name).or_insert(id);
        }
    }
    by_name
}

/// The net id a `(net ...)` reference on an element resolves to: the numeric id
/// when present, else the id of the named net from `by_name`. Returns `None`
/// only when neither is available.
fn elem_net_id(elem: &List, by_name: &HashMap<String, i64>) -> Option<i64> {
    let net = elem.find("net")?;
    if let Some(id) = net.arg_i64(0) {
        return Some(id);
    }
    by_name.get(&net.arg_value(0)?).copied()
}

/// Sum of discrete-trace (segment + arc) copper length on a net, in mm.
pub fn routed_length_mm(root: &List, net_id: i64) -> f64 {
    let by_name = net_name_index(root);
    let mut total = 0.0;
    for seg in root.find_all("segment") {
        if elem_net_id(seg, &by_name) != Some(net_id) {
            continue;
        }
        let (Some(s), Some(e)) = (seg.find("start"), seg.find("end")) else {
            continue;
        };
        let (sx, sy) = (s.arg_f64(0).unwrap_or(0.0), s.arg_f64(1).unwrap_or(0.0));
        let (ex, ey) = (e.arg_f64(0).unwrap_or(0.0), e.arg_f64(1).unwrap_or(0.0));
        total += ((ex - sx).powi(2) + (ey - sy).powi(2)).sqrt();
    }
    // Arcs: approximate by the chord plus a small bulge is overkill; use the
    // straight chord between start and end (KiCad arcs on a diff pair are gentle
    // and the chord under-estimates by < a few percent, which is well inside the
    // skew tolerance and documented).
    for arc in root.find_all("arc") {
        if elem_net_id(arc, &by_name) != Some(net_id) {
            continue;
        }
        let (Some(s), Some(e)) = (arc.find("start"), arc.find("end")) else {
            continue;
        };
        let (sx, sy) = (s.arg_f64(0).unwrap_or(0.0), s.arg_f64(1).unwrap_or(0.0));
        let (ex, ey) = (e.arg_f64(0).unwrap_or(0.0), e.arg_f64(1).unwrap_or(0.0));
        total += ((ex - sx).powi(2) + (ey - sy).powi(2)).sqrt();
    }
    total
}

/// Narrowest and widest discrete-track width on a net (mm), for the width/gap
/// consistency note.
fn track_width_range(root: &List, net_id: i64) -> Option<(f64, f64)> {
    let by_name = net_name_index(root);
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    for kw in ["segment", "arc"] {
        for seg in root.find_all(kw) {
            if elem_net_id(seg, &by_name) != Some(net_id) {
                continue;
            }
            let w = seg.find_f64("width").unwrap_or(0.0);
            if w > 0.0 {
                min = min.min(w);
                max = max.max(w);
            }
        }
    }
    if min.is_finite() {
        Some((min, max))
    } else {
        None
    }
}

/// Find USB D+/D- pairs by net name. Returns (plus_net_id, minus_net_id,
/// base_label) for each pair on the board.
///
/// A pair is only the two polarity legs of the *same logical net*: they must
/// share an identical scope key (the hierarchical sheet path PLUS the leaf stem
/// with the polarity token removed) and differ only in polarity. This is what
/// keeps the matcher from pairing two electrically-distinct nets that merely
/// look USB-ish — in particular the connector-side and MCU-side legs on opposite
/// sides of a series device (an ESD array, common-mode choke, or series R), which
/// KiCad gives different names (different sheet path and/or different stem). See
/// the regression note in `usb_pair_key` and
/// `tests::usb_pair_key_scopes_by_sheet_and_stem`.
fn usb_pairs(board: &ExtractedBoard) -> Vec<(i64, i64, String)> {
    // Index nets by their full scope key (sheet path + stem) with the polarity
    // stripped. Two legs pair only when this key is identical, so DP and DN (or
    // D+ and D-, USB_D+ / USB_D-) under the *same* scope pair up, while two
    // distinct nodes that happen to both be USB-ish (across a series device) get
    // different keys and never pair.
    let mut plus: HashMap<String, i64> = HashMap::new();
    let mut minus: HashMap<String, i64> = HashMap::new();
    for net in &board.nets {
        if net.id == 0 || is_unconnected_net(&net.name) {
            continue;
        }
        // Identify a USB data line, its polarity, and its full scope key. The key
        // is derived from the RAW net name (not `norm`), so the hierarchical sheet
        // path is preserved and two different sheets cannot collide.
        let Some((key, pol)) = usb_pair_key(&net.name) else {
            continue;
        };
        match pol {
            '+' => {
                plus.entry(key).or_insert(net.id);
            }
            '-' => {
                minus.entry(key).or_insert(net.id);
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    for (key, pid) in &plus {
        if let Some(mid) = minus.get(key) {
            out.push((*pid, *mid, key.clone()));
        }
    }
    out.sort_by_key(|(p, _, _)| *p);
    out
}

/// The polarity-stripped scope key + polarity of a USB data net, or `None` if the
/// net is not a USB data line.
///
/// The key is `<sheet-path>\u{1}<leaf-stem>`, both uppercased: the hierarchical
/// path up to and including the final `/` (KiCad's sheet scope) joined to the leaf
/// name with the recognised polarity token removed. Two nets pair iff their keys
/// are identical and their polarities are opposite — i.e. they are the `+`/`-`
/// legs of the *same* logical signal in the *same* sheet scope.
///
/// Why the scope, not just the stem: across a series device (ESD array,
/// common-mode choke, series resistor) the two sides are electrically distinct
/// nodes with distinct names. On the klp5e-esp32 board the connector side is
/// `/USB_DP` + `/USB_DN` (root sheet `/`, stem `USB_`) and the MCU side is
/// `/ESP32-C3-02/USB_D+` + `/ESP32-C3-02/USB_D-` (sheet `/ESP32-C3-02/`, stem
/// `USB_`). Keying on sheet path + stem keeps those two real pairs separate, so
/// the matcher never compares geometry across the ESD device. The previous
/// implementation collapsed every USB-ish net to the constant base `"USB"`, which
/// paired `/USB_DP` with `/ESP32-C3-02/USB_D-` across the ESD array and reported a
/// bogus width/spacing/skew mismatch — the false positive this guards against.
fn usb_pair_key(raw_name: &str) -> Option<(String, char)> {
    let trimmed = raw_name.trim();
    // Split the raw name into the sheet path (up to and including the final '/')
    // and the leaf. A leading-'/'-only name keeps that '/' as its scope so two
    // root-sheet nets still share a scope, while a name under a sub-sheet keeps
    // the distinguishing sub-sheet path.
    let (sheet, leaf) = match trimmed.rfind('/') {
        Some(idx) => (&trimmed[..=idx], &trimmed[idx + 1..]),
        None => ("", trimmed),
    };
    let leaf = leaf.trim().to_ascii_uppercase();
    let (stem, pol) = usb_polarity(&leaf)?;
    Some((format!("{}\u{1}{}", sheet.to_ascii_uppercase(), stem), pol))
}

/// Classify an uppercased leaf net name as a USB data line: return
/// (leaf-stem-with-polarity-removed, polarity). The stem preserves whatever
/// distinguishes one logical USB net from another within a sheet (e.g. `USB1_` vs
/// `USB2_`), so two legs only pair when their stems also match.
///
/// Recognises D+/D-, DP/DN, DM, USB_D+/USB_D-, UD+/UD- (MNT), USBDP/USBDM,
/// *_DP/_DN, D_P/D_N. `None` when the leaf is not a USB data line.
fn usb_polarity(n: &str) -> Option<(String, char)> {
    // Longest suffixes first so e.g. "DPLUS" is not shadowed by "DP".
    let candidates: [(&str, char); 9] = [
        ("DPLUS", '+'),
        ("DMINUS", '-'),
        ("D_P", '+'),
        ("D_N", '-'),
        ("D+", '+'),
        ("D-", '-'),
        ("DP", '+'),
        ("DM", '-'),
        ("DN", '-'),
    ];
    for (suf, pol) in candidates {
        if n == suf || n.ends_with(&format!("_{suf}")) || n.ends_with(suf) {
            // Guard: the char before the matched 'D...' must not be a letter that
            // makes it a different signal (e.g. "LED-", "VDD"). Require the
            // preceding context to be USB-ish or a boundary.
            let prefix = &n[..n.len() - suf.len()];
            let usbish = prefix.is_empty()
                || prefix.ends_with('U')   // UD+, USBD+
                || prefix.contains("USB")
                || prefix.ends_with('_')
                || prefix.ends_with('P')   // PD+ on MNT (peripheral USB)
                || prefix.ends_with('-')   // Net-(U3-USB-DP) style
                || prefix.ends_with("HS")
                || prefix.ends_with("FS");
            if usbish {
                // The stem is the prefix (everything before the polarity token).
                // It is what must match between the + and - legs, in addition to
                // the sheet scope, for them to be the same logical pair.
                return Some((prefix.to_string(), pol));
            }
        }
    }
    None
}

fn check_usb_diff_pair(board: &ExtractedBoard, root: &List, report: &mut SiReport) {
    for (pid, mid, _base) in usb_pairs(board) {
        let lp = routed_length_mm(root, pid);
        let lm = routed_length_mm(root, mid);
        // Both legs must be actually routed to compare lengths.
        if lp <= 0.0 || lm <= 0.0 {
            continue;
        }
        let skew = (lp - lm).abs();
        let pname = board.net(pid).map(|n| n.name.clone()).unwrap_or_default();
        let mname = board.net(mid).map(|n| n.name.clone()).unwrap_or_default();

        // Width / gap consistency: legs should share a width. A width mismatch is
        // an info-level note (impedance discontinuity), never a hard finding.
        let wp = track_width_range(root, pid);
        let wm = track_width_range(root, mid);
        let width_note = match (wp, wm) {
            (Some((minp, maxp)), Some((minm, maxm))) => {
                let inconsistent = (minp - minm).abs() > 0.02 || (maxp - maxm).abs() > 0.02;
                if inconsistent {
                    format!(
                        "; width mismatch D+={:.3}..{:.3} mm vs D-={:.3}..{:.3} mm",
                        minp, maxp, minm, maxm
                    )
                } else {
                    String::new()
                }
            }
            _ => String::new(),
        };

        // We default to the lenient FS limit (we cannot always tell FS from HS).
        // Fire only on a gross skew over the FS limit; otherwise report info with
        // the measured skew (and the HS limit for reference), plus any width note.
        if skew > USB_SKEW_FS_MM {
            report.findings.push(SiFinding {
                check: SiCheck::UsbDiffPair,
                severity: SiSeverity::Medium,
                message: format!(
                    "USB pair {} / {}: routed lengths {:.1} / {:.1} mm, intra-pair skew {:.1} mm \
                     exceeds even the lenient full-speed budget {:.0} mm{}",
                    pname, mname, lp, lm, skew, USB_SKEW_FS_MM, width_note
                ),
                refs: vec![],
                nets: vec![pname, mname],
            });
        } else {
            // Within the skew budget. The width note (if any) is INFO, not a
            // finding: a diff pair necking down at its connector/IC pad entry is
            // universal and benign (the ZSWatch DevKit's D- tapers 0.170 -> 0.127
            // mm at the USB-C pads, like essentially every board), so it must not
            // be a fire. Only the measured skew vs the HS budget is worth noting.
            report.findings.push(SiFinding {
                check: SiCheck::UsbDiffPair,
                severity: SiSeverity::Info,
                message: format!(
                    "USB pair {} / {}: lengths {:.1} / {:.1} mm, skew {:.2} mm (FS limit {:.0} mm, \
                     HS limit {:.2} mm){}",
                    pname, mname, lp, lm, skew, USB_SKEW_FS_MM, USB_SKEW_HS_MM, width_note
                ),
                refs: vec![],
                nets: vec![pname, mname],
            });
        }
    }
}

// ===========================================================================
// Rendering.
// ===========================================================================

/// Render an SI report. Findings first, then info notes, so the surface reads
/// like `--lint`.
pub fn render_si(report: &SiReport) -> String {
    let mut out = String::new();
    let n = report.finding_count();
    if n == 0 {
        out.push_str("si-checks: no findings.\n");
    } else {
        out.push_str(&format!("si-checks: {n} finding(s)\n"));
        for f in report.findings_only() {
            out.push_str(&format!(
                "  [{}] {} - {}\n",
                f.severity.as_str(),
                f.check.as_str(),
                f.message
            ));
        }
    }
    // Info notes (the auditable computed values) after the findings.
    let infos: Vec<&SiFinding> = report
        .findings
        .iter()
        .filter(|f| f.severity == SiSeverity::Info)
        .collect();
    if !infos.is_empty() {
        out.push_str(&format!("si-checks: {} info note(s)\n", infos.len()));
        for f in infos {
            out.push_str(&format!("  [info] {} - {}\n", f.check.as_str(), f.message));
        }
    }
    out
}

#[cfg(test)]
mod tests;
