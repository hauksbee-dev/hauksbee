//! Signal-integrity / physics static checks (the `--si` surface).
//!
//! Four pure-arithmetic checks over data hauksbee already extracts (copper
//! geometry, netlists, part values) plus a small table of datasheet constants.
//! Each corresponds to a bug class that really ships:
//!
//! 1. `check_crystal_load_cap` - crystal CL spec vs the board's two load caps
//!    and stray capacitance (`CL = C1*C2/(C1+C2) + Cstray`). Wrong caps pull the
//!    oscillator off-frequency or stop it starting.
//! 2. `check_i2c_rise_time` - pull-up resistance vs computed bus capacitance vs
//!    the I2C spec rise-time limit (`t_r ~ 0.8473*R*C`). Upgrades the existing
//!    "is a pull-up present" lint to "is the pull-up *sufficient*".
//! 3. `check_antenna_keepout` - copper / ground poured inside a chip-antenna or
//!    integrated-module-antenna keepout region (per the antenna/module
//!    datasheet). The Watchy / Inkplate-6 bad-WiFi class.
//! 4. `check_usb_diff_pair` - intra-pair routed-length skew on a USB D+/D- pair
//!    vs the full-speed / high-speed limit, plus width/gap consistency.
//!
//! ## Discipline (read before trusting any fire)
//!
//! These follow the same calibration rule as the rest of hauksbee: **zero false
//! positives on known-good corpus, or the check does not fire.** Every check has
//! an explicit "unknown -> info, never a fire" path, so a missing datasheet
//! constant produces silence (or an informational note), never a confident
//! false positive. Severity thresholds are set from physics and the corpus
//! distribution, documented per check below and in `docs/checks/SI_CHECKS.md`.
//!
//! The module is split between a pure-physics layer (the `cl_parallel`,
//! `i2c_rise_time_ns`, `routed_length_mm` helpers, all hand-checked in the unit
//! tests) and the board-level audits that attribute the physics to real parts.
//! Geometry is read from the same `.kicad_pcb` s-expression the DRC parses,
//! re-deriving only the narrow slice needed here.
//!
//! Long-form how-and-why: docs/how-and-why/hauksbee-extract/si.md.

use std::collections::HashMap;
use std::fmt::Write as _;

use forge_sexpr::List;

use crate::assembly::AssemblyState;
use crate::gerber::geo::{point_in_polygon, polygon_bounds};
use crate::netlint::{
    i2c_role, is_capacitor, is_connector_like, is_ground, is_resistor, is_supply,
    is_unconnected_net, norm, parse_ohms,
};
use crate::part_class::{self, connected_pads};
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
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
            Self::Info => "info",
        }
    }
    /// Info notes are observations, not findings.
    #[must_use]
    pub const fn is_finding(self) -> bool {
        !matches!(self, Self::Info)
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
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CrystalLoadCap => "crystal_load_cap",
            Self::I2cRiseTime => "i2c_rise_time",
            Self::AntennaKeepout => "antenna_keepout",
            Self::UsbDiffPair => "usb_diff_pair",
            Self::ControlledImpedance => "controlled_impedance",
            Self::TraceAmpacity => "trace_ampacity",
            Self::InputCapRipple => "input_cap_ripple",
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
    #[must_use]
    pub fn finding_count(&self) -> usize {
        self.findings_only().count()
    }
    #[must_use]
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
    /// nothing rather than guessing). The crystal CL check is netlist-only and
    /// always runs. The I2C rise-time check runs either way, but folds routed
    /// trace capacitance in when a layout is present and says out loud that it
    /// could not when one is not.
    #[must_use]
    pub fn si_checks(&self, pcb_text: Option<&str>) -> SiReport {
        self.si_checks_with(pcb_text, &CrystalCards::new())
    }

    /// [`Self::si_checks`] with what the bound cards say about each crystal and
    /// the oscillator pins that drive it (see [`CrystalFacts`]), keyed by the
    /// crystal's reference. A crystal with no entry is judged from its value,
    /// its part-number fields and the built-in tables alone.
    #[must_use]
    pub fn si_checks_with(&self, pcb_text: Option<&str>, cards: &CrystalCards) -> SiReport {
        let mut report = SiReport::default();
        check_crystal_load_cap_with(self, cards, &mut report);

        // Geometry checks need the layout s-expression.
        let root = pcb_text
            .filter(|t| t.contains("(kicad_pcb"))
            .and_then(|t| forge_sexpr::parse(t).ok());
        let geom = root.as_ref().and_then(|doc| doc.root());

        // The I2C bus model runs netlist-only, but folds in routed trace
        // capacitance whenever the layout is available.
        check_i2c_rise_time(self, geom, &mut report);

        if let Some(r) = geom {
            check_antenna_keepout(self, r, &mut report);
            check_usb_diff_pair(self, r, &mut report);
            impedance::check_controlled_impedance(self, r, &mut report);
        }
        report
    }
}

/// Controlled-impedance signal-integrity check (single-ended microstrip /
/// stripline Z0 and differential pair impedance from geometry + stackup).
pub mod impedance;

// ===========================================================================
// Net / part helpers. The name classifiers the --si and --lint reports must
// agree on (`norm`, `is_ground`, `is_supply`, `i2c_role`, `is_connector_like`)
// live in `netlint` and are shared, so the two surfaces cannot classify one net two
// ways; what follows is what only this module needs.
// ===========================================================================

/// A net is rail-like if its name is a rail, or it structurally behaves like a
/// local supply (a bypass cap to ground sits on it). Used by the I2C check to
/// recognise pull-ups to CAD-auto-named local rails.
fn net_is_raillike(board: &ExtractedBoard, net_id: i64) -> bool {
    if let Some(n) = board.net(net_id) {
        if is_supply(&n.name) {
            return true;
        }
    }
    board.net_members(net_id).iter().any(|(c, _)| {
        AssemblyState::of(c).is_present()
            && is_capacitor(c)
            && c.pins.iter().any(|op| {
                op.net
                    .filter(|id| *id != net_id)
                    .and_then(|id| board.net(id))
                    .is_some_and(|on| is_ground(&on.name))
            })
    })
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
    // (U+00B5, "µ") and the Greek small-letter mu (U+03BC, "μ"), component
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
            // "4p7" style: the suffix doubles as a decimal point, but ONLY when
            // digits IMMEDIATELY follow it. A space- or letter-separated trailing
            // token, a dielectric code ("18pF C0G"), a voltage rating
            // ("18pF 50V"), or a tolerance ("10n 5%"), is metadata, not a
            // fraction: ignore it and take the base value, matching netlint's
            // parse_capacitance_uf. (Before this, any trailing token poisoned the
            // fractional parse and dropped the whole value to None, producing a
            // false "crystal has no load caps" finding.)
            let frac_digits: String = b.chars().take_while(char::is_ascii_digit).collect();
            if frac_digits.is_empty() {
                return Some(a * mult);
            }
            let frac: f64 = format!("0.{frac_digits}").parse().ok()?;
            return Some((a + frac) * mult);
        }
    }
    // A bare number with no unit on a crystal load cap is implausibly farads;
    // treat as picofarads only when it is a small integer-ish value, else reject.
    // Take only the leading whitespace-delimited token so a trailing metadata
    // token ("18 C0G") does not defeat the parse.
    let n: f64 = s.split_whitespace().next()?.parse().ok()?;
    (1.0..=100.0).contains(&n).then_some(n * 1e-12)
}

// ===========================================================================
// Check 1: crystal load capacitance.
// ===========================================================================
//
// Physics. A parallel-resonant crystal is specified for a particular load
// capacitance CL; the board must present that CL across the crystal terminals
// for it to run on frequency. The two load caps (one per terminal to ground, each
// plus any on-chip capacitance of the driver) are in series across the crystal and
// the stray Cs adds in parallel:
//     CL_board = ((C1 + Ci) * (C2 + Ci)) / (C1 + C2 + 2 Ci) + Cstray
// Cstray folds in trace parasitics and the IC pin capacitance, taken as a band
// (3-5 pF; 0-1 pF where the driver's on-chip figure already includes the pin)
// unless the crystal carries its own figure (an overridable property).
//
// Judgement. The CL the crystal wants comes from its card (`CrystalFacts`), or
// failing that its value ("12MHz 18pF"), a recognised part number, or the
// 32.768 kHz class default. The edge of the board's CL band nearest that spec is
// turned into a frequency pull with the exact series-resonance relation
//     ppm = 1e6 * C1m/2 * |1/(C0 + CL_a) - 1/(C0 + CL_b)|
// (C1m and C0 from the card, else conservative class defaults, said so in the
// message) and flagged from `FLAG_PPM`, High from `HIGH_PPM` or when the load is
// under half or over 1.6x the spec. The message names the caps that fit. A
// driver with software-tuned on-chip caps and no printed range is not judged.
//
// Reach / honesty. Most boards put only the frequency in the value field, so with
// no card and no stated CL the check emits an INFO note carrying the computed
// CL_board and does not fire. RTC chips that integrate their own load caps are
// recognised and never flagged for "missing caps".

/// The stray capacitance assumed per oscillator (pF), folding PCB trace
/// parasitics and the MCU/IC pin capacitance: the textbook midpoint (2-3 pF of
/// short trace + ~1-2 pF pin). Used to size the caps the message suggests.
const CSTRAY_PF: f64 = 4.0;

/// The band the board's real stray capacitance is assumed to sit in (pF). The
/// judgement uses the edge nearest the crystal's CL, so a mismatch is reported
/// only when no stray inside the band would make the board right.
const STRAY_BAND_PF: (f64, f64) = (3.0, 5.0);

/// The stray band (pF) for a driver whose card says its on-chip figure already
/// contains the chip's pin capacitance: only the board's trace is left. The
/// Si5351 data sheet (Rev. 1.3, section 4.1.1, p.15) gives the whole load as one
/// internal CL of 6 to 10 pF and allows "additional external load capacitance
/// < 2 pF" in all, so the board contributes its caps' series value plus a short
/// trace, never the 3 to 5 pF a discrete oscillator's pin and trace amount to.
const TRACE_STRAY_BAND_PF: (f64, f64) = (0.0, 1.0);

/// A crystal component property that replaces the assumed stray band with the
/// designer's own figure (pF, +-0.5 pF), e.g. from a layout extraction.
pub const STRAY_PROPERTY: &str = "hauksbee_stray_cap_pf";

/// The mismatch, as the frequency error it causes (ppm), past which the load is
/// flagged Medium; past `HIGH_PPM` it is High. A typical crystal is specified
/// +-20 ppm at 25 C with +-30 ppm over temperature, so 30 ppm of load error on
/// its own uses up the whole tolerance a designer budgets for it.
const FLAG_PPM: f64 = 30.0;
const HIGH_PPM: f64 = 100.0;

/// CL deviation (pF) beyond which a 32.768 kHz crystal that states no CL is
/// flagged against the class's standard CL (its C1 is not known).
const CL_TOLERANCE_PF: f64 = 8.0;

/// Motional and shunt capacitance assumed for a crystal whose card states none.
/// The figures sit on the low side of the datasheets (a small C1 and a large C0
/// both shrink the pulling estimate), so an assumed figure under-reports the ppm
/// rather than over-reports it. MHz AT-cut: C1 5 fF (catalogued parts print 5 to
/// 20 fF), C0 5 pF (the Abracon ABM8G maximum). 32.768 kHz tuning fork: C1 2.5 fF
/// (typically 2 to 4), C0 2 pF (the Abracon ABS07 maximum).
const DEFAULT_C1_FF_MHZ: f64 = 5.0;
const DEFAULT_C0_PF_MHZ: f64 = 5.0;
const DEFAULT_C1_FF_WATCH: f64 = 2.5;
const DEFAULT_C0_PF_WATCH: f64 = 2.0;

/// What a driving IC's card says about the load capacitance built into an
/// oscillator pin pair.
#[derive(Debug, Clone, PartialEq)]
pub enum IntegratedCaps {
    /// Capacitance (pF) the IC adds from EACH oscillator pin to ground.
    Range {
        lo_pf: f64,
        hi_pf: f64,
        /// The datasheet's figure already contains the chip's pin capacitance
        /// (see `XtalIntCap::includes_pin_capacitance`), so only a trace stray
        /// is assumed on top of it.
        pin_cap_included: bool,
        basis: String,
    },
    /// The IC has on-chip load capacitance but prints no range for it (a tunable
    /// bank), so the board's load cannot be computed.
    Unspecified { basis: String },
}

impl IntegratedCaps {
    /// What `model`'s card states for the oscillator whose pads include `pad`
    /// (`[[models.pin_capability.xtal_int_caps]]`), with the card and datasheet
    /// basis; `None` when the card lists no integrated load for that pad.
    #[must_use]
    pub fn of_card(model: &hauksbee_models::ModelEntry, pad: &str) -> Option<Self> {
        let cap = model.xtal_int_caps_on(pad)?;
        let basis = format!("card '{}' {}: {}", model.id, cap.oscillator, cap.basis);
        Some(match (cap.min_f, cap.max_f) {
            (Some(lo), Some(hi)) => Self::Range {
                lo_pf: lo * 1e12,
                hi_pf: hi * 1e12,
                pin_cap_included: cap.includes_pin_capacitance,
                basis,
            },
            _ => Self::Unspecified { basis },
        })
    }
}

/// The on-chip load capacitance a card states for a crystal's oscillator pad.
///
/// `model_of` gives a part's card (`None` for no card, or one too weak to speak
/// for the part).
///
/// The driver is found on either crystal net, or one series resistor away (the
/// RP2040 damping-resistor topology). Shared by the bound-card pipeline, which
/// resolves each part through the binder, and the card-less
/// [`ExtractedBoard::si_checks`], which asks the built-in library directly.
pub fn driver_caps_with(
    board: &ExtractedBoard,
    nets: &[i64; 2],
    model_of: impl Fn(&Component) -> Option<hauksbee_models::ModelEntry>,
) -> Option<IntegratedCaps> {
    let mut candidates: Vec<i64> = nets.to_vec();
    for &n in nets {
        for (c, _) in board.net_members(n) {
            if !AssemblyState::of(c).is_present() || !is_resistor(c) {
                continue;
            }
            candidates.extend(c.pins.iter().filter_map(|p| p.net).filter(|id| *id != n));
        }
    }
    candidates.iter().find_map(|&n| {
        board.net_members(n).into_iter().find_map(|(c, pin)| {
            if !AssemblyState::of(c).is_present() {
                return None;
            }
            IntegratedCaps::of_card(&model_of(c)?, &pin.number)
        })
    })
}

/// [`driver_caps_with`] over the built-in model library, for a board judged
/// without bound cards. Only a card that names the part counts: a guessed or
/// unresolved stand-in never speaks for an oscillator's load.
fn library_driver_caps(board: &ExtractedBoard, nets: &[i64; 2]) -> Option<IntegratedCaps> {
    use hauksbee_models::{Confidence, ModelLibrary};
    let lib = ModelLibrary::builtin_shared();
    driver_caps_with(board, nets, |c| {
        let mut q = hauksbee_models::ComponentQuery::new(
            Some(c.lib_id.clone()).filter(|s| !s.is_empty()),
            Some(c.value.clone()).filter(|s| !s.is_empty()),
            Some(c.footprint.clone()).filter(|s| !s.is_empty()),
        );
        q.reference = Some(c.reference.clone());
        q.properties.clone_from(&c.properties);
        q.pads = c
            .pins
            .iter()
            .map(|p| p.number.trim().to_string())
            .filter(|n| !n.is_empty())
            .collect();
        q.mpn = crate::bom::component_mpn(c);
        let res = lib.resolve(&q);
        if matches!(res.confidence, Confidence::Guessed | Confidence::Unresolved) {
            return None;
        }
        res.model
    })
}

/// What the bound cards say about one crystal and its driver.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CrystalFacts {
    /// The crystal's specified load capacitance (pF) and its citation, from its card.
    pub cl_pf: Option<(f64, String)>,
    /// Shunt capacitance C0 (pF) from the card.
    pub c0_pf: Option<f64>,
    /// Motional capacitance C1 (fF) from the card.
    pub c1_ff: Option<f64>,
    /// The driving IC's integrated oscillator capacitance, when its card has one.
    pub driver_caps: Option<IntegratedCaps>,
}

/// [`CrystalFacts`] by crystal reference designator.
pub type CrystalCards = std::collections::BTreeMap<String, CrystalFacts>;

/// A crystal whose datasheet CL spec we know, keyed by an uppercase substring of
/// the value / part-number field. Only parts whose datasheet pins the CL are
/// listed; each entry cites its source. Anything not here -> CL unknown -> info.
///
/// `(value-substring, CL_pF, citation)`.
const KNOWN_CRYSTAL_CL: &[(&str, f64, &str)] = &[
    // Abracon ABM8-272-T3, the 12 MHz crystal the RP2040 datasheet recommends:
    // its Table 257 (Key Crystal Specifications, section 2.16.1.1, p.216)
    // gives CL 10 pF min/typ/max, C0 3.0 pF max, ESR 50 ohm max, +-30 ppm.
    (
        "ABM8-272",
        10.0,
        "RP2040 datasheet Table 257: ABM8-272-T3 CL = 10 pF",
    ),
    // Abracon ABM8G frequency-stamped parts (used on MNT Reform) are sold in
    // multiple CL options; without the full ordering code the CL is NOT known,
    // so ABM8G alone is deliberately absent here (info, not a guess).
];

/// The standard load capacitance (pF) of a 32.768 kHz tuning-fork crystal and
/// the largest listed option: Abracon ABS07 lists CL 4, 6, 7, 9 and 12.5 pF
/// (default 12.5) and ABS25 gives 12.5 pF as standard. A watch crystal whose
/// value states no CL is judged against it with `CL_TOLERANCE_PF`.
const WATCH_CRYSTAL_CL_PF: f64 = 12.5;
const WATCH_CRYSTAL_CL_CITE: &str =
    "Abracon ABS07 and ABS25 datasheets: CL options 4 to 12.5 pF, standard 12.5 pF";

/// A 32.768 kHz watch crystal by its value ("32.768kHz", "32768", "32K").
fn is_watch_crystal(value: &str) -> bool {
    let v = value.to_ascii_uppercase().replace(' ', "");
    v.contains("32.768") || v.contains("32768") || v.starts_with("32K") || v.contains("32KHZ")
}

/// A ceramic resonator (Murata `CSTxE` / CERALOCK, ZTT, 3-terminal) integrates its
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
        // MaxLinear XR22417 USB 2.0 7-port hub: datasheet REV1A, Applications
        // Information "External Crystal": "both the load capacitors and feedback
        // resistance are internal to the XR22417 device"; the 12 MHz crystal
        // (rated 16-20 pF) hangs straight on XTALIN/XTALOUT.
        "XR22417",
        // Silicon Labs Series 2 xG22 (EFM32PG22, EFR32xG22): the HFXO has
        // on-chip, software-tuned load capacitance (CTUNE on XI and XO), so the
        // crystal needs no external caps (Silicon Labs EFM32PG22 data sheet,
        // HFXO; Gecko Platform HFXO initialisation, `CMU_HFXOInit_TypeDef`).
        "EFM32PG22",
        "EFR32BG22",
        "EFR32FG22",
        "EFR32MG22",
    ]
    .iter()
    .any(|p| v.contains(p))
}

/// A crystal's CL (pF) and where it comes from: the load capacitance its own
/// value states ("12MHz 18pF", "Q12MHz/20pF/30ppm"), else a recognised part
/// number's datasheet CL.
fn known_crystal_cl(value: &str) -> Option<(f64, String)> {
    if let Some(cl) = stated_crystal_cl(value) {
        return Some((cl, format!("the crystal's value states CL = {cl} pF")));
    }
    let v = value.to_ascii_uppercase();
    KNOWN_CRYSTAL_CL
        .iter()
        .find(|(k, _, _)| v.contains(k))
        .map(|(_, cl, cite)| (*cl, (*cite).to_string()))
}

/// A crystal's CL from the part itself: its value, then its part-number
/// properties (a stated CL or a recognised ordering code), then a property that
/// is named for the load capacitance ("Load Capacitance" = "12pF" / "12").
fn component_crystal_cl(c: &Component) -> Option<(f64, String)> {
    if let Some(found) = known_crystal_cl(&c.value) {
        return Some(found);
    }
    for (key, text) in &c.properties {
        if crate::bom::is_mpn_property_key(key) {
            if let Some((cl, cite)) = known_crystal_cl(text) {
                return Some((cl, format!("{cite} (from the {key} property)")));
            }
        }
        let k = key.to_ascii_lowercase();
        if k.contains("load") || k == "cl" || k == "c_l" {
            let t = text.to_ascii_lowercase();
            let t = t.trim().trim_end_matches("pf").trim();
            if let Ok(cl) = t.parse::<f64>() {
                if (4.0..=50.0).contains(&cl) {
                    return Some((cl, format!("the crystal's {key} property states {cl} pF")));
                }
            }
        }
    }
    // Free-text fields that describe the part (description, parameters, comment,
    // specification) can write the load among the rest ("12MHz 12pF 20ppm").
    for (key, text) in &c.properties {
        if is_descriptive_property(key) {
            if let Some(cl) = stated_crystal_cl(text) {
                return Some((
                    cl,
                    format!("the crystal's {key} property states CL = {cl} pF"),
                ));
            }
        }
    }
    None
}

/// Whether a symbol or BOM property is free text that can describe the part, as
/// opposed to a link, a footprint, a reference or a part number.
fn is_descriptive_property(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    ["descr", "param", "comment", "spec", "remark", "note"]
        .iter()
        .any(|w| k.contains(w))
}

/// The crystal's specified CL: its card's first, else what the part states.
fn crystal_spec_cl(c: &Component, facts: Option<&CrystalFacts>) -> Option<(f64, String)> {
    facts
        .and_then(|f| f.cl_pf.clone())
        .or_else(|| component_crystal_cl(c))
}

/// The load capacitance a crystal's value field states: the number right
/// before a `pF` unit ("12MHz 18pF", "Q12MHz/20pF/30ppm/2P", "16 MHz CL=12.5 pF").
/// Only 4..=50 pF counts, the span every catalogued quartz CL option sits in,
/// so a stray number is never read as one.
fn stated_crystal_cl(value: &str) -> Option<f64> {
    let up = value.to_ascii_uppercase();
    let mut from = 0;
    while let Some(i) = up[from..].find("PF") {
        let at = from + i;
        let head = up[..at].trim_end();
        let start = head
            .char_indices()
            .rev()
            .take_while(|(_, c)| c.is_ascii_digit() || *c == '.')
            .last()
            .map_or(head.len(), |(i, _)| i);
        if let Ok(cl) = head[start..].parse::<f64>() {
            if (4.0..=50.0).contains(&cl) {
                return Some(cl);
            }
        }
        from = at + 2;
    }
    None
}

/// One crystal's load-capacitor site, for checks that need the bound models
/// (the driving IC's card) to judge it.
#[derive(Debug, Clone)]
pub struct CrystalSite<'a> {
    pub crystal: &'a Component,
    /// The crystal's two signal nets.
    pub nets: [i64; 2],
    /// The fitted capacitor directly from each terminal to ground, with its
    /// value in pF; `None` when there is none or its value does not parse.
    pub caps: [Option<(&'a Component, f64)>; 2],
    /// The CL the crystal's value or a known part number states. When it is
    /// `Some`, `check_crystal_load_cap` judges the caps against it.
    pub stated_cl_pf: Option<f64>,
}

/// Every fitted two-terminal crystal (not a ceramic resonator) with its load
/// caps, the same parts `check_crystal_load_cap` reads.
#[must_use]
pub fn crystal_sites(board: &ExtractedBoard) -> Vec<CrystalSite<'_>> {
    let direct_cap = |net_id: i64| {
        board.net_members(net_id).iter().find_map(|(c, _)| {
            let to_ground = AssemblyState::of(c).is_present()
                && is_capacitor(c)
                && c.pins.iter().any(|op| {
                    op.net
                        .filter(|id| *id != net_id)
                        .and_then(|id| board.net(id))
                        .is_some_and(|on| is_ground(&on.name))
                });
            to_ground
                .then(|| parse_farads(&c.value))
                .flatten()
                .map(|f| (*c, f * 1e12))
        })
    };
    board
        .components
        .iter()
        .filter(|x| AssemblyState::of(x).is_present() && is_crystal(x) && !is_ceramic_resonator(x))
        .filter_map(|x| {
            let nets: [i64; 2] = crystal_signal_nets(board, x).try_into().ok()?;
            Some(CrystalSite {
                crystal: x,
                nets,
                caps: nets.map(&direct_cap),
                stated_cl_pf: component_crystal_cl(x).map(|(cl, _)| cl),
            })
        })
        .collect()
}

/// Is this component a discrete crystal/resonator (ref Y*/X*/XTAL, or a crystal
/// footprint/lib)? Excludes connectors (X is sometimes a connector prefix) by
/// requiring a crystal footprint or a 2/4-pin shape.
fn is_crystal(c: &Component) -> bool {
    let r = part_class::ref_designator(&c.reference);
    let lib = c.lib_id.to_ascii_lowercase();
    let fp = c.footprint.to_ascii_lowercase();
    // HC-49 (HC49U, HC49UP, HC-49/US) is a crystal-only can outline.
    let crystal_fp = lib.contains("crystal")
        || fp.contains("crystal")
        || fp.contains("xtal")
        || fp.contains("hc49")
        || fp.contains("hc-49");
    // Y is the unambiguous crystal designator. X is overloaded (connectors), so
    // an X-ref only counts as a crystal with a crystal footprint.
    if r.starts_with('Y') && r.chars().nth(1).is_some_and(|c| c.is_ascii_digit()) {
        return true;
    }
    crystal_fp && connected_pads(c) >= 2 && !r.starts_with('J') && !r.starts_with("CN")
}

/// `CL = (C1 * C2) / (C1 + C2)` series combination, in the caps' own unit.
#[must_use]
pub fn cl_series(c1: f64, c2: f64) -> f64 {
    if c1 <= 0.0 || c2 <= 0.0 {
        return 0.0;
    }
    (c1 * c2) / (c1 + c2)
}

/// Board-presented load capacitance (pF): the series combination of the two load
/// caps plus the stray.
#[must_use]
pub fn cl_board_pf(c1_pf: f64, c2_pf: f64, cstray_pf: f64) -> f64 {
    cl_series(c1_pf, c2_pf) + cstray_pf
}

/// A load cap found on a crystal terminal. `Unvalued` is a cap wired to ground
/// whose value field is unset or not a capacitance ("TBD"): it exists, only its
/// size is unknown, so it must not read as a missing cap.
#[derive(Clone, Copy)]
enum LoadCap {
    Pf(f64),
    Unvalued,
}

/// On a crystal terminal net, find the load cap to ground: a capacitor with one
/// pad on `net` and the other on a ground net.
fn load_cap_on_net(board: &ExtractedBoard, net_id: i64) -> Option<LoadCap> {
    board.net_members(net_id).iter().find_map(|(c, _p)| {
        // A DNP or identity-refused cap loads nothing: crediting it would
        // silence a genuine missing-load-cap finding.
        let to_ground = AssemblyState::of(c).is_present()
            && is_capacitor(c)
            && c.pins.iter().any(|op| {
                op.net
                    .filter(|id| *id != net_id)
                    .and_then(|id| board.net(id))
                    .is_some_and(|on| is_ground(&on.name))
            });
        to_ground
            .then(|| parse_farads(&c.value).map_or(LoadCap::Unvalued, |f| LoadCap::Pf(f * 1e12)))
    })
}

/// The two signal nets of a crystal: its non-ground pads. A 2-pin crystal has
/// exactly two signal pads; a 4-pin crystal has two signal pads and two ground
/// pads. We pick the (up to two) distinct non-ground nets the crystal touches.
fn crystal_signal_nets(board: &ExtractedBoard, xtal: &Component) -> Vec<i64> {
    let mut nets: Vec<i64> = Vec::new();
    for p in &xtal.pins {
        let Some(id) = p.net else { continue };
        let is_gnd = board.net(id).is_some_and(|n| is_ground(&n.name));
        if is_gnd {
            continue;
        }
        if !nets.contains(&id) {
            nets.push(id);
        }
    }
    nets
}

/// The stray band for one crystal: its own property when present, else the
/// `assumed` band. The bool says whether it was overridden.
fn stray_band(xtal: &Component, assumed: (f64, f64)) -> ((f64, f64), bool) {
    let own = xtal
        .properties
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(STRAY_PROPERTY))
        .and_then(|(_, v)| v.trim().parse::<f64>().ok())
        .filter(|s| (0.0..=30.0).contains(s));
    own.map_or((assumed, false), |s| (((s - 0.5).max(0.0), s + 0.5), true))
}

/// The crystal figures the pull estimate needs, and where each came from.
#[derive(Clone, Copy)]
struct Pullability {
    /// Motional capacitance (fF) and shunt capacitance (pF).
    motional_ff: f64,
    shunt_pf: f64,
    motional_assumed: bool,
    shunt_assumed: bool,
}

impl Pullability {
    /// The card's figures, else the conservative defaults for the crystal's class.
    fn of(facts: Option<&CrystalFacts>, watch: bool) -> Self {
        let (default_motional, default_shunt) = if watch {
            (DEFAULT_C1_FF_WATCH, DEFAULT_C0_PF_WATCH)
        } else {
            (DEFAULT_C1_FF_MHZ, DEFAULT_C0_PF_MHZ)
        };
        let motional = facts.and_then(|f| f.c1_ff);
        let shunt = facts.and_then(|f| f.c0_pf);
        Self {
            motional_ff: motional.unwrap_or(default_motional),
            shunt_pf: shunt.unwrap_or(default_shunt),
            motional_assumed: motional.is_none(),
            shunt_assumed: shunt.is_none(),
        }
    }

    /// The frequency error (ppm) of running the crystal at load `cl_a` instead of
    /// `cl_b` (pF): the series-resonance offset is `C1 / (2 (C0 + CL))`, so the
    /// difference of the two offsets is the pull. Exact in CL (no linearisation).
    fn pull_ppm(self, cl_a: f64, cl_b: f64) -> f64 {
        let motional_pf = self.motional_ff / 1000.0;
        1e6 * motional_pf / 2.0
            * (1.0 / (self.shunt_pf + cl_a) - 1.0 / (self.shunt_pf + cl_b)).abs()
    }
}

/// The nearest E12 value (pF) to `pf`.
fn nearest_e12_pf(pf: f64) -> f64 {
    const E12: [f64; 12] = [1.0, 1.2, 1.5, 1.8, 2.2, 2.7, 3.3, 3.9, 4.7, 5.6, 6.8, 8.2];
    let decade = 10f64.powf(pf.log10().floor());
    [decade / 10.0, decade, decade * 10.0]
        .into_iter()
        .flat_map(|d| E12.map(|e| e * d))
        .min_by(|a, b| (a - pf).abs().total_cmp(&(b - pf).abs()))
        .unwrap_or(pf)
}

/// The board side of a load judgement: external caps, on-chip caps and stray.
struct LoadSite {
    /// External capacitance on each terminal (pF).
    ext: (f64, f64),
    /// On-chip capacitance range on each pin (pF), `(0, 0)` when none.
    int: (f64, f64),
    /// Whether `int` comes from a driver card (so the message reports it).
    on_chip: bool,
    stray: (f64, f64),
    /// The words for the stray band, saying whether it is assumed.
    stray_note: String,
    /// The words for the caps: "C1=27p, C2=27p" or "no external caps".
    caps: String,
    /// The words for the on-chip capacitance, with the card's basis.
    on_chip_note: String,
}

impl LoadSite {
    /// The site of `xtal` with `ext` external caps and the `driver`'s on-chip bank.
    fn new(xtal: &Component, ext: (f64, f64), driver: &DriverCaps) -> Self {
        let trace_only = driver.range.is_some() && driver.pin_cap_included;
        let (stray, stray_own) = stray_band(
            xtal,
            if trace_only {
                TRACE_STRAY_BAND_PF
            } else {
                STRAY_BAND_PF
            },
        );
        let int = driver.range.unwrap_or((0.0, 0.0));
        let stray_note = if stray_own {
            format!(
                "{} pF stray (from the {STRAY_PROPERTY} property)",
                band_text(stray)
            )
        } else if trace_only {
            format!(
                "{} pF trace stray assumed, the pin capacitance being inside the on-chip figure \
                 (set the crystal's {STRAY_PROPERTY} property to your layout's figure)",
                band_text(stray)
            )
        } else {
            format!(
                "{} pF stray assumed (set the crystal's {STRAY_PROPERTY} property to your \
                 layout's figure)",
                band_text(stray)
            )
        };
        let caps = if ext == (0.0, 0.0) && driver.range.is_some() {
            "no external caps".to_string()
        } else {
            format!("C1={:.0}p, C2={:.0}p", ext.0, ext.1)
        };
        let on_chip_note = if driver.range.is_some() {
            format!(", +{} pF on-chip per pin{}", band_text(int), driver.basis)
        } else {
            String::new()
        };
        Self {
            ext,
            int,
            on_chip: driver.range.is_some(),
            stray,
            stray_note,
            caps,
            on_chip_note,
        }
    }

    /// The board's CL band (pF).
    fn band(&self) -> (f64, f64) {
        let (a, b) = self.ext;
        (
            cl_series(a + self.int.0, b + self.int.0) + self.stray.0,
            cl_series(a + self.int.1, b + self.int.1) + self.stray.1,
        )
    }

    /// The edge of the band nearest `spec`, and the signed distance to it: positive
    /// when the board is heavier than the spec, 0 inside the band.
    fn nearest_edge(&self, spec: f64) -> (f64, f64) {
        let (lo, hi) = self.band();
        if spec < lo {
            (lo, lo - spec)
        } else if spec > hi {
            (hi, hi - spec)
        } else {
            (spec, 0.0)
        }
    }

    /// The largest CL (pF) the on-chip bank can present on its own, when the site
    /// has an on-chip range and no external caps: the per-pin maximum over two.
    fn bank_only_reach(&self) -> Option<f64> {
        (self.on_chip && self.ext == (0.0, 0.0)).then(|| self.int.1 / 2.0)
    }

    /// The same site with `cap` pF on each terminal.
    const fn with_caps(&self, cap: f64) -> Self {
        Self {
            ext: (cap, cap),
            int: self.int,
            on_chip: self.on_chip,
            stray: self.stray,
            stray_note: String::new(),
            caps: String::new(),
            on_chip_note: String::new(),
        }
    }
}

/// "16.5-18.5" or "17.0" when the band is a point.
fn band_text((lo, hi): (f64, f64)) -> String {
    if (hi - lo).abs() < 0.05 {
        format!("{lo:.1}")
    } else {
        format!("{lo:.1}-{hi:.1}")
    }
}

/// Judge the board's load against the crystal's specified `spec_cl` (pF, with its
/// citation): a finding with the ppm pull and the caps that fit, or an ok note.
fn judge_against_spec(
    site: &LoadSite,
    (spec_cl, cite): &(f64, String),
    pull: Pullability,
) -> (SiSeverity, String) {
    let cl = band_text(site.band());
    let (stray_note, on_chip) = (&site.stray_note, &site.on_chip_note);
    // With no external caps the crystal sees only the on-chip bank, two pins in
    // series: half the per-pin maximum. A larger CL cannot be reached however the
    // bank is tuned (EFM32PG22 Table 4.12 note 2: "greater than 12 pF require
    // external load capacitors").
    if let Some(reach) = site.bank_only_reach().filter(|reach| spec_cl > reach) {
        let caps = &site.caps;
        let severity = if *spec_cl > reach * 1.6 {
            SiSeverity::High
        } else {
            SiSeverity::Medium
        };
        return (
            severity,
            format!(
                "board presents CL ~ {cl} pF ({caps}, {stray_note}{on_chip}) but the crystal \
                 specs CL = {spec_cl:.1} pF [{cite}]; with no external caps the driver's on-chip \
                 bank alone reaches at most {reach:.2} pF (half its per-pin maximum, the two \
                 pins in series), so this crystal needs external load capacitors in parallel \
                 with the on-chip ones, or a crystal with CL of {reach:.0} pF or less."
            ),
        );
    }
    let (edge, miss_pf) = site.nearest_edge(*spec_cl);
    let ppm = pull.pull_ppm(*spec_cl, edge);
    if ppm < FLAG_PPM {
        return (
            SiSeverity::Info,
            format!(
                "board CL ~ {cl} pF ({stray_note}{on_chip}) vs spec {spec_cl} pF [{cite}]: \
                 {ppm:.0} ppm of pull at worst - ok"
            ),
        );
    }
    // Past HIGH_PPM, or a load under half / over 1.6x the spec (the pull saturates
    // there; the oscillator stops starting instead).
    let severe = ppm >= HIGH_PPM || edge < spec_cl * 0.5 || edge > spec_cl * 1.6;
    let (pulled, weight) = if miss_pf > 0.0 {
        ("low", "heavier")
    } else {
        ("high", "lighter")
    };
    let want = 2.0f64.mul_add(
        spec_cl - site.stray.0.midpoint(site.stray.1),
        -site.int.0.midpoint(site.int.1),
    );
    let fit = if want >= 1.0 {
        let e12 = nearest_e12_pf(want);
        format!(
            "Caps that fit: C = 2 x (CL - stray) = {want:.1} pF each, nearest E12 {e12:.1} pF, \
             giving CL {} pF.",
            band_text(site.with_caps(e12).band())
        )
    } else {
        "Caps that fit: none; the stray and on-chip capacitance alone already exceed the \
         crystal's CL, so pick a crystal with a larger CL."
            .to_string()
    };
    let (c1_src, c0_src) = (
        if pull.motional_assumed {
            "assumed"
        } else {
            "card"
        },
        if pull.shunt_assumed {
            "assumed"
        } else {
            "card"
        },
    );
    let caps = &site.caps;
    (
        if severe {
            SiSeverity::High
        } else {
            SiSeverity::Medium
        },
        format!(
            "board presents CL ~ {cl} pF ({caps}, {stray_note}{on_chip}) but the crystal specs \
             CL = {spec_cl:.1} pF [{cite}]; the load is {:.1} pF {weight} than the spec even at the \
             nearest end of the stray band, which pulls the frequency ~{ppm:.0} ppm {pulled} (C1 \
             {:.1} fF {c1_src}, C0 {:.1} pF {c0_src}; a typical crystal is specified +-20 to \
             30 ppm in all). {fit}",
            miss_pf.abs(),
            pull.motional_ff,
            pull.shunt_pf,
        ),
    )
}

/// The note for a crystal whose CL is unknown: a 32.768 kHz fork far above its
/// class's standard CL is a finding, anything else is the computed load as info.
fn judge_without_spec(site: &LoadSite, watch: bool) -> (SiSeverity, String) {
    let cl = band_text(site.band());
    let (caps, stray_note, on_chip) = (&site.caps, &site.stray_note, &site.on_chip_note);
    let int_mid = site.int.0.midpoint(site.int.1);
    let mid_cl = cl_board_pf(site.ext.0 + int_mid, site.ext.1 + int_mid, CSTRAY_PF);
    if watch && mid_cl > WATCH_CRYSTAL_CL_PF + CL_TOLERANCE_PF {
        // A 32.768 kHz tuning fork with no stated CL: judge it against the class's
        // standard CL with the tolerance a stated CL gets.
        return (
            SiSeverity::Medium,
            format!(
                "board presents CL ~ {mid_cl:.1} pF ({caps}, +{CSTRAY_PF:.0}p stray{on_chip}) on a \
                 32.768 kHz crystal whose value states no CL, more than {CL_TOLERANCE_PF:.0} pF above \
                 the {WATCH_CRYSTAL_CL_PF} pF standard CL of the class [{WATCH_CRYSTAL_CL_CITE}]. \
                 A heavier load cuts the oscillator's gain margin (the critical gm grows with \
                 (C0+CL)^2) and pulls the frequency low; the ppm error needs the crystal's motional \
                 C1, which its datasheet rarely gives. Size the caps to the crystal's CL \
                 (C = 2 x (CL - stray)), or state a higher CL in the value if the part has one."
            ),
        );
    }
    (
        SiSeverity::Info,
        format!(
            "board presents CL ~ {cl} pF ({caps}, {stray_note}{on_chip}); load capacitance \
             unknown: crystal CL spec unknown from value, no judgement. Neither the crystal's \
             value nor its properties state a CL and no part card is bound, so the load caps \
             cannot be checked against it. State the load capacitance in the value \
             (\"12MHz 12pF\") or a Load Capacitance property, or give the crystal's part number"
        ),
    )
}

/// What a crystal's driver contributes: the on-chip capacitance range per pin
/// (when its card prints one), whether the driver integrates any, and the
/// card's basis text.
struct DriverCaps {
    range: Option<(f64, f64)>,
    /// The card says the range already contains the chip's pin capacitance.
    pin_cap_included: bool,
    tunable_unspecified: bool,
    integrates: bool,
    basis: String,
}

impl DriverCaps {
    fn of(facts: Option<&CrystalFacts>, integrated_by_name: bool) -> Self {
        let card = facts.and_then(|f| f.driver_caps.as_ref());
        Self {
            range: match card {
                Some(IntegratedCaps::Range { lo_pf, hi_pf, .. }) => Some((*lo_pf, *hi_pf)),
                _ => None,
            },
            pin_cap_included: matches!(
                card,
                Some(IntegratedCaps::Range {
                    pin_cap_included: true,
                    ..
                })
            ),
            tunable_unspecified: matches!(card, Some(IntegratedCaps::Unspecified { .. })),
            integrates: card.is_some() || integrated_by_name,
            basis: match card {
                Some(
                    IntegratedCaps::Range { basis, .. } | IntegratedCaps::Unspecified { basis },
                ) => {
                    format!(" [{basis}]")
                }
                None => String::new(),
            },
        }
    }
}

#[cfg(test)]
fn check_crystal_load_cap(board: &ExtractedBoard, report: &mut SiReport) {
    check_crystal_load_cap_with(board, &CrystalCards::new(), report);
}

fn check_crystal_load_cap_with(
    board: &ExtractedBoard,
    cards: &CrystalCards,
    report: &mut SiReport,
) {
    for xtal in &board.components {
        // Unassembled or identity-refused parts are not trusted crystals.
        if !crate::assembly::AssemblyState::of(xtal).is_present() || !is_crystal(xtal) {
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
        // The bound cards' facts; where they name no driver capacitance (a card-less
        // run, or a driver the pipeline did not bind), the built-in library's card
        // for the driver is read through the same field.
        let bound = cards.get(&xtal.reference);
        let from_library = if bound.is_some_and(|f| f.driver_caps.is_some()) {
            None
        } else {
            library_driver_caps(board, &[sig_nets[0], sig_nets[1]]).map(|caps| CrystalFacts {
                driver_caps: Some(caps),
                ..bound.cloned().unwrap_or_default()
            })
        };
        let facts = from_library.as_ref().or(bound);

        // Find a load cap on each terminal. The RP2040 series-resistor topology
        // puts the second cap on a `Net-(Cx-Pad1)` node bridged through the 1k
        // damping resistor; walk one resistor hop to find it if the direct net
        // has no cap.
        let [c1, c2] = [sig_nets[0], sig_nets[1]]
            .map(|n| load_cap_on_net(board, n).or_else(|| load_cap_through_resistor(board, n)));

        // The driver's on-chip load capacitance: its card when it has one, else
        // the built-in table of parts known to integrate theirs.
        let by_name = sig_nets
            .iter()
            .flat_map(|&n| board.net_members(n))
            .any(|(c, _)| AssemblyState::of(c).is_present() && has_integrated_xtal_caps(&c.value));
        let driver = DriverCaps::of(facts, by_name);

        let net_names: Vec<String> = sig_nets
            .iter()
            .filter_map(|id| board.net(*id).map(|n| n.name.clone()))
            .collect();
        let mut push = |severity, message: String| {
            report.findings.push(SiFinding {
                check: SiCheck::CrystalLoadCap,
                severity,
                message: format!("{} ({}): {message}", xtal.reference, xtal.value),
                refs: vec![xtal.reference.clone()],
                nets: net_names.clone(),
            });
        };

        // The external caps: both found, or (with an on-chip range to stand in
        // for them) both absent.
        let ext = match (c1, c2) {
            (Some(LoadCap::Pf(a)), Some(LoadCap::Pf(b))) => Some((a, b)),
            (None, None) if driver.range.is_some() => Some((0.0, 0.0)),
            _ => None,
        };
        if let (Some(ext), false) = (ext, driver.tunable_unspecified) {
            let site = LoadSite::new(xtal, ext, &driver);
            let watch = is_watch_crystal(&xtal.value);
            let (severity, message) = crystal_spec_cl(xtal, facts).map_or_else(
                || judge_without_spec(&site, watch),
                |spec| judge_against_spec(&site, &spec, Pullability::of(facts, watch)),
            );
            push(severity, message);
        } else if driver.tunable_unspecified {
            // The driver's on-chip bank is software-tuned with no printed range,
            // so the board's load cannot be computed: say so, judge nothing.
            push(
                SiSeverity::Info,
                format!(
                    "the driver integrates tunable load capacitance with no printed range{}; \
                     the board's CL cannot be computed, no judgement",
                    driver.basis
                ),
            );
        } else if !driver.integrates {
            // A discrete MHz crystal with no load caps either way is a real
            // omission, but only fire when BOTH are absent (a single missing
            // cap is often the series-R topology we failed to trace, so stay
            // conservative and emit info). An RTC that integrates its caps is
            // correct without them.
            if c1.is_none() && c2.is_none() {
                push(
                    SiSeverity::Medium,
                    "discrete crystal with no load capacitors to ground on either \
                     terminal; a parallel-resonant crystal needs two load caps to start on \
                     frequency (unless the driver integrates them)"
                        .into(),
                );
            } else if c1.is_some() && c2.is_some() {
                push(
                    SiSeverity::Info,
                    "load caps present but a value is unset or unreadable; no judgement".into(),
                );
            } else {
                push(
                    SiSeverity::Info,
                    "only one load cap traced (other terminal cap not found, \
                     possibly behind a series resistor); no judgement"
                        .into(),
                );
            }
        }
    }
}

/// Walk one resistor hop from a crystal terminal net to find a load cap (the
/// RP2040 series-damping-resistor topology: XOUT -> R(1k) -> node carrying the
/// second load cap and the crystal's far pad).
fn load_cap_through_resistor(board: &ExtractedBoard, net_id: i64) -> Option<LoadCap> {
    for (c, _p) in board.net_members(net_id) {
        // An absent damping resistor bridges to nothing.
        if !AssemblyState::of(c).is_present() || !is_resistor(c) {
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
// default per I2C pin) + trace capacitance (C_TRACE_PF_PER_MM times the net's
// routed length, or nothing at all when no layout is available - and the note
// then says the routing term is missing rather than implying it was zero).
//
// Mode inference is conservative: assume STANDARD mode (1000 ns) unless the net
// name encodes fast mode. So we only ever fire when even the most lenient mode
// is violated, or when the margin is severe - which is exactly what keeps it at
// zero false positives on the proven-good corpus buses (Olimex UEXT 2.2k,
// ZSWatch 1.8k/3.3k), all of which sit far under 1000 ns.

/// I2C rise-time constant: `t_r` = `K_RISE` * R * C (ln(0.7/0.3) = 0.8473).
pub const K_RISE: f64 = 0.8473;
/// Standard-mode (100 kHz) rise-time limit (ns).
pub const T_R_STANDARD_NS: f64 = 1000.0;
/// Fast-mode (400 kHz) rise-time limit (ns).
pub const T_R_FAST_NS: f64 = 300.0;
/// Default capacitance per I2C device pin (pF), a common datasheet figure.
const C_PIN_PF: f64 = 10.0;

// Trace self-capacitance to the reference plane, pF per mm of routed length.
//
// For a transmission line, `C' = sqrt(Er_eff) / (c0 * Z0)`. On FR4
// (`Er_eff ~ 3`) that is 0.116 pF/mm for a 50 ohm line, 0.077 pF/mm at 75 ohm
// and 0.057 pF/mm at 100 ohm; the widest, closest-coupled realistic case
// (`Er_eff 3.2`, `Z0 40 ohm`) gives 0.149 pF/mm.
//
// Hauksbee does not know a given I2C route's impedance, so it does not pretend
// to: it carries the whole real range and reports it. Which end is used where
// matters, and the split follows the module's standing rule that a check fires
// only when even the most LENIENT assumption is violated:
//
//   - the LOW figure decides whether to fire, so a bus is not failed on an
//     assumed geometry it may not have;
//   - the HIGH figure is reported alongside it, so the reader sees the worst
//     case the geometry permits.

/// Low end of the trace-capacitance range (pF/mm): a thin 2-layer route over a
/// distant plane, `Er_eff 2.9` at 150 ohm. Findings are gated on this, so a bus
/// is not failed on a capacitance a high-impedance route would not have.
///
/// This is the low end of the range hauksbee is willing to reason about, not a
/// proof that no route can be lower: impedance rises without bound as a trace
/// narrows and its plane recedes, and the 10 pF per device pin beside it is a
/// datasheet-typical figure rather than a floor either. The messages say "the low
/// end of the plausible range" for exactly that reason, and never call it the
/// lowest possible.
const C_TRACE_PF_PER_MM_LOW: f64 = 0.038;
/// High end of the trace-capacitance range (pF/mm): `Er_eff 3.2` at 40 ohm, the
/// widest, closest-coupled realistic case. Reported, never used to fire.
const C_TRACE_PF_PER_MM_HIGH: f64 = 0.15;

/// Render a range to whole numbers, collapsing to one when the ends round equal:
/// "20" rather than the "20-20" a blind range would print.
#[allow(clippy::float_cmp, reason = "compares whole numbers after rounding")]
fn range_0dp(low: f64, high: f64) -> String {
    if low.round() == high.round() {
        format!("{low:.0}")
    } else {
        format!("{low:.0}-{high:.0}")
    }
}

/// Trace capacitance per mm computed from the board's own geometry, when the
/// layout declares enough to do it: `C' = sqrt(Er_eff) / (c0 * Z0)`, with `Z0`
/// from the same microstrip model the controlled-impedance check uses and
/// `Er_eff` from the standard Hammerstad approximation.
///
/// This is what turns the capacitance from an assumed range into a measurement,
/// so a declared stackup plus a routed track width settles the question rather
/// than merely narrowing it. `None` when the board declares no stackup or the net
/// has no discrete track to measure, which is when the range is still all there is.
fn trace_capacitance_pf_per_mm(w_mm: f64, stack: &impedance::Stackup) -> Option<f64> {
    // c0 in mm/s, so the result is F/mm; scale to pF.
    const C0_MM_PER_S: f64 = 2.998e11;
    if w_mm <= 0.0 || stack.source != impedance::StackupSource::Board {
        return None;
    }
    let (h, t, er) = (stack.h_microstrip_mm, stack.t_cu_mm, stack.er);
    let z0 = impedance::microstrip_z0(w_mm, h, t, er)?;
    if z0 <= 0.0 {
        return None;
    }
    // Hammerstad's effective permittivity for a microstrip.
    let er_eff = f64::midpoint(er, 1.0) + ((er - 1.0) / 2.0) / (1.0 + 10.0 * h / w_mm).sqrt();
    let c_pf_per_mm = 1e12 * er_eff.sqrt() / (C0_MM_PER_S * z0);
    c_pf_per_mm.is_finite().then_some(c_pf_per_mm)
}

/// The bus-capacitance estimate. A range when the trace's impedance is unknown,
/// collapsed to a single computed value when the board declares enough geometry.
#[derive(Debug, Clone, Copy)]
struct BusCapacitance {
    /// Pin capacitance plus the low-end trace term (pF). Gates findings.
    low_pf: f64,
    /// Pin capacitance plus the high-end trace term (pF). Reported only.
    high_pf: f64,
    devices: u32,
    /// Routed length actually measured (mm), or `None` with no layout.
    trace_len_mm: Option<f64>,
    /// Set when the trace term was COMPUTED from the board's stackup and the net's
    /// track width rather than assumed from a range. Carries that pF/mm figure.
    measured_pf_per_mm: Option<f64>,
}

/// Rise time (ns) for a pull-up R (ohms) charging a bus capacitance C (pF).
#[must_use]
pub fn i2c_rise_time_ns(r_ohm: f64, c_pf: f64) -> f64 {
    // t = 0.8473 * R[ohm] * C[F]; with C in pF (1e-12) and t in ns (1e9):
    // 0.8473 * R * (C*1e-12) * 1e9 = 0.8473 * R * C * 1e-3.
    K_RISE * r_ohm * c_pf * 1e-3
}

/// True if a net leaf name carries an explicit I2C fast-mode tag as a whole
/// token (`FM` or `FAST`). Tokenised like [`i2c_role`] so a bus that merely
/// embeds the letters (`FMC_SDA`, `CONFIRM_SCL`) is NOT misread as fast-mode.
fn is_fast_mode_name(name: &str) -> bool {
    let n = norm(name);
    n.split(|c: char| !c.is_ascii_alphanumeric())
        .any(|t| t == "FM" || t == "FAST")
}

/// Find the effective pull-up resistance on an I2C net: every resistor with one
/// pad on the net and the other on a rail-like node. Multiple pull-ups on one
/// net sit in PARALLEL, so the effective R is the reciprocal of the summed
/// conductances (1/(Σ 1/Rᵢ)), strictly SMALLER than any single resistor, i.e.
/// a faster bus. Returns ohms, or `None` if the net carries no pull-up.
fn pullup_ohms(board: &ExtractedBoard, net_id: i64) -> Option<f64> {
    let mut conductance = 0.0_f64; // Σ 1/Rᵢ (siemens)
    for (c, _p) in board.net_members(net_id) {
        // A DNP option pull-up conducts nothing, and a refused record's value
        // is not conductance evidence.
        if !AssemblyState::of(c).is_present() || !is_resistor(c) {
            continue;
        }
        let to_rail = c.pins.iter().any(|op| {
            op.net
                .filter(|id| *id != net_id)
                .is_some_and(|id| net_is_raillike(board, id))
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
/// on the net (each ~`C_PIN_PF`). Trace length, when known, adds
/// [`C_TRACE_PF_PER_MM`].
fn bus_capacitance_pf(
    board: &ExtractedBoard,
    net_id: i64,
    trace_len_mm: Option<f64>,
    measured_pf_per_mm: Option<f64>,
) -> BusCapacitance {
    // Dedup by (reference, pad number): an IPC-356 both-sided through-hole access
    // record lists the same pad twice, and counting raw net_members double-counts
    // its pin capacitance (8 devices / 80 pF instead of 4 / 40 pF), inflating the
    // I2C rise time enough to fire a false fast-mode finding. Same discipline as
    // `connected_pads`.
    let mut seen: std::collections::HashSet<(&str, &str)> = std::collections::HashSet::new();
    let mut devices = 0u32;
    for (c, p) in board.net_members(net_id) {
        // An unassembled part's pin is not soldered to the bus, so it adds no
        // pin capacitance.
        if !AssemblyState::of(c).is_present() || is_resistor(c) || is_connector_like(c) {
            continue;
        }
        if seen.insert((c.reference.as_str(), p.number.as_str())) {
            devices += 1;
        }
    }
    let c_dev = f64::from(devices) * C_PIN_PF;
    let len = trace_len_mm.unwrap_or(0.0);
    // A computed figure replaces both ends of the range: there is nothing left to
    // bracket once the geometry is known.
    let (low, high) =
        measured_pf_per_mm.map_or((C_TRACE_PF_PER_MM_LOW, C_TRACE_PF_PER_MM_HIGH), |c| (c, c));
    BusCapacitance {
        low_pf: c_dev + len * low,
        high_pf: c_dev + len * high,
        devices,
        trace_len_mm,
        measured_pf_per_mm,
    }
}

#[allow(clippy::too_many_lines, reason = "one per-net rise-time pass")]
fn check_i2c_rise_time(board: &ExtractedBoard, root: Option<&List>, report: &mut SiReport) {
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
        // Routed trace copper is real bus capacitance, so fold it in whenever the
        // layout is available. Without a layout the device-count model is all we
        // have; the message says so rather than passing it off as complete.
        let trace_len_mm = root.map(|r| routed_length_mm(r, net.id));
        // With a stackup and a routed track width, the trace capacitance is
        // computable rather than assumed, which is what actually settles the
        // range. Falls back to the range when either is missing.
        // Only when the net's copper is entirely on F.Cu, because that is the only
        // layer `read_stackup` describes: it returns F.Cu's thickness and the first
        // dielectric below it. Applying those to a B.Cu route on an asymmetric
        // stackup, or to any inner layer, would compute a number for geometry the
        // board never stated and then call it measured.
        let measured = root.and_then(|r| {
            let layers = net_copper_layers(r, net.id);
            if layers.len() != 1 || !layers.contains("F.Cu") {
                return None;
            }
            // A microstrip needs a reference PLANE under the trace, which a stackup
            // does not establish: it lists dielectric thicknesses, not which copper
            // is poured solid. Every routed segment of this net must actually sit
            // over a filled lower-layer pour; a pour existing SOMEWHERE on the
            // board says nothing about the copper beneath this bus.
            if !net_is_over_a_plane(r, net.id) {
                return None;
            }
            let stack = impedance::read_stackup(r)?;
            let (w_min, _) = track_width_range(r, net.id)?;
            trace_capacitance_pf_per_mm(w_min, &stack)
        });
        let c = bus_capacitance_pf(board, net.id, trace_len_mm, measured);
        if c.low_pf <= 0.0 {
            continue;
        }
        // Fire on the LOW end of the capacitance range: a bus is never failed on
        // an assumed trace impedance it may not have. The high end is reported so
        // the reader still sees the worst case the geometry permits.
        let t_r = i2c_rise_time_ns(r, c.low_pf);
        let t_r_high = i2c_rise_time_ns(r, c.high_pf);
        // Rise time from the DEVICE PINS alone, with no trace term at all.
        //
        // This is NOT an assumption-free number: the pin count and pull-up value
        // come from the netlist, but C_PIN_PF is a datasheet-typical 10 pF, and a
        // part with 3 pF pins would sit far below it. What it is free of is the
        // GEOMETRIC assumption, the trace impedance hauksbee cannot see, which is
        // the one the trace term rests on and the one the caveat distinguishes.
        let t_r_pins = i2c_rise_time_ns(r, f64::from(c.devices) * C_PIN_PF);

        // Conservative: judge against STANDARD mode unless the name says fast.
        // Whole-token match, not raw `contains`: a bare substring test fires on
        // any leaf that merely embeds the letters (e.g. `FMC_SDA`, the FPGA
        // Mezzanine Connector I2C bus, contains "FM" inside the token "FMC"),
        // misclassifying a standard-mode bus as fast and tightening the limit
        // 3.3x, a false positive. Mirror i2c_role's tokenise-and-compare.
        let fast = is_fast_mode_name(&net.name);
        let limit = if fast { T_R_FAST_NS } else { T_R_STANDARD_NS };

        // How the capacitance was arrived at, so a reader can tell a
        // geometry-backed number from a pin-count-only floor, and can see that
        // the trace term is a range rather than a measurement.
        let devices = c.devices;
        let basis = match (c.trace_len_mm, c.measured_pf_per_mm) {
            (Some(len), Some(cpm)) => format!(
                "{devices} devices + {len:.0} mm routing at {cpm:.3} pF/mm, computed from the \
                 board stackup and the net's track width"
            ),
            (Some(len), None) => format!(
                "{devices} devices + {len:.0} mm routing at an ASSUMED \
                 {C_TRACE_PF_PER_MM_LOW}-{C_TRACE_PF_PER_MM_HIGH} pF/mm; declare the stackup to \
                 compute it from the real geometry"
            ),
            (None, _) => format!(
                "{devices} devices, routing capacitance NOT counted - \
                 upload the .kicad_pcb layout to include trace copper"
            ),
        };

        if t_r > limit {
            // Even the assumed (lenient) mode fails. How hard we say it depends on
            // what the verdict rests on: the pin-only figure needs no geometric
            // assumption, so it carries full severity, while a shortfall that only
            // appears once trace capacitance is added is true for the impedance
            // range we assumed and false above it. That one is capped at Medium
            // and says so, rather than presenting an assumption as a defect.
            let rests_on_trace = t_r_pins <= limit;
            let sev = if rests_on_trace {
                SiSeverity::Medium
            } else if t_r > limit * 1.5 {
                SiSeverity::High
            } else {
                SiSeverity::Medium
            };
            let caveat = if rests_on_trace && c.measured_pf_per_mm.is_none() {
                " This shortfall depends on the ASSUMED trace capacitance: the device pins \
                 alone are within the limit, so a higher-impedance route than assumed would \
                 pass. Declare the board's stackup and this shortfall is recomputed from the \
                 real trace geometry instead of a range."
            } else if rests_on_trace {
                " The device pins alone are within the limit, so this rests on the trace \
                 capacitance - which was computed from the board's own stackup and track \
                 width, not assumed."
            } else {
                " The device pins alone exceed the limit, so this does not rest on any routing \
                 assumption (it does still use the datasheet-typical 10 pF per I2C pin; give \
                 the parts models carrying their real pin capacitance to tighten it)."
            };
            report.findings.push(SiFinding {
                check: SiCheck::I2cRiseTime,
                severity: sev,
                message: if c.measured_pf_per_mm.is_some() {
                    // Computed geometry: there is no range, so no range language.
                    format!(
                        "I2C {role} '{}': pull-up {:.0} ohm x bus {:.0} pF ({}) gives \
                         t_r ~ {:.0} ns, over the {} limit {:.0} ns.{}",
                        net.name,
                        r,
                        c.low_pf,
                        basis,
                        t_r,
                        if fast { "fast-mode" } else { "standard-mode" },
                        limit,
                        caveat
                    )
                } else {
                    format!(
                        "I2C {role} '{}': pull-up {:.0} ohm x bus {} pF ({}) gives t_r ~ {:.0} ns \
                         at the LOW end of the plausible trace-capacitance range (up to {:.0} ns \
                         at the high end), over the {} limit {:.0} ns.{}",
                        net.name,
                        r,
                        range_0dp(c.low_pf, c.high_pf),
                        basis,
                        t_r,
                        t_r_high,
                        if fast { "fast-mode" } else { "standard-mode" },
                        limit,
                        caveat
                    )
                },
                refs: mem.iter().map(|(c, _)| c.reference.clone()).collect(),
                nets: vec![net.name.clone()],
            });
        } else {
            report.findings.push(SiFinding {
                check: SiCheck::I2cRiseTime,
                severity: SiSeverity::Info,
                message: format!(
                    "I2C {role} '{}': pull-up {:.0} ohm x {} pF ({}) -> t_r ~ {} ns (< {:.0} ns) - ok",
                    net.name,
                    r,
                    range_0dp(c.low_pf, c.high_pf),
                    basis,
                    range_0dp(t_r, t_r_high),
                    limit
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
    // ESP32-WROOM-32 / 32E / 32D: the module is 18 x 25.5 mm; the PCB
    // antenna occupies the top ~6 mm, and Espressif's hardware design guidelines
    // require a keepout of >= 15 mm beyond the antenna edge, spanning the module
    // width, with NO copper or ground on any layer. We model the footprint origin
    // at the module centre (KiCad's ESP-WROOM footprints place it so), antenna at
    // the -y (top) edge, and the keepout as the band just beyond the antenna.
    // (Espressif ESP32-WROOM-32 datasheet, "Peripheral Schematic / Keepout
    // zone"; ESP-WROOM hardware design guidelines: 15 mm keepout.)
    // A `-<n>U[E]` variant (WROOM-32U, WROOM-1U, WROOM-32UE) swaps the PCB
    // antenna for a u.FL connector, so there is no trace antenna to keep clear.
    let external_antenna = hay
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .any(|token| {
            let token = token.strip_suffix('E').unwrap_or(token);
            token
                .strip_suffix('U')
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        });
    // ESP-WROOM-02 / 02D / 02U is a different module (18 x 20 mm, pads on two
    // sides, a footprint that is symmetric about its origin): the WROOM-32
    // footprint-frame rectangle below says nothing about where its antenna is,
    // and applied to it (Duet 2, rotated 180 degrees) it lands on the module's
    // own body. Its keepout is not in this table, so it gets none.
    let is_wroom_02 = hay.contains("WROOM-02") || hay.contains("WROOM02");
    if hay.contains("WROOM") && !external_antenna && !is_wroom_02 {
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
        // The rectangle below is only the FALLBACK geometry, used when the
        // footprint's pads cannot be read: `check_antenna_keepout` starts the band
        // at the footprint's own antenna-side pad edge (`antenna_side_pad_edge`),
        // because that edge differs per footprint (WROOM-32 -5.7, WROVER -8.7)
        // and the pad-row centre used here puts the module's own ground-pad
        // stitching inside the band. A module set back from the board edge with
        // ground copper really in front of the antenna still fires.
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

/// Depth (mm) of the keepout beyond the module's antenna-side pad edge
/// (Espressif's 15 mm).
const KEEPOUT_DEPTH_MM: f64 = 15.0;

/// How far (mm) a copper object must reach past the module's antenna-side pad
/// edge before it counts as inside the antenna keepout.
///
/// The datasheet draws the keepout from the module's antenna end; the footprint
/// only locates that to its outermost pad row, and the module's own environment
/// sits right against that row: the pour's clearance contour around the end
/// pads, the stitching vias and ground tracks of the module's own ground pads.
/// Measured on the Olimex ESP32-EVB Rev L (a shipping, working board whose
/// antenna region is otherwise copper-free): the nearest ground fill vertex is
/// 0.65 mm past the pad edge and the stitching vias' centres sit just inside it.
/// 1 mm clears that environment; a pour or track that genuinely floods the
/// antenna region reaches millimetres, never tenths.
const KEEPOUT_NEAR_EDGE_SLACK_MM: f64 = 1.0;

/// The antenna-side outer edge (footprint-local y, mm) of the part's copper pads.
///
/// The keepout starts where the module's own copper ends, and that depends on
/// the footprint: a WROOM-32 footprint's end pads reach local y -5.7, a WROVER
/// footprint's reach -8.7 (it is a longer module with the same antenna end), so
/// a fixed constant put the rectangle over the WROVER's own ground pads. The
/// antenna is at the -y end for every footprint in the table. `None` when the
/// footprint cannot be found or carries no copper pad, and the caller falls back
/// to the table's constant.
fn antenna_side_pad_edge(root: &List, ant: &Component) -> Option<f64> {
    let fp = ["footprint", "module"]
        .into_iter()
        .flat_map(|kw| root.find_all(kw))
        .find(|fp| {
            let by_property = fp.find_all("property").any(|p| {
                p.arg_value(0).as_deref() == Some("Reference")
                    && p.arg_value(1).as_deref() == Some(ant.reference.as_str())
            });
            by_property
                || fp.find_all("fp_text").any(|t| {
                    t.arg_value(0).as_deref() == Some("reference")
                        && t.arg_value(1).as_deref() == Some(ant.reference.as_str())
                })
        })?;
    let fp_rot = crate::pcb::at_of(fp).2;
    fp.find_all("pad")
        .filter(|pad| {
            pad.find("layers").is_some_and(|l| {
                (0..)
                    .map_while(|i| l.arg_value(i))
                    .any(|n| n.ends_with(".Cu"))
            })
        })
        .filter_map(|pad| {
            let (_, y, pad_rot) = crate::pcb::at_of(pad);
            let size = pad.find("size")?;
            let (w, h) = (size.arg_f64(0)?, size.arg_f64(1)?);
            // A pad turned a quarter-turn relative to its footprint swaps its
            // extents along the footprint's y axis.
            let quarter_turns = ((pad_rot - fp_rot) / 90.0).round().abs();
            let along_y = if quarter_turns % 2.0 > 0.5 { w } else { h };
            Some(y - along_y / 2.0)
        })
        .min_by(f64::total_cmp)
}

/// Rotate a local-frame point by the placement angle (degrees, KiCad CCW with
/// y-down) and translate to board coordinates. Mirrors `pcb::extract_footprint`.
fn local_to_board(fx: f64, fy: f64, frot_deg: f64, lx: f64, ly: f64) -> (f64, f64) {
    let (sin, cos) = frot_deg.to_radians().sin_cos();
    (
        ly.mul_add(sin, lx.mul_add(cos, fx)),
        ly.mul_add(cos, lx.mul_add(-sin, fy)),
    )
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

/// A copper point intruding into a keepout, with its net id and a label.
struct Intrusion {
    net: i64,
    x: f64,
    y: f64,
    kind: &'static str,
}

#[allow(clippy::too_many_lines, reason = "one sweep per antenna")]
fn check_antenna_keepout(board: &ExtractedBoard, root: &List, report: &mut SiReport) {
    // Net id -> name, for excluding the antenna's own net and naming intrusions.
    let net_name = |id: i64| board.net(id).map(|n| n.name.clone()).unwrap_or_default();

    for ant in &board.components {
        if !crate::assembly::AssemblyState::of(ant).is_present() {
            continue;
        }
        let Some((table_k, cite)) = antenna_keepout(ant) else {
            continue;
        };
        let Some(pos) = ant.position else { continue };
        // The band starts at the module's own antenna-side pad edge (from the
        // footprint, else the table's constant), `KEEPOUT_DEPTH_MM` deep, less the
        // near-edge slack.
        let edge = antenna_side_pad_edge(root, ant).unwrap_or(table_k.y_max);
        let k = KeepoutRect {
            y_min: edge - KEEPOUT_DEPTH_MM,
            y_max: edge - KEEPOUT_NEAR_EDGE_SLACK_MM,
            ..table_k
        };
        let poly = keepout_polygon(pos, &k);
        let poly_v: Vec<(f64, f64)> = poly.to_vec();
        let [left, bottom, right, top] = polygon_bounds(&poly_v);

        // Resolve name-only net refs (KiCad 10 `(net "GND")`) via the board's
        // declarations; without this every track/via/zone on such a board has
        // `arg_i64(0) == None` and is skipped, yielding a confident false all-clear.
        let by_name = net_name_index(root);
        let id_to_name: std::collections::HashMap<i64, &str> =
            by_name.iter().map(|(n, &i)| (i, n.as_str())).collect();

        // Nets owned by the antenna part itself are not intrusions; its own RF
        // FEED net lives at its edge by design. But the antenna's GROUND pads are
        // bonded to the whole board ground net, so excluding every own net would
        // remove the board ground plane from the check and silently miss a ground
        // pour flooding the keepout; the exact detuning case this check exists to
        // catch. So exclude only the antenna's NON-ground own nets; a ground pour
        // under the antenna must still register as an intrusion.
        let own_nets: std::collections::HashSet<i64> = ant
            .pins
            .iter()
            .filter_map(|p| p.net)
            .filter(|id| id_to_name.get(id).is_none_or(|n| !is_ground(n)))
            .collect();

        let mut intrusions: Vec<Intrusion> = Vec::new();
        let in_box = |x: f64, y: f64| x >= left && x <= right && y >= bottom && y <= top;

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
                let mx = f64::midpoint(sx, ex);
                let my = f64::midpoint(sy, ey);
                for (x, y) in [(sx, sy), (ex, ey), (mx, my)] {
                    if in_box(x, y) && point_in_polygon(x, y, &poly_v) {
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
            if in_box(x, y) && point_in_polygon(x, y, &poly_v) {
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
                if in_box(x, y) && point_in_polygon(x, y, &poly_v) {
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
            let on_cu = zone.find("layers").is_some_and(|l| {
                (0..)
                    .map_while(|i| l.arg_value(i))
                    .any(|n| n.ends_with(".Cu"))
            }) || zone.find_value("layer").is_some_and(|l| l.ends_with(".Cu"));
            if !on_cu {
                continue;
            }
            // A pour intrudes the keepout in either of two ways: (a) a fill
            // vertex lands inside the keepout (partial overlap), or (b) the pour
            // ENGULFS the keepout, a board-wide ground plane covering the whole
            // antenna region has ALL its fill vertices outside the small keepout
            // rectangle, so vertex-only sampling missed it and reported a false
            // all-clear. Also test each keepout corner against the fill polygon to
            // catch containment (this is the exact bad-WiFi failure the check
            // exists to catch).
            let keepout_corners = [(left, bottom), (right, bottom), (right, top), (left, top)];
            let mut hit: Option<(f64, f64)> = None;
            for fp in zone.find_all("filled_polygon") {
                if let Some(pts) = fp.find("pts") {
                    let fill: Vec<(f64, f64)> = pts
                        .find_all("xy")
                        .map(|xy| (xy.arg_f64(0).unwrap_or(0.0), xy.arg_f64(1).unwrap_or(0.0)))
                        .collect();
                    // (a) fill vertex inside the keepout.
                    for &(x, y) in &fill {
                        if in_box(x, y) && point_in_polygon(x, y, &poly_v) {
                            hit = Some((x, y));
                            break;
                        }
                    }
                    // (b) keepout corner inside the pour (containment / engulf).
                    if hit.is_none() && fill.len() >= 3 {
                        for &(kx, ky) in &keepout_corners {
                            if point_in_polygon(kx, ky, &fill) {
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
        // Sort+dedup the kinds (like `nets` above) before formatting: a
        // HashSet's Debug order is randomized per process, so an unsorted set made
        // the finding message non-byte-reproducible across runs.
        let mut kinds: Vec<&str> = intrusions.iter().map(|i| i.kind).collect();
        kinds.sort_unstable();
        kinds.dedup();
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
#[must_use]
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
        total += (ex - sx).hypot(ey - sy);
    }
    // Arcs carry their true swept length, computed from KiCad's start/mid/end
    // triple. The chord is NOT a usable approximation: it is exact only in the
    // limit of a straight arc and under-reports a quarter turn by 10% and a
    // semicircle by 36% (chord 2r against arc pi*r). Copper that goes missing
    // here under-reports both I2C bus capacitance and USB intra-pair skew, so a
    // curve-heavy route would read as shorter than it is.
    for arc in root.find_all("arc") {
        if elem_net_id(arc, &by_name) != Some(net_id) {
            continue;
        }
        let (Some(s), Some(e)) = (arc.find("start"), arc.find("end")) else {
            continue;
        };
        let (sx, sy) = (s.arg_f64(0).unwrap_or(0.0), s.arg_f64(1).unwrap_or(0.0));
        let (ex, ey) = (e.arg_f64(0).unwrap_or(0.0), e.arg_f64(1).unwrap_or(0.0));
        let mid = arc
            .find("mid")
            .and_then(|m| Some((m.arg_f64(0)?, m.arg_f64(1)?)));
        total += match mid {
            Some((mx, my)) => arc_length_mm((sx, sy), (mx, my), (ex, ey)),
            // No mid point recorded: the chord is all the file gives us. It is a
            // floor on the real length, never an over-estimate.
            None => (ex - sx).hypot(ey - sy),
        };
    }
    total
}

/// True length (mm) of the circular arc through `start`, `mid`, `end`.
///
/// KiCad stores a track arc as those three points. The circumcentre gives the
/// radius, and the swept angle is the sum of the two half-sweeps start->mid and
/// mid->end, which is what makes this correct for a major arc (over 180 degrees)
/// as well as a minor one: summing halves cannot wrap the way a single
/// start-to-end angle does.
///
/// Falls back to the chord when the three points are collinear (a degenerate
/// arc, zero curvature), where the chord IS the length.
#[must_use]
pub fn arc_length_mm(start: (f64, f64), mid: (f64, f64), end: (f64, f64)) -> f64 {
    let ((sx, sy), (mx, my), (ex, ey)) = (start, mid, end);
    let chord = |a: (f64, f64), b: (f64, f64)| (b.0 - a.0).hypot(b.1 - a.1);
    // Twice the signed triangle area; zero when collinear.
    let cross = (mx - sx).mul_add(ey - sy, -((my - sy) * (ex - sx)));
    if cross.abs() < 1e-12 {
        return chord(start, end);
    }
    // Circumradius R = abc / (4 * area), with area = |cross| / 2.
    let (a, b, c) = (chord(start, mid), chord(mid, end), chord(start, end));
    let r = (a * b * c) / (2.0 * cross.abs());
    if !r.is_finite() || r <= 0.0 {
        return c;
    }
    // Half-sweep of a sub-chord of length `l` on radius `r`: 2*asin(l / 2r),
    // clamped because floating point can push the ratio a hair past 1.
    let sweep = |l: f64| 2.0 * (l / (2.0 * r)).clamp(-1.0, 1.0).asin();
    r * (sweep(a) + sweep(b))
}

/// Every filled pour polygon on a copper layer BELOW the top.
fn lower_layer_pours(root: &List) -> Vec<Vec<(f64, f64)>> {
    let mut out = Vec::new();
    for z in root.find_all("zone") {
        let layer_names = z
            .find("layers")
            .map(|l| (0..).map_while(|i| l.arg_value(i)).collect::<Vec<_>>())
            .or_else(|| z.find_value("layer").map(|l| vec![l]))
            .unwrap_or_default();
        let on_lower_copper = layer_names
            .iter()
            .any(|n| n.ends_with(".Cu") && !n.eq_ignore_ascii_case("F.Cu"));
        if !on_lower_copper {
            continue;
        }
        for fill in z.find_all("filled_polygon") {
            if let Some(pts) = fill.find("pts") {
                let poly: Vec<(f64, f64)> = pts
                    .find_all("xy")
                    .filter_map(|p| Some((p.arg_f64(0)?, p.arg_f64(1)?)))
                    .collect();
                if poly.len() >= 3 {
                    out.push(poly);
                }
            }
        }
    }
    out
}

/// Whether **every** routed segment of a net sits over a filled lower-layer pour,
/// i.e. whether this bus really has a reference plane beneath it.
///
/// A board-wide "is there a pour anywhere" test is not evidence about this net: a
/// small polygon in a far corner would turn an unreferenced route into a
/// supposedly geometry-computed microstrip. Each segment's midpoint is tested
/// against the pour polygons, so a route that leaves the plane falls back to the
/// assumed range.
///
/// Residual: a segment whose midpoint is covered but whose ends overhang the
/// pour edge still counts as covered. Shrinking that further needs real polygon
/// clipping.
fn net_is_over_a_plane(root: &List, net_id: i64) -> bool {
    let pours = lower_layer_pours(root);
    if pours.is_empty() {
        return false;
    }
    let by_name = net_name_index(root);
    let mut any_segment = false;
    for kw in ["segment", "arc"] {
        for elem in root.find_all(kw) {
            if elem_net_id(elem, &by_name) != Some(net_id) {
                continue;
            }
            let (Some(sp), Some(ep)) = (elem.find("start"), elem.find("end")) else {
                continue;
            };
            let (sx, sy) = (sp.arg_f64(0).unwrap_or(0.0), sp.arg_f64(1).unwrap_or(0.0));
            let (ex, ey) = (ep.arg_f64(0).unwrap_or(0.0), ep.arg_f64(1).unwrap_or(0.0));
            let (mx, my) = (f64::midpoint(sx, ex), f64::midpoint(sy, ey));
            any_segment = true;
            if !pours.iter().any(|poly| {
                let [x0, y0, x1, y1] = polygon_bounds(poly);
                mx >= x0 && mx <= x1 && my >= y0 && my <= y1 && point_in_polygon(mx, my, poly)
            }) {
                return false;
            }
        }
    }
    any_segment
}

/// The distinct copper layers a net's discrete tracks are routed on.
///
/// Used to decide whether the stackup actually describes this net's geometry: the
/// microstrip parameters available here are F.Cu's, so a net that leaves F.Cu is
/// outside what they can honestly be applied to.
fn net_copper_layers(root: &List, net_id: i64) -> std::collections::BTreeSet<String> {
    let by_name = net_name_index(root);
    let mut out = std::collections::BTreeSet::new();
    for kw in ["segment", "arc"] {
        for elem in root.find_all(kw) {
            if elem_net_id(elem, &by_name) != Some(net_id) {
                continue;
            }
            if let Some(l) = elem.find_value("layer") {
                out.insert(l);
            }
        }
    }
    out
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
    min.is_finite().then_some((min, max))
}

/// Find USB D+/D- pairs by net name. Returns (`plus_net_id`, `minus_net_id`,
/// `base_label`) for each pair on the board.
///
/// A pair is only the two polarity legs of the *same logical net*: they must
/// share an identical scope key (the hierarchical sheet path PLUS the leaf stem
/// with the polarity token removed) and differ only in polarity. That keeps the
/// matcher from pairing two electrically-distinct nets that merely look USB-ish,
/// in particular the connector-side and MCU-side legs on opposite sides of a
/// series device (an ESD array, common-mode choke, or series R). See
/// [`usb_pair_key`].
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
/// are identical and their polarities are opposite, i.e. they are the `+`/`-`
/// legs of the *same* logical signal in the *same* sheet scope.
///
/// Why the scope, not just the stem: across a series device (ESD array,
/// common-mode choke, series resistor) the two sides are electrically distinct
/// nodes with distinct names, e.g. a connector side `/USB_DP` + `/USB_DN` (root
/// sheet `/`, stem `USB_`) against an MCU side `/ESP32-C3-02/USB_D+` +
/// `/ESP32-C3-02/USB_D-` (sheet `/ESP32-C3-02/`, stem `USB_`). Keying on sheet
/// path + stem keeps those two real pairs separate, so the matcher never
/// compares geometry across the series device and invents a width/spacing/skew
/// mismatch.
fn usb_pair_key(raw_name: &str) -> Option<(String, char)> {
    let trimmed = raw_name.trim();
    // Split the raw name into the sheet path (up to and including the final '/')
    // and the leaf. A leading-'/'-only name keeps that '/' as its scope so two
    // root-sheet nets still share a scope, while a name under a sub-sheet keeps
    // the distinguishing sub-sheet path.
    let (sheet, leaf) = trimmed
        .rfind('/')
        .map_or(("", trimmed), |idx| (&trimmed[..=idx], &trimmed[idx + 1..]));
    let leaf = leaf.trim().to_ascii_uppercase();
    let (stem, pol) = usb_polarity(&leaf)?;
    Some((format!("{}\u{1}{}", sheet.to_ascii_uppercase(), stem), pol))
}

/// Classify an uppercased leaf net name as a USB data line: return
/// (leaf-stem-with-polarity-removed, polarity). The stem preserves whatever
/// distinguishes one logical USB net from another within a sheet (e.g. `USB1_` vs
/// `USB2_`), so two legs only pair when their stems also match.
///
/// Recognises D+/D-, DP/DN, DM, `USB_D+/USB_D`-, UD+/UD- (MNT), USBDP/USBDM,
/// *_DP/_DN, `D_P/D_N`. `None` when the leaf is not a USB data line.
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
        // A `_DP`/`_DN` pair is USB only when its nets or parts say so: an eDP
        // lane is no USB pair and USB's skew budget is not its budget.
        if impedance::pair_class(board, pid, mid) != Some(impedance::ImpedanceClass::UsbDiff) {
            continue;
        }
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
            (Some((plus_lo, plus_hi)), Some((minus_lo, minus_hi))) => {
                let inconsistent =
                    (plus_lo - minus_lo).abs() > 0.02 || (plus_hi - minus_hi).abs() > 0.02;
                if inconsistent {
                    format!(
                        "; width mismatch D+={plus_lo:.3}..{plus_hi:.3} mm vs D-={minus_lo:.3}..{minus_hi:.3} mm"
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
                    "USB pair {pname} / {mname}: routed lengths {lp:.1} / {lm:.1} mm, intra-pair skew {skew:.1} mm \
                     exceeds even the lenient full-speed budget {USB_SKEW_FS_MM:.0} mm{width_note}"
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
                    "USB pair {pname} / {mname}: lengths {lp:.1} / {lm:.1} mm, skew {skew:.2} mm (FS limit {USB_SKEW_FS_MM:.0} mm, \
                     HS limit {USB_SKEW_HS_MM:.2} mm){width_note}"
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
#[must_use]
pub fn render_si(report: &SiReport) -> String {
    let mut out = String::new();
    let n = report.finding_count();
    if n == 0 {
        // "no gating findings", not "no findings": informational notes may
        // follow directly below, and "no findings." above a list of notes
        // read as a contradiction.
        out.push_str("si-checks: no gating findings.\n");
    } else {
        let _ = writeln!(out, "si-checks: {n} finding(s)");
        let groups = crate::repeat::group_repeats(report.findings_only(), |f| {
            let [_] = f.refs.as_slice() else { return None };
            Some(format!(
                "{}/{}/{}",
                f.check.as_str(),
                f.severity.as_str(),
                crate::repeat::repeat_shape(&f.message, &f.refs)
            ))
        });
        for group in groups {
            let first = group[0];
            let (severity, check) = (first.severity.as_str(), first.check.as_str());
            if group.len() == 1 {
                let _ = writeln!(out, "  [{severity}] {check} - {}", first.message);
            } else {
                let parts: Vec<&str> = group.iter().map(|f| f.refs[0].as_str()).collect();
                let lead = crate::repeat::describe_parts(&parts);
                let _ = writeln!(out, "  [{severity}] {check} - {lead}: {}", first.message);
            }
        }
    }
    // Info notes (the auditable computed values) after the findings.
    let infos: Vec<&SiFinding> = report
        .findings
        .iter()
        .filter(|f| f.severity == SiSeverity::Info)
        .collect();
    if !infos.is_empty() {
        let _ = writeln!(out, "si-checks: {} info note(s)", infos.len());
        for f in infos {
            let _ = writeln!(out, "  [info] {} - {}", f.check.as_str(), f.message);
        }
    }
    out
}

#[cfg(test)]
mod tests;
