//! Check 5: controlled-impedance signal integrity.
//!
//! Tells a USB / Ethernet / high-speed designer whether their controlled-
//! impedance traces are routed to the right characteristic impedance, from
//! geometry + the board stackup, using the standard quasi-static closed-form
//! formulas (IPC-2141 / Wheeler-Hammerstad era). This is **not** a field solve:
//! it is the same arithmetic the published online calculators (chemandy,
//! Polar's IPC-2141 form, the National Semiconductor differential form) use, and
//! it carries the same few-percent error band. See `docs/checks/SI_CHECKS.md`.
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
//! Three gates. A deviation becomes a *finding* only with gates 1 and 3;
//! anything short of both is an *info* note carrying the computed value. Gate 2
//! is the veto: when it fails outright there is no impedance to report at all,
//! only the named span where the reference is absent.
//!
//! 1. **A real stackup.** The impedance can only be computed when the stackup is
//!    known: KiCad stores it in `(setup (stackup ...))` with per-dielectric
//!    `thickness` / `epsilon_r` and per-copper `thickness`. When the board has
//!    no stackup (e.g. the RP2040 minimal board), we report the estimate under a
//!    stated default assumption (1.6 mm 2-layer FR4, Er 4.3, 1 oz copper) as info
//!    only, never a fire.
//!
//! 2. **A reference plane verified under the trace.** Every formula above takes
//!    `H`, the height to a reference plane, and so assumes solid copper exists
//!    at that height directly beneath the trace. The stackup says how far away
//!    the next copper layer is; it does not say whether that layer has copper
//!    under *this* trace. A pair that crosses a plane void, a power-domain split,
//!    or the edge of a partial pour has no reference there, its real impedance
//!    rises steeply as the return current detours, and the formula's output is
//!    not an estimate of anything. So for a differential pair the assumption is
//!    checked against the copper: points along both legs, at a half-millimetre
//!    pitch, are tested against the fill polygons of the pours on the adjacent
//!    copper layer. Samples that fall in a via's anti-pad are excused (a plane's
//!    anti-pads are a designed feature, and a pair's segments terminate at its
//!    layer-transition vias, so endpoint samples land in one systematically),
//!    and the criterion is a CONTIGUOUS reference-less run of at least 2 mm along
//!    one leg, which is wider than any plausible anti-pad, so a scatter of
//!    designed clearance holes cannot add up to a void. Where copper is genuinely
//!    absent over such a run, the check reports
//!    "reference missing under trace", names the span, and says what would
//!    unlock a confident answer, instead of printing a Zdiff. Where the
//!    reference cannot be established either way (no pour on the adjacent layer,
//!    zones with no stored fill, a pair routed on an inner layer) the estimate
//!    is reported as before with its reference stated as unverified: the bias is
//!    against inventing a void.
//!
//! 3. **Declared impedance-control intent.** This is the hard-won corpus lesson.
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
//! board's own statement that the net should be controlled, and a reported
//! impedance always carries a reference plane that was either verified under the
//! trace or explicitly stated as unverified. This matches checks 1-4: unknown /
//! unintended -> info, never a confident false positive.

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

    /// The label WITHOUT its impedance, for use next to a printed target value.
    /// [`Self::label`] carries the number, so pairing it with the target read
    /// back as "[target 90 ohm USB 90 ohm differential]"; this says the kind
    /// once and lets the caller print the number once.
    pub fn kind_label(self) -> &'static str {
        match self {
            ImpedanceClass::UsbDiff => "USB differential",
            ImpedanceClass::EthernetDiff => "Ethernet differential",
            ImpedanceClass::SingleEnded50 => "single-ended",
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
    // A control/status/select qualifier means this is the antenna/RF *control*
    // GPIO (ANT_SEL, ANT_DET, RF_EN, ANT_SW…), an ordinary logic signal, NOT the
    // deliberate 50 ohm feed trace. The RF token would otherwise match the bare
    // `ANT`/`RF` token these names still carry, fabricating a controlled-impedance
    // finding on a GPIO. Exclude them to stay at zero false positives.
    let is_control = toks.iter().any(|t| {
        matches!(
            *t,
            "SEL"
                | "SELECT"
                | "DET"
                | "DETECT"
                | "CTRL"
                | "CTL"
                | "CONTROL"
                | "EN"
                | "ENABLE"
                | "SW"
                | "SWITCH"
                | "GPIO"
                | "DIV"
                | "DIVERSITY"
                | "STAT"
                | "STATUS"
        )
    });
    if is_control {
        return false;
    }
    // RF feedline conventions only. These are the nets a designer routes to a
    // deliberate 50 ohm; an ordinary digital signal is not assumed controlled.
    toks.iter().any(|t| {
        matches!(
            *t,
            "RF" | "RFIN" | "RFOUT" | "RFOUTPUT" | "ANT" | "ANTENNA" | "RF_IN" | "RF_OUT"
        ) || t.starts_with("RFIO")
            // `ANT` followed by DIGITS ONLY (ANT1/ANT2, a switched antenna feed),
            // NOT a bare `starts_with("ANT")`, which would swallow non-RF tokens
            // like ANTIALIAS and fabricate a controlled-impedance finding on a
            // mixed-signal net (the module's zero-false-positive contract).
            || t.strip_prefix("ANT")
                .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
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

        // The formula's H is the height to a reference plane. Check that the
        // plane is actually under the pair before reporting the number it
        // produces; where it is not, name the span instead.
        let reference = reference_plane_under_pair(root, pid, mid);
        if let ReferencePlane::Missing(missing) = &reference {
            emit_reference_missing(&pname, &mname, class, w, s, stackup, missing, report);
            continue;
        }
        // Solid, or unverifiable: report the Zdiff as before. A verified plane
        // is not narrated (a check that says "ok" about every assumption it
        // discharged is unreadable); an unverified one says so, so the estimate
        // never reads as more grounded than it is.
        let reference_note = match &reference {
            ReferencePlane::Solid { .. } => String::new(),
            ReferencePlane::Unverified(why) => format!(" (reference plane unverified: {why})"),
            ReferencePlane::Missing(_) => unreachable!("handled above"),
        };

        judge(
            report,
            stackup,
            class,
            zdiff,
            &format!(
                "{} / {}: W~{:.3} mm, S~{:.3} mm microstrip -> Zdiff ~ {:.0} ohm{}",
                pname, mname, w, s, zdiff, reference_note
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
    if let Some(why) = confidence_caveat(stackup) {
        report.findings.push(SiFinding {
            check: SiCheck::ControlledImpedance,
            severity: SiSeverity::Info,
            message: format!(
                "{} [target {:.0} ohm, {}]: estimate {:+.0}% from target - info only ({})",
                detail,
                target,
                class.kind_label(),
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
                "{} vs target {:.0} ohm, {} ({:+.1}%, within +-{:.0}%) - ok [{}]",
                detail,
                target,
                class.kind_label(),
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
                "{} vs target {:.0} ohm, {}: {:+.1}% deviation exceeds +-{:.0}% tolerance [{}]",
                detail,
                target,
                class.kind_label(),
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
// Reference-plane verification.
// ===========================================================================
//
// The microstrip and differential-microstrip formulas both take `H`, the height
// to *the reference plane*, and every one of them assumes there IS solid copper
// at that height directly beneath the trace. The stackup block says how far away
// the next copper layer is; it does not say whether that layer has copper under
// this particular trace. A pair that crosses a plane void, a split between two
// power domains, or the edge of a partial pour has no reference under it there:
// its real impedance rises steeply (the return current has to detour) and the
// number the formula produces is not an estimate of anything. Emitting it as a
// confident Zdiff is exactly the kind of unearned confidence this codebase
// refuses, and the fact that the assumption existed only in this module's prose
// is what made it invisible.
//
// So the assumption is now checked against the copper: sample points along both
// legs of the pair and test each against the fill polygons of the pours on the
// adjacent copper layer. Where copper is absent, the check names the span and
// says what would unlock a confident answer instead of printing a Zdiff.
//
// The bias is deliberately toward *not* abstaining: a board whose reference
// layer has no pour at all, or whose zones carry no stored fill, or whose pair
// routes somewhere the microstrip model does not describe, yields "cannot
// verify" and the existing behaviour, never a fabricated void.

/// The state of the reference copper under a routed pair.
#[derive(Debug, Clone, PartialEq)]
enum ReferencePlane {
    /// Reference copper found under every sampled point on both legs.
    Solid {
        /// The copper layer the plane was found on, for the report line.
        layer: String,
    },
    /// A run of the pair has no reference copper beneath it.
    Missing(MissingReference),
    /// Nothing to test against, with the reason. The impedance keeps its
    /// previous, stackup-gated treatment; the reason is stated so the estimate's
    /// unverified reference is never silent.
    Unverified(String),
}

/// The named span over which the reference plane is absent.
#[derive(Debug, Clone, PartialEq)]
struct MissingReference {
    /// The copper layer that should have carried the reference plane.
    layer: String,
    /// Sampled points with no copper beneath them, out of the total sampled.
    uncovered: usize,
    total: usize,
    /// Routed length (mm) of the segments that lost their reference.
    length_mm: f64,
    /// The two farthest-apart uncovered sample points, in board mm: the span
    /// the report names so the designer can go look at it.
    from: (f64, f64),
    to: (f64, f64),
}

impl MissingReference {
    /// The span, as a report reads it.
    fn describe_span(&self) -> String {
        format!(
            "~{:.2} mm of the pair's routing, between ({:.2}, {:.2}) and ({:.2}, {:.2}) mm, {} of \
             {} sampled points with no {} copper beneath",
            self.length_mm,
            self.from.0,
            self.from.1,
            self.to.0,
            self.to.1,
            self.uncovered,
            self.total,
            self.layer,
        )
    }
}

/// Sampling pitch (mm) along a routed segment. Fine enough to locate a void's
/// extent to a fraction of a millimetre, coarse enough that a 20 mm run costs
/// forty polygon tests rather than the twenty thousand a per-mil walk would.
/// Every segment gets at least its endpoints and midpoint regardless.
const SAMPLE_PITCH_MM: f64 = 0.5;

/// Cap on samples per segment, so one very long segment on a large board cannot
/// make this quadratic in board size.
const MAX_SAMPLES_PER_SEG: usize = 64;

/// Extra radius (mm) beyond a via's own barrel added to reach the edge of its
/// anti-pad in a plane. A plane's anti-pads are a *designed* feature present on
/// every routed board: the copper is deliberately cleared so the via can pass
/// through. Sampling into one is not evidence of a missing reference, and
/// because a differential pair's segments *terminate* at its layer-transition
/// vias, endpoint samples land in one systematically. Watchy is the board that
/// proved it: all seven of its "uncovered" samples were within 0.36 mm of a via
/// centre, four of them exactly on one, and its In2.Cu plane is in fact solid
/// under the pair. 0.3 mm is a generous but bounded anti-pad clearance.
const VIA_ANTIPAD_MARGIN_MM: f64 = 0.3;

/// Minimum CONTIGUOUS reference-less run (mm), along one leg, before the check
/// abstains.
///
/// Contiguous, not summed: what degrades a pair's impedance is a stretch with no
/// return path under it, and a dozen pinholes scattered along a 30 mm route are
/// not equivalent to one 12 mm gap. Summing was the second false-positive Watchy
/// produced after the via fix: four samples scattered around a test-point / pad
/// cluster summed to 1.86 mm and read as a void, when each was an individual
/// anti-pad in an otherwise continuous plane.
///
/// 2 mm is set above the widest plausible anti-pad. Pad clearances in a plane run
/// to roughly a millimetre and a half across even for a large through-hole pad,
/// and traces cross anti-pad clusters and thermal-relief spokes routinely with no
/// impedance consequence worth a report. A plane split, a power-domain boundary,
/// or the edge of a partial pour, the things that actually force the return
/// current on a detour, are all far wider than this.
const MIN_REFERENCE_VOID_MM: f64 = 2.0;

/// Verify that solid reference copper exists under both legs of a pair.
///
/// Samples both legs at [`SAMPLE_PITCH_MM`], excuses samples that fall in a via
/// anti-pad, and abstains only when the reference-less routed length reaches
/// [`MIN_REFERENCE_VOID_MM`] over a contiguous run. All three guards exist to
/// keep a designed plane feature from reading as a plane defect.
///
/// Only straight `(segment ...)` copper is sampled, not `(arc ...)`: the same
/// limitation [`pair_edge_spacing`] already has. A curved stretch is therefore
/// simply not examined, so an arc over a void is a miss rather than a false
/// alarm, and a pair routed entirely in arcs yields no samples and reports
/// `Unverified`. That is the intended direction of failure here.
fn reference_plane_under_pair(root: &List, pid: i64, mid: i64) -> ReferencePlane {
    let mut segs = net_segments(root, pid);
    segs.extend(net_segments(root, mid));
    if segs.is_empty() {
        return ReferencePlane::Unverified("the pair has no routed copper segments".into());
    }

    // The routing layer is the one carrying most of the pair's routed length.
    let mut per_layer: HashMap<&str, f64> = HashMap::new();
    for s in &segs {
        *per_layer.entry(s.layer.as_str()).or_insert(0.0) += s.length();
    }
    let Some(route_layer) = per_layer
        .iter()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(l, _)| (*l).to_string())
    else {
        return ReferencePlane::Unverified("the pair's routing layer is unknown".into());
    };

    let Some(ref_layer) = adjacent_reference_layer(root, &route_layer) else {
        return ReferencePlane::Unverified(format!(
            "{route_layer} is not an outer layer of the declared copper stack, so the microstrip \
             reference layer is not determined"
        ));
    };

    let fills = plane_fills(root, &ref_layer);
    if fills.is_empty() {
        return ReferencePlane::Unverified(format!(
            "no filled copper pour on {ref_layer} to reference against"
        ));
    }

    // Sample the segments on the routing layer only: a via-dropped stub on
    // another layer is not the microstrip run being modelled.
    let antipads = via_antipads(root);
    let mut total = 0usize;
    let mut uncovered: Vec<(f64, f64)> = Vec::new();
    let mut uncovered_len = 0.0_f64;
    for s in segs.iter().filter(|s| s.layer == route_layer) {
        let len = s.length();
        // At least the endpoints and the midpoint; more on a long segment, so a
        // void's extent is located rather than merely detected.
        let steps = ((len / SAMPLE_PITCH_MM).ceil() as usize)
            .clamp(2, MAX_SAMPLES_PER_SEG)
            .max(2);
        // Length each sample stands for along this segment.
        let step_mm = len / steps as f64;
        let mut seg_sampled = 0usize;
        let mut run = 0usize;
        let mut best_run = 0usize;
        for i in 0..=steps {
            let t = i as f64 / steps as f64;
            let (x, y) = (s.ax + t * (s.bx - s.ax), s.ay + t * (s.by - s.ay));
            // A sample inside a via's anti-pad carries no information about the
            // plane: the copper there is cleared by design.
            if antipads
                .iter()
                .any(|(vx, vy, r)| (x - vx).powi(2) + (y - vy).powi(2) <= r * r)
            {
                continue;
            }
            seg_sampled += 1;
            total += 1;
            if fills
                .iter()
                .any(|poly| crate::gerber::geo::point_in_polygon(x, y, poly))
            {
                run = 0;
            } else {
                uncovered.push((x, y));
                run += 1;
                best_run = best_run.max(run);
            }
        }
        // The longest unbroken reference-less stretch on this segment. The worst
        // single stretch anywhere on the pair is what the criterion tests, so a
        // scatter of pinholes cannot add up to a void.
        if seg_sampled > 0 {
            uncovered_len = uncovered_len.max(best_run as f64 * step_mm);
        }
    }

    if total == 0 {
        return ReferencePlane::Unverified(format!(
            "no sampleable {route_layer} routing on the pair outside its via anti-pads"
        ));
    }
    if uncovered.is_empty() {
        return ReferencePlane::Solid { layer: ref_layer };
    }
    // A scatter of pinholes is not a return-path detour.
    if uncovered_len < MIN_REFERENCE_VOID_MM {
        return ReferencePlane::Solid { layer: ref_layer };
    }

    // Name the span by the bounding box of the uncovered samples: the region a
    // designer needs to go and look at. Computed in one pass rather than by
    // searching for the farthest-apart pair, which is quadratic in the sample
    // count and, on a long pair wholly off its plane, would be thousands of
    // samples squared for the same two corners.
    let (mut min_x, mut min_y) = uncovered[0];
    let (mut max_x, mut max_y) = uncovered[0];
    for &(x, y) in &uncovered {
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
    }
    let (from, to) = ((min_x, min_y), (max_x, max_y));
    ReferencePlane::Missing(MissingReference {
        layer: ref_layer,
        uncovered: uncovered.len(),
        total,
        length_mm: uncovered_len,
        from,
        to,
    })
}

/// Every via's anti-pad in a plane, as `(x, y, radius)` in board mm: the barrel
/// radius plus [`VIA_ANTIPAD_MARGIN_MM`]. Buried/blind spans are not
/// distinguished, which is the conservative direction here (excusing one sample
/// too many only ever loses an abstention, never invents one).
fn via_antipads(root: &List) -> Vec<(f64, f64, f64)> {
    let mut out = Vec::new();
    for via in root.find_all("via") {
        let Some(at) = via.find("at") else { continue };
        let (Some(x), Some(y)) = (at.arg_f64(0), at.arg_f64(1)) else {
            continue;
        };
        let size = via.find_f64("size").unwrap_or(0.0).max(0.0);
        out.push((x, y, size / 2.0 + VIA_ANTIPAD_MARGIN_MM));
    }
    out
}

/// The copper layer a microstrip trace on `route_layer` references: the next
/// layer inward from the top of the declared stack, or the next one inward from
/// the bottom. Returns `None` for an inner layer, where the trace is a stripline
/// between two planes and this module's microstrip model does not apply.
fn adjacent_reference_layer(root: &List, route_layer: &str) -> Option<String> {
    let stack = crate::drc::copper_layers_of(root);
    let idx = stack.iter().position(|l| l == route_layer)?;
    if stack.len() < 2 {
        return None;
    }
    if idx == 0 {
        Some(stack[1].clone())
    } else if idx == stack.len() - 1 {
        Some(stack[idx - 1].clone())
    } else {
        None
    }
}

/// The fill polygons of every copper pour on `layer`.
///
/// Any pour counts as reference copper, not only a ground one: a solid power
/// plane is a perfectly good AC return path, and demanding a ground net here
/// would fabricate a "reference missing" on a board whose second layer is a
/// power plane. Zones with no stored `filled_polygon` (a board saved without
/// being refilled) contribute nothing, which lands the caller in `Unverified`
/// rather than in a false void.
///
/// A fill's own `(layer ...)` tag is authoritative when present, and is checked
/// *before* the zone's layer list. A multi-layer zone stores one fill per layer,
/// each so tagged, and the zone header may name those layers in a form this code
/// would not otherwise recognise: Watchy carries a `(layers "F&B.Cu")` zone,
/// KiCad's shorthand for front-and-back. Reading the tag first means such a zone
/// contributes its copper correctly instead of being skipped wholesale, which
/// would have shown up as a spurious "no pour on the reference layer".
fn plane_fills(root: &List, layer: &str) -> Vec<Vec<(f64, f64)>> {
    let mut out = Vec::new();
    for zone in root.find_all("zone") {
        for fp in zone.find_all("filled_polygon") {
            let belongs = match fp.find_value("layer") {
                // Tagged: the tag decides, whatever the zone header says.
                Some(fl) => fl == layer,
                // Untagged: fall back to the zone's own layer list / single
                // layer, accepting the `*.Cu` wildcard KiCad writes for
                // all-copper zones.
                None => zone_covers_layer(zone, layer),
            };
            if !belongs {
                continue;
            }
            if let Some(pts) = fp.find("pts") {
                let poly: Vec<(f64, f64)> = pts
                    .find_all("xy")
                    .map(|xy| (xy.arg_f64(0).unwrap_or(0.0), xy.arg_f64(1).unwrap_or(0.0)))
                    .collect();
                if poly.len() >= 3 {
                    out.push(poly);
                }
            }
        }
    }
    out
}

/// Does a zone's own layer declaration name `layer`? Handles the `(layers ...)`
/// list, the single `(layer ...)` form, and the `*.Cu` all-copper wildcard.
fn zone_covers_layer(zone: &List, layer: &str) -> bool {
    let named = |n: &str| n == layer || (n == "*.Cu" && layer.ends_with(".Cu"));
    zone.find("layers")
        .map(|l| (0..).map_while(|i| l.arg_value(i)).any(|n| named(&n)))
        .unwrap_or(false)
        || zone.find_value("layer").map(|l| named(&l)).unwrap_or(false)
}

/// The abstention: say the reference plane is missing under the named span, and
/// say what would turn this into a confident answer. No Zdiff is printed,
/// because there is no reference plane for the number to be the impedance
/// against.
fn emit_reference_missing(
    pname: &str,
    mname: &str,
    class: ImpedanceClass,
    w: f64,
    s: f64,
    stackup: &Stackup,
    missing: &MissingReference,
    report: &mut SiReport,
) {
    // A board that declares controlled impedance and then routes a controlled
    // pair across a plane void has a real defect, and the declaration is what
    // makes us confident enough to say so. Without the declaration this is the
    // same informational note the rest of the module emits, carrying the same
    // explanation of why it is only informational.
    let caveat = confidence_caveat(stackup);
    let severity = if caveat.is_none() {
        SiSeverity::Medium
    } else {
        SiSeverity::Info
    };
    report.findings.push(SiFinding {
        check: SiCheck::ControlledImpedance,
        severity,
        message: format!(
            "{} / {} ({}): reference missing under trace - {}. W~{:.3} mm, S~{:.3} mm; no Zdiff \
             reported, because the microstrip formula's H is the height to a reference plane that \
             is not there over that span. A confident answer needs solid reference-plane copper \
             under the whole pair (route the pair over the plane, or close the void / plane split \
             it crosses), and a stackup that declares the plane layer it references [{}]",
            pname,
            mname,
            class.label(),
            missing.describe_span(),
            w,
            s,
            caveat.unwrap_or_else(|| stackup.describe()),
        ),
        refs: vec![],
        nets: vec![pname.to_string(), mname.to_string()],
    });
}

/// Why an impedance statement about this board can only be informational, or
/// `None` when both confidence gates are met (a file-derived stackup AND the
/// board's own declaration that these nets are controlled).
///
/// Shared by [`judge`] and [`emit_reference_missing`] so the two never explain
/// the same gate differently: a reader comparing an out-of-band note with an
/// abstention on the same board must see the same reason for both.
fn confidence_caveat(stackup: &Stackup) -> Option<String> {
    if stackup.source == StackupSource::Default {
        return Some(stackup.describe());
    }
    if !stackup.impedance_controlled {
        return Some(format!(
            "board does not declare controlled impedance (dielectric_constraints no); {}",
            stackup.describe()
        ));
    }
    None
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

/// A routed segment's endpoints, width and copper layer.
struct Seg {
    ax: f64,
    ay: f64,
    bx: f64,
    by: f64,
    w: f64,
    layer: String,
}

impl Seg {
    fn length(&self) -> f64 {
        ((self.bx - self.ax).powi(2) + (self.by - self.ay).powi(2)).sqrt()
    }
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
            layer,
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

#[cfg(test)]
mod tests {
    use super::{is_single_ended_50, judge, ImpedanceClass, Stackup, StackupSource};
    use crate::si::SiReport;

    #[test]
    fn out_of_band_finding_deviation_does_not_read_equal_to_the_tolerance() {
        // R50: judge() decides in_band on the unrounded deviation (<= 15%) but the
        // finding message rounded deviation to whole percent, so a 15.4% deviation
        // rendered "+15% deviation exceeds +-15% tolerance"; the number shown
        // equalled the limit it claimed to break. Higher display precision must
        // make the deviation read as genuinely greater than the tolerance.
        let stackup = Stackup {
            h_microstrip_mm: 0.2,
            t_cu_mm: 0.035,
            er: 4.3,
            source: StackupSource::Board,
            impedance_controlled: true,
        };
        // 57.72 ohm vs 50 ohm target = +15.44% deviation (out of band).
        let mut report = SiReport::default();
        judge(
            &mut report,
            &stackup,
            ImpedanceClass::SingleEnded50,
            57.72,
            "TESTNET: W~0.200 mm microstrip -> Z0 ~ 58 ohm",
            vec!["TESTNET".into()],
        );
        let msg = &report.findings[0].message;
        assert!(
            msg.contains("15.4%"),
            "the true deviation must be shown to sub-percent precision: {msg}"
        );
        assert!(
            !msg.contains("+15% deviation exceeds +-15%"),
            "the message must not render deviation equal to the tolerance: {msg}"
        );
    }

    #[test]
    fn antenna_control_gpio_is_not_a_50_ohm_line() {
        // R49: is_single_ended_50 matched the bare `ANT` token these carry, so an
        // antenna-diversity control GPIO was classified as a 50 ohm RF feed and
        // judged against controlled-impedance limits, a false finding on a
        // wireless board (which declares controlled impedance for its real feed).
        assert!(!is_single_ended_50("ANT_SEL"), "ANT_SEL is a control GPIO");
        assert!(
            !is_single_ended_50("ANT_DET"),
            "ANT_DET is antenna-detect status"
        );
        assert!(
            !is_single_ended_50("ANT_CTRL"),
            "ANT_CTRL is a control line"
        );
        assert!(!is_single_ended_50("ANT_EN"), "ANT_EN is an enable");
        assert!(!is_single_ended_50("RF_SW"), "RF_SW is a switch control");
        assert!(
            !is_single_ended_50("ANT_DIV_SEL"),
            "diversity select is control"
        );
        // The genuine RF feed conventions must still classify.
        assert!(is_single_ended_50("ANT"), "the bare antenna feed is 50 ohm");
        assert!(is_single_ended_50("ANTENNA"), "ANTENNA feed is 50 ohm");
        assert!(is_single_ended_50("RF"), "the RF feed is 50 ohm");
        assert!(is_single_ended_50("RF_IN"), "RF_IN feed is 50 ohm");
        assert!(
            is_single_ended_50("ANT1"),
            "a switched antenna feed ANT1 is 50 ohm"
        );
        // R54: `starts_with("ANT")` swallowed non-RF tokens; only `ANT` + digits
        // (a switched feed) or the bare `ANT`/`ANTENNA` token is an RF feed.
        assert!(
            !is_single_ended_50("ANTIALIAS_IN"),
            "ANTIALIAS is not an antenna"
        );
        assert!(
            !is_single_ended_50("ANTI_ALIAS_OUT"),
            "ANTI is not an antenna"
        );
        assert!(
            is_single_ended_50("ANT2"),
            "a switched antenna feed ANT2 is 50 ohm"
        );
    }
}
