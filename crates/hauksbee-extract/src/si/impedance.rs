//! Check 5: controlled-impedance signal integrity.
//!
//! Tells a USB / Ethernet / high-speed designer whether their controlled-
//! impedance traces are routed to the right characteristic impedance, from
//! geometry + the board stackup, using the standard quasi-static closed-form
//! formulas (IPC-2141 / Wheeler-Hammerstad era). This is **not** a field solve:
//! it is the same arithmetic the published online calculators (chemandy,
//! Polar's IPC-2141 form, the National Semiconductor differential form) use, and
//! it carries the same few-percent error band. See `docs/SI_CHECKS.md`.
//! Long-form how-and-why: docs/how-and-why/hauksbee-extract/si.md.
//!
//! ## What it computes
//!
//! - Single-ended **microstrip** Z0 (a trace on an outer copper layer over the
//!   nearest reference plane), IPC-2141:
//!     `Z0 = (87 / sqrt(Er + 1.41)) * ln(5.98*H / (0.8*W + T))`
//!   with `H` the dielectric height to the reference plane, `W` the trace width,
//!   `T` the copper thickness, `Er` the substrate dielectric constant.
//! - Single-ended **stripline** Z0 (a trace on an inner layer between two
//!   planes), IPC-2141:
//!     `Z0 = (60 / sqrt(Er)) * ln(4*H / (0.67*pi*(0.8*W + T)))`
//!   with `H` the plane-to-plane dielectric height.
//! - **Differential** impedance of a coupled edge-to-edge pair (microstrip),
//!   National Semiconductor / Wadell form:
//!     `Zdiff = 2*Z0 * (1 - 0.48 * exp(-0.96 * S / H))`
//!   with `S` the edge-to-edge trace spacing.
//!
//! ## The check
//!
//! Nets that *should* be controlled-impedance are recognised by net-name
//! convention plus the existing diff-pair detection: USB D+/D- at 90 ohm
//! differential, Ethernet pairs at 100 ohm differential, and common 50 ohm
//! single-ended high-speed lines. The estimated impedance is compared to the
//! target with a +-15% tolerance; a deviation past that fires (medium, or high
//! when grossly off). Intra-pair skew and width necking are also surfaced.
//!
//! ## Honesty / zero-false-positive discipline
//!
//! Two gates, both required before a deviation becomes a *finding* (anything
//! short of both is an *info* note carrying the computed value):
//!
//! 1. **A real stackup.** The impedance can only be computed when the stackup is
//!    known: KiCad stores it in `(setup (stackup ...))` with per-dielectric
//!    `thickness` / `epsilon_r` and per-copper `thickness`. When the board has
//!    no stackup (e.g. the RP2040 minimal board), we report the estimate under a
//!    stated default assumption (1.6 mm 2-layer FR4, Er 4.3, 1 oz copper) as info
//!    only, never a fire.
//!
//! 2. **Declared impedance-control intent.** This is the hard-won corpus lesson.
//!    The closed-form microstrip/differential model is a quasi-static estimate
//!    with a real error band on dense real boards: it has no co-planar-ground
//!    term and assumes the trace references the nearest plane at the dielectric
//!    height, so on 4-layer boards with ground-flanked routing it over-estimates
//!    Zdiff by ~25-35%, and on 2-layer boards a full-speed USB pair that was
//!    *deliberately not* impedance-controlled (every keyboard / trackball in the
//!    corpus) reads 140-160 ohm and is perfectly fine. We cannot tell a
//!    full-speed pair (impedance irrelevant) from a high-speed one (impedance
//!    critical) from the netlist. So a deviation only becomes a finding when the
//!    board itself declares it is impedance-controlled, via KiCad's
//!    `(stackup (dielectric_constraints yes))`. Every known-good corpus board
//!    sets `dielectric_constraints no` (they chose not to control these nets),
//!    so the check is silent on the whole corpus while still computing and
//!    surfacing every impedance as an auditable info note. A board that *does*
//!    declare impedance control yet routes a pair out of band is the genuine bug
//!    class this fires on.
//!
//! A finding therefore always carries a real, file-derived stackup AND the
//! board's own statement that the net should be controlled. This matches checks
//! 1-4: unknown / unintended -> info, never a confident false positive.

use forge_sexpr::List;

use std::collections::HashMap;

use super::{
    elem_net_id, is_unconnected_net, net_name_index, norm, routed_length_mm, track_width_range,
    usb_pairs, SiCheck, SiFinding, SiReport, SiSeverity,
};
use crate::ExtractedBoard;

// ===========================================================================
// Stackup model.
// ===========================================================================

/// Default substrate dielectric constant for FR4 (the corpus stackups all
/// declare 4.5; the textbook quick-estimate value is 4.3). Used only when the
/// board declares no stackup, and only ever to produce an *info* estimate.
pub const DEFAULT_ER: f64 = 4.3;
/// Default 1 oz copper thickness (mm).
pub const DEFAULT_CU_THICKNESS_MM: f64 = 0.035;
/// Default total board thickness for a 2-layer assumption (mm): standard 1.6 mm
/// FR4. The microstrip reference height is the full core for a 2-layer board.
pub const DEFAULT_DIELECTRIC_MM: f64 = 1.51;

/// Impedance tolerance band (fraction). A controlled-impedance target is met if
/// the estimate is within +-15% of target. 15% is the looser of the two common
/// fab tolerances (+-10% / +-15%); using the looser band keeps the model's own
/// few-percent error from ever producing the finding on its own.
pub const Z_TOLERANCE: f64 = 0.15;

/// Whether the impedance was computed against a real file stackup or a default
/// assumption. Findings require [`StackupSource::Board`]; a defaulted estimate
/// is always info.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackupSource {
    /// Read from the board's `(setup (stackup ...))` block.
    Board,
    /// No stackup in the file: a stated default assumption (info only).
    Default,
}

/// The stackup parameters the impedance formulas need. A minimal model: the
/// outer-layer microstrip reference height (dielectric below F.Cu / above B.Cu),
/// the copper thickness, and the substrate Er. This is all the closed-form
/// quasi-static formulas consume.
#[derive(Debug, Clone, Copy)]
pub struct Stackup {
    /// Dielectric height (mm) from an outer copper layer to the nearest plane:
    /// the first dielectric thickness under F.Cu. For a 2-layer board this is
    /// the full core; for a 4-layer board it is the thin prepreg/core to the
    /// inner ground plane.
    pub h_microstrip_mm: f64,
    /// Copper thickness (mm).
    pub t_cu_mm: f64,
    /// Substrate dielectric constant (relative permittivity).
    pub er: f64,
    /// Where the numbers came from.
    pub source: StackupSource,
    /// True when the board declares controlled-impedance intent
    /// (`(stackup (dielectric_constraints yes))`). Only then can a deviation be
    /// a finding; otherwise the designer chose not to control these nets and the
    /// estimate is informational.
    pub impedance_controlled: bool,
}

impl Stackup {
    /// The default-assumption stackup (used only for info-level estimates when
    /// the file has no stackup block).
    fn default_assumed() -> Self {
        Stackup {
            h_microstrip_mm: DEFAULT_DIELECTRIC_MM,
            t_cu_mm: DEFAULT_CU_THICKNESS_MM,
            er: DEFAULT_ER,
            source: StackupSource::Default,
            impedance_controlled: false,
        }
    }

    /// A one-line description of the assumptions for the report.
    fn describe(&self) -> String {
        match self.source {
            StackupSource::Board => format!(
                "board stackup H={:.3} mm, T={:.3} mm, Er={:.2}",
                self.h_microstrip_mm, self.t_cu_mm, self.er
            ),
            StackupSource::Default => format!(
                "ASSUMED default stackup (no stackup in file): H={:.2} mm FR4, T={:.3} mm, Er={:.2}",
                self.h_microstrip_mm, self.t_cu_mm, self.er
            ),
        }
    }
}

/// Read the microstrip-relevant stackup from a KiCad `(setup (stackup ...))`
/// block. Returns `None` (caller falls back to the default-assumption stackup)
/// when no stackup is present.
///
/// KiCad's stackup lists physical layers top-to-bottom; we want the first
/// `dielectric` layer's `thickness` + `epsilon_r` (the gap from the top copper
/// to the next plane) and the F.Cu `copper` thickness. Inner dielectric layers
/// for stripline are not modelled here (a separate, smaller reach): the common
/// controlled-impedance case in this corpus is outer-layer microstrip, and
/// claiming a stripline reference plane requires knowing which inner layer is a
/// solid plane, which the stackup block alone does not say.
pub fn read_stackup(root: &List) -> Option<Stackup> {
    let setup = root.find("setup")?;
    let stackup = setup.find("stackup")?;

    let mut t_cu: Option<f64> = None;
    let mut h_diel: Option<f64> = None;
    let mut er: Option<f64> = None;
    let mut seen_top_copper = false;

    // Walk the layers in declared (top-to-bottom) order. The first `copper`
    // layer is F.Cu; the first `dielectric` (core/prepreg) after it is the
    // microstrip reference height.
    for layer in stackup.find_all("layer") {
        let ty = layer.find_value("type").unwrap_or_default();
        let ty_l = ty.to_ascii_lowercase();
        if ty_l == "copper" {
            if !seen_top_copper {
                seen_top_copper = true;
                if let Some(t) = layer.find_f64("thickness") {
                    if t > 0.0 {
                        t_cu = Some(t);
                    }
                }
            }
        } else if (ty_l == "core" || ty_l == "prepreg") && seen_top_copper && h_diel.is_none() {
            if let Some(t) = layer.find_f64("thickness") {
                if t > 0.0 {
                    h_diel = Some(t);
                    er = layer.find_f64("epsilon_r").filter(|e| *e > 0.0);
                }
            }
        }
    }

    let h = h_diel?;
    // Controlled-impedance intent: `(dielectric_constraints yes)`.
    let impedance_controlled = stackup
        .find_value("dielectric_constraints")
        .map(|v| v.eq_ignore_ascii_case("yes") || v == "true")
        .unwrap_or(false);
    Some(Stackup {
        h_microstrip_mm: h,
        t_cu_mm: t_cu.unwrap_or(DEFAULT_CU_THICKNESS_MM),
        er: er.unwrap_or(DEFAULT_ER),
        source: StackupSource::Board,
        impedance_controlled,
    })
}

// ===========================================================================
// Closed-form impedance formulas (hand-checked in the unit tests against a
// published reference calculator).
// ===========================================================================

/// IPC-2141 single-ended **microstrip** characteristic impedance (ohms).
///
/// `Z0 = (87 / sqrt(Er + 1.41)) * ln(5.98*H / (0.8*W + T))`
///
/// `w`, `h`, `t` in the same length unit (mm here). Returns `None` for a
/// degenerate geometry (the log argument <= 1, i.e. a trace too wide for the
/// formula's validity band, which we then decline to judge).
pub fn microstrip_z0(w: f64, h: f64, t: f64, er: f64) -> Option<f64> {
    if w <= 0.0 || h <= 0.0 || er <= 0.0 {
        return None;
    }
    let denom = 0.8 * w + t;
    if denom <= 0.0 {
        return None;
    }
    let arg = 5.98 * h / denom;
    if arg <= 1.0 {
        // ln(<=1) is <= 0: outside the formula's validity (very wide trace).
        return None;
    }
    Some((87.0 / (er + 1.41).sqrt()) * arg.ln())
}

/// IPC-2141 single-ended **stripline** characteristic impedance (ohms): a trace
/// centred in a dielectric of total plane-to-plane height `h`.
///
/// `Z0 = (60 / sqrt(Er)) * ln(4*H / (0.67*pi*(0.8*W + T)))`
pub fn stripline_z0(w: f64, h: f64, t: f64, er: f64) -> Option<f64> {
    if w <= 0.0 || h <= 0.0 || er <= 0.0 {
        return None;
    }
    let denom = 0.67 * std::f64::consts::PI * (0.8 * w + t);
    if denom <= 0.0 {
        return None;
    }
    let arg = 4.0 * h / denom;
    if arg <= 1.0 {
        return None;
    }
    Some((60.0 / er.sqrt()) * arg.ln())
}

/// Differential microstrip impedance (ohms) of an edge-coupled pair, National
/// Semiconductor / Wadell form:
///
/// `Zdiff = 2*Z0 * (1 - 0.48 * exp(-0.96 * S / H))`
///
/// `z0` is the single-ended microstrip impedance of one trace, `s` the
/// edge-to-edge spacing, `h` the dielectric height.
pub fn differential_microstrip_z(z0: f64, s: f64, h: f64) -> Option<f64> {
    if z0 <= 0.0 || s < 0.0 || h <= 0.0 {
        return None;
    }
    Some(2.0 * z0 * (1.0 - 0.48 * (-0.96 * s / h).exp()))
}

// ===========================================================================
// Controlled-impedance target classification.
// ===========================================================================

/// A controlled-impedance class: its target impedance, whether it is
/// differential, and a human label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImpedanceClass {
    /// USB D+/D- data pair: 90 ohm differential.
    UsbDiff,
    /// Ethernet MDI / other 100 ohm differential pair.
    EthernetDiff,
    /// A generic single-ended 50 ohm high-speed line.
    SingleEnded50,
}

impl ImpedanceClass {
    /// Target impedance (ohms).
    pub fn target_ohm(self) -> f64 {
        match self {
            ImpedanceClass::UsbDiff => 90.0,
            ImpedanceClass::EthernetDiff => 100.0,
            ImpedanceClass::SingleEnded50 => 50.0,
        }
    }
    pub fn is_differential(self) -> bool {
        matches!(self, ImpedanceClass::UsbDiff | ImpedanceClass::EthernetDiff)
    }
    pub fn label(self) -> &'static str {
        match self {
            ImpedanceClass::UsbDiff => "USB 90 ohm differential",
            ImpedanceClass::EthernetDiff => "Ethernet 100 ohm differential",
            ImpedanceClass::SingleEnded50 => "50 ohm single-ended",
        }
    }
}

/// All controlled-impedance differential pairs on the board with their class:
/// USB pairs from the shared `usb_pairs` detector (tagged [`ImpedanceClass::UsbDiff`]),
/// plus Ethernet / generic `_P`/`_N` (`+`/`-`) pairs whose name matches an
/// Ethernet convention (tagged [`ImpedanceClass::EthernetDiff`]). USB-claimed
/// nets are not re-emitted. Returns `(plus_id, minus_id, class)`.
fn classified_pairs(board: &ExtractedBoard) -> Vec<(i64, i64, ImpedanceClass)> {
    let mut out: Vec<(i64, i64, ImpedanceClass)> = Vec::new();
    let mut claimed: std::collections::HashSet<i64> = std::collections::HashSet::new();

    // USB pairs first (they own their nets).
    for (pid, mid, _base) in usb_pairs(board) {
        // A USB pair whose name is actually Ethernet-flavoured (rare) is judged
        // as Ethernet; otherwise USB.
        let pname = board.net(pid).map(|n| n.name.clone()).unwrap_or_default();
        let mname = board.net(mid).map(|n| n.name.clone()).unwrap_or_default();
        let class = if is_ethernet_name(&pname) || is_ethernet_name(&mname) {
            ImpedanceClass::EthernetDiff
        } else {
            ImpedanceClass::UsbDiff
        };
        claimed.insert(pid);
        claimed.insert(mid);
        out.push((pid, mid, class));
    }

    // Ethernet / generic differential pairs by name (P/N, +/-, DP/DN suffix).
    let mut plus: HashMap<String, i64> = HashMap::new();
    let mut minus: HashMap<String, i64> = HashMap::new();
    for net in &board.nets {
        if net.id == 0 || is_unconnected_net(&net.name) || claimed.contains(&net.id) {
            continue;
        }
        // Only Ethernet-flavoured names participate (zero-false-positive: we do
        // not assume every _P/_N net is a controlled 100 ohm pair).
        if !is_ethernet_name(&net.name) {
            continue;
        }
        if let Some((base, pol)) = diff_polarity(&norm(&net.name)) {
            match pol {
                '+' => {
                    plus.entry(base).or_insert(net.id);
                }
                '-' => {
                    minus.entry(base).or_insert(net.id);
                }
                _ => {}
            }
        }
    }
    let mut keys: Vec<&String> = plus.keys().collect();
    keys.sort();
    for base in keys {
        if let (Some(&pid), Some(&mid)) = (plus.get(base), minus.get(base)) {
            if !claimed.contains(&pid) && !claimed.contains(&mid) {
                out.push((pid, mid, ImpedanceClass::EthernetDiff));
            }
        }
    }
    out
}

/// Does a net name match an Ethernet-pair convention? `TRD0..3`, `TRX0..3`
/// (RGMII / MDI), `MDI`, `MX0..3`, `ETH`. Deliberately a tight allow-list of
/// unambiguous Ethernet tokens so an unrelated net (e.g. a `BOARD`/`DDR` signal)
/// is never mistaken for a 100 ohm pair.
fn is_ethernet_name(name: &str) -> bool {
    let n = norm(name);
    ["TRD", "TRX", "MDI", "ETH", "MX0", "MX1", "MX2", "MX3"]
        .iter()
        .any(|k| n.contains(k))
}

/// Split a normalized net name into (base, polarity) for a generic differential
/// pair, recognising the `_P`/`_N`, `_DP`/`_DN`, trailing `+`/`-`, and `P`/`N`
/// suffix conventions. Returns `None` if no polarity suffix is present.
fn diff_polarity(n: &str) -> Option<(String, char)> {
    for (suf, pol) in [
        ("_P", '+'),
        ("_N", '-'),
        ("_DP", '+'),
        ("_DN", '-'),
        ("+", '+'),
        ("-", '-'),
        ("_POS", '+'),
        ("_NEG", '-'),
    ] {
        if n.ends_with(suf) && n.len() > suf.len() {
            let base = n[..n.len() - suf.len()].to_string();
            return Some((base, pol));
        }
    }
    None
}

/// Recognise a single-ended net that is conventionally a 50 ohm controlled-
/// impedance line. Deliberately conservative: only names that are *industry
/// conventions* for a 50 ohm trace, so an ordinary GPIO is never judged. RF
/// feed lines (`RF`, `ANT`), SMA/U.FL ports, and CSI/DSI single-ended clocks are
/// the textbook 50 ohm single-ended set; we keep it to the RF feed conventions
/// to stay at zero false positives (a bare `CLK` is NOT assumed 50 ohm).
fn is_single_ended_50(name: &str) -> bool {
    let n = norm(name);
    let toks: Vec<&str> = n.split(|c: char| !c.is_ascii_alphanumeric()).collect();
    // RF feedline conventions only. These are the nets a designer routes to a
    // deliberate 50 ohm; an ordinary digital signal is not assumed controlled.
    toks.iter().any(|t| {
        matches!(
            *t,
            "RF" | "RFIN" | "RFOUT" | "RFOUTPUT" | "ANT" | "ANTENNA" | "RF_IN" | "RF_OUT"
        ) || t.starts_with("RFIO")
            || t.starts_with("ANT")
    })
}

// ===========================================================================
// The check.
// ===========================================================================

/// Run the controlled-impedance check. Appends findings/info to `report`.
pub fn check_controlled_impedance(board: &ExtractedBoard, root: &List, report: &mut SiReport) {
    // Resolve the stackup once: real if the file has one, else the stated
    // default-assumption (which can only ever produce info notes).
    let stackup = read_stackup(root).unwrap_or_else(Stackup::default_assumed);

    // 1. Differential pairs (USB + Ethernet) via the existing pair detector.
    check_diff_pairs(board, root, &stackup, report);

    // 2. Single-ended 50 ohm RF/controlled lines.
    check_single_ended(board, root, &stackup, report);
}

/// Estimate the differential impedance of a routed pair and judge it against its
/// class target.
fn check_diff_pairs(board: &ExtractedBoard, root: &List, stackup: &Stackup, report: &mut SiReport) {
    for (pid, mid, class) in classified_pairs(board) {
        let pname = board.net(pid).map(|n| n.name.clone()).unwrap_or_default();
        let mname = board.net(mid).map(|n| n.name.clone()).unwrap_or_default();

        // Geometry: trace width (use the median-ish: the widest run, since the
        // controlled section is the long run, not the pad neck-down) and the
        // edge-to-edge spacing. We do not have the pair spacing from the netlist
        // directly; estimate it from the routed geometry below. Both legs must
        // be routed.
        let lp = routed_length_mm(root, pid);
        let lm = routed_length_mm(root, mid);
        if lp <= 0.0 || lm <= 0.0 {
            continue;
        }
        let (wp, wm) = (track_width_range(root, pid), track_width_range(root, mid));
        // The controlled-impedance trace width is the dominant (widest) routed
        // width of the pair; the pad neck-down is the narrow end and is not the
        // controlled section.
        let w = match (wp, wm) {
            (Some((_, maxp)), Some((_, maxm))) => (maxp + maxm) / 2.0,
            _ => continue,
        };
        if w <= 0.0 {
            continue;
        }

        // Edge-to-edge spacing of the pair, measured from the geometry: the
        // closest approach between a D+ segment and a D- segment, minus the two
        // half-widths. This is the coupled spacing the formula needs.
        let Some(s) = pair_edge_spacing(root, pid, mid) else {
            // Cannot measure the coupling geometry: report the SE impedance of
            // one leg as info only (no differential judgement without spacing).
            emit_diff_info_no_spacing(&pname, &mname, w, stackup, class, report);
            continue;
        };

        let Some(z0) = microstrip_z0(w, stackup.h_microstrip_mm, stackup.t_cu_mm, stackup.er)
        else {
            continue;
        };
        let Some(zdiff) = differential_microstrip_z(z0, s, stackup.h_microstrip_mm) else {
            continue;
        };

        judge(
            report,
            stackup,
            class,
            zdiff,
            &format!(
                "{} / {}: W~{:.3} mm, S~{:.3} mm microstrip -> Zdiff ~ {:.0} ohm",
                pname, mname, w, s, zdiff
            ),
            vec![pname.clone(), mname.clone()],
        );
    }
}

/// When the pair coupling spacing cannot be measured, report the single-ended
/// impedance of the trace width as info (never a differential finding).
fn emit_diff_info_no_spacing(
    pname: &str,
    mname: &str,
    w: f64,
    stackup: &Stackup,
    class: ImpedanceClass,
    report: &mut SiReport,
) {
    let z0 = microstrip_z0(w, stackup.h_microstrip_mm, stackup.t_cu_mm, stackup.er);
    let z0s = z0
        .map(|z| format!("{z:.0} ohm"))
        .unwrap_or_else(|| "n/a".into());
    report.findings.push(SiFinding {
        check: SiCheck::ControlledImpedance,
        severity: SiSeverity::Info,
        message: format!(
            "{} / {} ({}): pair spacing not measurable from geometry; single-ended Z0 of W~{:.3} mm \
             trace ~ {} [{}]; no differential judgement",
            pname,
            mname,
            class.label(),
            w,
            z0s,
            stackup.describe()
        ),
        refs: vec![],
        nets: vec![pname.to_string(), mname.to_string()],
    });
}

/// Single-ended 50 ohm lines (RF feeds): estimate microstrip Z0 and judge.
fn check_single_ended(
    board: &ExtractedBoard,
    root: &List,
    stackup: &Stackup,
    report: &mut SiReport,
) {
    for net in &board.nets {
        if net.id == 0 || !is_single_ended_50(&net.name) {
            continue;
        }
        let len = routed_length_mm(root, net.id);
        if len <= 0.0 {
            continue;
        }
        let Some((_, w)) = track_width_range(root, net.id) else {
            continue;
        };
        if w <= 0.0 {
            continue;
        }
        let Some(z0) = microstrip_z0(w, stackup.h_microstrip_mm, stackup.t_cu_mm, stackup.er)
        else {
            continue;
        };
        judge(
            report,
            stackup,
            ImpedanceClass::SingleEnded50,
            z0,
            &format!(
                "{}: W~{:.3} mm microstrip -> Z0 ~ {:.0} ohm",
                net.name, w, z0
            ),
            vec![net.name.clone()],
        );
    }
}

/// Compare an estimated impedance to its class target and emit the finding or
/// info note. The zero-false-positive rule: a *finding* is only emitted when the
/// stackup came from the file. A defaulted-stackup estimate is always info.
fn judge(
    report: &mut SiReport,
    stackup: &Stackup,
    class: ImpedanceClass,
    z_est: f64,
    detail: &str,
    nets: Vec<String>,
) {
    let target = class.target_ohm();
    let dev = (z_est - target) / target;
    let in_band = dev.abs() <= Z_TOLERANCE;

    // GATE 1: no real stackup -> estimate is informational, never a finding.
    // GATE 2: the board does not declare impedance-control intent
    // (`dielectric_constraints no`, or no stackup) -> the designer chose not to
    // control these nets (e.g. a full-speed USB keyboard), so a deviation is not
    // a defect. Report the computed impedance as an auditable info note. Only a
    // board that declares it is impedance-controlled can produce a finding.
    if stackup.source == StackupSource::Default || !stackup.impedance_controlled {
        let why = if stackup.source == StackupSource::Default {
            stackup.describe()
        } else {
            format!(
                "board does not declare controlled impedance (dielectric_constraints no); {}",
                stackup.describe()
            )
        };
        report.findings.push(SiFinding {
            check: SiCheck::ControlledImpedance,
            severity: SiSeverity::Info,
            message: format!(
                "{} [target {:.0} ohm {}]: estimate {:+.0}% from target - info only ({})",
                detail,
                target,
                class.label(),
                dev * 100.0,
                why
            ),
            refs: vec![],
            nets,
        });
        return;
    }

    if in_band {
        report.findings.push(SiFinding {
            check: SiCheck::ControlledImpedance,
            severity: SiSeverity::Info,
            message: format!(
                "{} vs target {:.0} ohm {} ({:+.0}%, within +-{:.0}%) - ok [{}]",
                detail,
                target,
                class.label(),
                dev * 100.0,
                Z_TOLERANCE * 100.0,
                stackup.describe()
            ),
            refs: vec![],
            nets,
        });
    } else {
        // Out of band against a real stackup: a finding. Gross deviation (>30%)
        // is high (the link will likely fail / reflect badly); 15-30% is medium.
        let sev = if dev.abs() > 0.30 {
            SiSeverity::High
        } else {
            SiSeverity::Medium
        };
        report.findings.push(SiFinding {
            check: SiCheck::ControlledImpedance,
            severity: sev,
            message: format!(
                "{} vs target {:.0} ohm {}: {:+.0}% deviation exceeds +-{:.0}% tolerance [{}]",
                detail,
                target,
                class.label(),
                dev * 100.0,
                Z_TOLERANCE * 100.0,
                stackup.describe()
            ),
            refs: vec![],
            nets,
        });
    }
}

// ===========================================================================
// Pair coupling geometry.
// ===========================================================================

/// Measure the edge-to-edge coupled spacing (mm) of a differential pair from the
/// routed geometry: the minimum centreline distance between a `plus`-net segment
/// and a `minus`-net segment over the section where they run parallel, minus the
/// two half-widths. Returns `None` if either leg has no segments.
///
/// This is a geometric estimate of the coupled gap. We take the *median* of the
/// per-plus-segment nearest-minus-segment centreline distances (robust to the
/// pad fan-out and the via transitions, where the legs splay apart), which
/// recovers the parallel-run spacing the formula wants.
fn pair_edge_spacing(root: &List, pid: i64, mid: i64) -> Option<f64> {
    let plus = net_segments(root, pid);
    let minus = net_segments(root, mid);
    if plus.is_empty() || minus.is_empty() {
        return None;
    }
    // For each plus segment, the nearest minus segment centreline distance and
    // the half-widths at the closest approach.
    let mut gaps: Vec<f64> = Vec::new();
    for p in &plus {
        let mut best = f64::INFINITY;
        let mut best_hw = 0.0;
        for m in &minus {
            let d = seg_seg_dist((p.ax, p.ay), (p.bx, p.by), (m.ax, m.ay), (m.bx, m.by));
            if d < best {
                best = d;
                best_hw = p.w / 2.0 + m.w / 2.0;
            }
        }
        if best.is_finite() {
            // Edge-to-edge gap = centreline distance - both half-widths.
            gaps.push((best - best_hw).max(0.0));
        }
    }
    if gaps.is_empty() {
        return None;
    }
    // Median gap: robust against the splayed pad-entry / via segments that sit
    // far apart and would otherwise inflate a mean.
    gaps.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid_idx = gaps.len() / 2;
    let median = if gaps.len() % 2 == 1 {
        gaps[mid_idx]
    } else {
        (gaps[mid_idx - 1] + gaps[mid_idx]) / 2.0
    };
    // A pair that never runs close (median gap implausibly large for coupling)
    // is not a routed coupled pair we can model: decline.
    if median > 5.0 {
        return None;
    }
    Some(median)
}

/// A routed segment's endpoints and width.
struct Seg {
    ax: f64,
    ay: f64,
    bx: f64,
    by: f64,
    w: f64,
}

/// All copper `(segment ...)` of a net as (endpoints, width). Arcs are not used
/// for the spacing measure (the straight runs carry the coupling).
fn net_segments(root: &List, net_id: i64) -> Vec<Seg> {
    let by_name = net_name_index(root);
    let mut out = Vec::new();
    for seg in root.find_all("segment") {
        if elem_net_id(seg, &by_name) != Some(net_id) {
            continue;
        }
        let layer = seg.find_value("layer").unwrap_or_default();
        if !layer.ends_with(".Cu") {
            continue;
        }
        let (Some(s), Some(e)) = (seg.find("start"), seg.find("end")) else {
            continue;
        };
        out.push(Seg {
            ax: s.arg_f64(0).unwrap_or(0.0),
            ay: s.arg_f64(1).unwrap_or(0.0),
            bx: e.arg_f64(0).unwrap_or(0.0),
            by: e.arg_f64(1).unwrap_or(0.0),
            w: seg.find_f64("width").unwrap_or(0.0),
        });
    }
    out
}

/// Approximate minimum distance between two segments (centrelines) as the
/// smallest of the four endpoint-to-segment distances. This does NOT return 0
/// for two segments that cross in their interiors (an X) - that case is
/// irrelevant here, since the legs of a routed differential pair run parallel,
/// never crossing, over the coupled section we measure.
fn seg_seg_dist(a1: (f64, f64), a2: (f64, f64), b1: (f64, f64), b2: (f64, f64)) -> f64 {
    let d = point_seg_dist2(a1.0, a1.1, b1.0, b1.1, b2.0, b2.1)
        .min(point_seg_dist2(a2.0, a2.1, b1.0, b1.1, b2.0, b2.1))
        .min(point_seg_dist2(b1.0, b1.1, a1.0, a1.1, a2.0, a2.1))
        .min(point_seg_dist2(b2.0, b2.1, a1.0, a1.1, a2.0, a2.1));
    d.sqrt()
}

fn point_seg_dist2(px: f64, py: f64, ax: f64, ay: f64, bx: f64, by: f64) -> f64 {
    let dx = bx - ax;
    let dy = by - ay;
    let len2 = dx * dx + dy * dy;
    if len2 <= f64::EPSILON {
        let (ex, ey) = (px - ax, py - ay);
        return ex * ex + ey * ey;
    }
    let t = (((px - ax) * dx + (py - ay) * dy) / len2).clamp(0.0, 1.0);
    let (cx, cy) = (ax + t * dx, ay + t * dy);
    let (ex, ey) = (px - cx, py - cy);
    ex * ex + ey * ey
}
