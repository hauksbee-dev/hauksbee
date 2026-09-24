//! Fault / stress monitor.
//!
//! After each solver chunk the scheduler hands this module the chunk's final
//! node voltages plus supply/branch currents. For every device with known
//! absolute-maximum ratings we compute the live operating point (current,
//! voltage, power) and compare it against its limits. A *stress fraction*
//! (worst rating utilisation, 0..1) is exported per component so the UI can
//! heat-map parts approaching their limits, and a [`FaultEvent`] is raised once
//! a violation is sustained (or a surge rating is exceeded instantly).
//!
//! ## Sustained-vs-surge
//!
//! Switching circuits spike hard for a single chunk on every edge, so a naive
//! "instantaneous over limit ⇒ fault" rule false-positives constantly. We only
//! raise a continuous-rating fault after the violation persists for
//! [`SUSTAIN_CHUNKS`] consecutive chunks. A *surge* rating, when present, is the
//! instantaneous ceiling: exceeding it raises immediately.
//!
//! ## Time-weighted thermal power (accepted-step integration)
//!
//! The junction-temperature check must NOT sample the chunk endpoint's
//! instantaneous power: a firmware PWM waveform switches inside the chunk, so
//! the endpoint reads either the full peak or zero depending on phase, while
//! the junction heats on the *duty-cycle average*. The solver already produces
//! the truth: every accepted step inside the chunk carries a solved operating
//! point and a `dt`. The scheduler integrates each device's dissipation over
//! those accepted steps ([`StressMonitor::step_powers`] per step, trapezoid
//! between steps) and deposits the chunk's energy via
//! [`StressMonitor::deposit_chunk_energy`]; [`StressMonitor::evaluate`] then
//! uses `energy / elapsed` — the time-weighted average power — for BOTH heat
//! checks: the junction-temperature estimate (pooled per package) and the
//! per-unit continuous Overpower rating, which is the same thermal physics.
//! A 25%-duty PWM therefore deposits 25% of the always-on energy whatever
//! the chunk width or pulse phase. When no energy was deposited for the
//! chunk (direct unit-test drives, legacy callers), the endpoint's
//! instantaneous power is the fallback.
//!
//! ## Destructive mode
//!
//! With `destructive` enabled, raising a fault also mutates the bound circuit so
//! the simulation shows the consequence and keeps running:
//!   - resistor / fuse / diode over-current → the device *opens* (we set a huge
//!     resistance or, for a diode, replace it with an open). This is the
//!     physically-typical failure for a fusible resistor or a wirebond/LED that
//!     burns out under sustained over-current.
//!   - diode reverse over-voltage (past breakdown) → the junction *shorts*
//!     (avalanche/punch-through that fails closed), modelled as a small series
//!     resistor across the former diode nodes.
//! Non-destructive mode reports continuously and never mutates the circuit.

use std::collections::HashMap;

use hauksbee_ir::evidence::{Assumption, AssumptionSource, Scope, Subject};
use hauksbee_ir::{Circuit, Device, DeviceId, NodeId};
use hauksbee_models::schema::{ComponentKind, Ratings};

/// Consecutive chunks a continuous-rating violation must persist before it is
/// reported as a fault (filters switching-edge transients).
pub const SUSTAIN_CHUNKS: u32 = 4;

/// Relative margin below which an over-limit excursion is numerical noise,
/// not evidence. A converged solve knows a node voltage no better than its
/// own acceptance band (reltol 1e-3, vntol 1e-6 V), so a device sitting
/// EXACTLY at its rating (the MNT Reform mb3.0 charger rail rides Q15 at
/// its 30.0 V abs-max by design) reads as `limit * (1 +/- 1e-10)` depending
/// on which side the transient settles from, and which side that is moves
/// with any picoamp-scale change anywhere in the matrix. Counting such an
/// excursion toward a fault manufactures a finding out of float noise. One
/// part per million is far below anything the solve can resolve and far
/// below any physically meaningful margin, so no real overstress is masked:
/// a device 0.01% over its rating still trips exactly as before.
const OVER_LIMIT_NOISE_MARGIN: f64 = 1e-6;

/// What kind of limit a fault tripped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultKind {
    /// Continuous current over `max_current_a`.
    Overcurrent,
    /// Instantaneous current over `max_surge_current_a`.
    SurgeCurrent,
    /// Power dissipation over rated / derived `max_power_w`.
    Overpower,
    /// Working/blocking voltage over `max_voltage_v`.
    Overvoltage,
    /// Reverse bias on a polarized part (electrolytic/tantalum cap).
    ReverseBias,
    /// Per-pin source/sink current over `max_pin_current_a`.
    PinOvercurrent,
    /// Two nets are shorted together (detected from copper geometry, or applied
    /// as a what-if solder-bridge scenario). Surfaced so the frontend highlights
    /// the bridge through the same fault channel as electrical-limit faults.
    Short,
    /// Steady-state junction temperature `Tj = Tamb + P*theta_JA` over the
    /// device's max junction temperature. Treated as a continuous (sustained)
    /// rating: a single switching-edge power spike does not heat a junction.
    Overtemperature,
}

impl FaultKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FaultKind::Overcurrent => "overcurrent",
            FaultKind::SurgeCurrent => "surge_current",
            FaultKind::Overpower => "overpower",
            FaultKind::Overvoltage => "overvoltage",
            FaultKind::ReverseBias => "reverse_bias",
            FaultKind::PinOvercurrent => "pin_overcurrent",
            FaultKind::Short => "short",
            FaultKind::Overtemperature => "overtemperature",
        }
    }

    /// Inverse of [`Self::as_str`]; unknown strings map to `Overcurrent`.
    pub fn from_str(s: &str) -> FaultKind {
        match s {
            "surge_current" => FaultKind::SurgeCurrent,
            "overpower" => FaultKind::Overpower,
            "overvoltage" => FaultKind::Overvoltage,
            "reverse_bias" => FaultKind::ReverseBias,
            "pin_overcurrent" => FaultKind::PinOvercurrent,
            "overtemperature" => FaultKind::Overtemperature,
            "short" => FaultKind::Short,
            _ => FaultKind::Overcurrent,
        }
    }
}

/// One raised fault.
#[derive(Debug, Clone)]
pub struct FaultEvent {
    /// Component reference designator (e.g. "D1", "R3").
    pub component: String,
    pub kind: FaultKind,
    /// The offending live value (A, V, or W depending on kind).
    pub value: f64,
    /// The rating it exceeded (same units as `value`).
    pub limit: f64,
    /// Simulation time (s) the fault was raised.
    pub t: f64,
    /// Whether the circuit was mutated (destructive mode) in response.
    pub destroyed: bool,
}

/// Per-device metadata captured at bind time so the monitor can evaluate it.
/// Built additively by the binder; the solver never sees it.
#[derive(Debug, Clone)]
pub struct DeviceMeta {
    /// Component reference designator.
    pub reference: String,
    /// IR device this entry monitors.
    pub device: DeviceId,
    /// Component kind (drives which checks apply).
    pub kind: ComponentKind,
    /// Footprint string (for deriving resistor power rating).
    pub footprint: String,
    /// Datasheet ratings, if the model carried any.
    pub ratings: Ratings,
}

impl DeviceMeta {
    /// Effective power rating (W): explicit `max_power_w`, else derived from the
    /// resistor footprint size. `None` if no power limit is known.
    pub fn power_rating_w(&self) -> Option<f64> {
        if let Some(p) = self.ratings.max_power_w {
            return Some(p);
        }
        // Only a RESISTOR has a footprint-derived power rating. `Passive` also
        // covers capacitors, inductors and ferrite beads, whose limits are
        // current and voltage, not a chip-resistor wattage: handing an
        // `Inductor_SMD:L_0805_2012Metric` an 0805 resistor's 1/8 W invents an
        // overpower fault out of ordinary I^2R heating in a coil.
        if matches!(self.kind, ComponentKind::Passive) && self.is_resistor_like() {
            return resistor_power_from_footprint(&self.footprint).watts;
        }
        None
    }

    /// Whether this passive is a resistor, from the footprint library/body name
    /// with the reference designator as the fallback.
    ///
    /// Deliberately narrow: a part that does not look like a resistor gets no
    /// derived wattage at all, because a wrong power rating fires on correct
    /// designs and a missing one only declines to check.
    fn is_resistor_like(&self) -> bool {
        let f = self.footprint.to_ascii_uppercase();
        let body = f.rsplit(':').next().unwrap_or(&f);
        if f.contains("RESISTOR") || body.starts_with("R_") || body.starts_with("R-") {
            return true;
        }
        // Anything that names another passive family is definitely not one.
        if f.contains("CAPACITOR")
            || f.contains("INDUCTOR")
            || f.contains("FERRITE")
            || f.contains("CHOKE")
            || body.starts_with("C_")
            || body.starts_with("L_")
            || body.starts_with("FB_")
        {
            return false;
        }
        // Fall back to the reference designator's letter prefix: "R7" is a
        // resistor, "RN1" a resistor network, "L1"/"C3" are not.
        let prefix: String = self
            .reference
            .chars()
            .take_while(|c| c.is_ascii_alphabetic())
            .collect::<String>()
            .to_ascii_uppercase();
        matches!(prefix.as_str(), "R" | "RN" | "RA")
    }

    /// The coverage gap this device leaves in the overpower check, when its
    /// package could not be read at all. An abstention nobody can see is
    /// indistinguishable from a pass, so the caller surfaces this.
    /// The unreadable package string behind this device's missing power rating,
    /// when that is why its overpower check could not run. `None` when the device
    /// has a rating, is not a resistor, or its package was readable.
    ///
    /// The aggregation key: boards commonly carry dozens of resistors from one
    /// library whose naming this cannot parse, and reporting each separately
    /// floods the honesty channel to the point where it stops being read.
    pub fn power_coverage_package(&self) -> Option<&str> {
        if !matches!(self.kind, ComponentKind::Passive)
            || self.ratings.max_power_w.is_some()
            || !self.is_resistor_like()
        {
            return None;
        }
        if resistor_power_from_footprint(&self.footprint).basis != ResistorPowerBasis::Unknown {
            return None;
        }
        Some(if self.footprint.is_empty() {
            "(none)"
        } else {
            self.footprint.as_str()
        })
    }

    pub fn power_coverage_gap(&self) -> Option<String> {
        let package = self.power_coverage_package()?;
        Some(format!(
            "stress: {} has no power rating and no readable package \"{package}\", so its \
             overpower check did not run. Give the part a model with ratings.max_power_w, \
             or a footprint / BOM line naming the package, to cover it.",
            self.reference,
        ))
    }

    /// Effective junction-to-ambient thermal resistance (C/W): explicit
    /// `theta_ja_c_per_w` from the model, else derived from the footprint
    /// package class. Always returns a value (the footprint default is the
    /// conservative fallback), so every dissipating device gets a temperature.
    pub fn theta_ja_c_per_w(&self) -> f64 {
        self.ratings
            .theta_ja_c_per_w
            .unwrap_or_else(|| crate::thermal::theta_ja_from_footprint(&self.footprint, self.kind))
    }

    /// Effective maximum junction temperature (C): explicit
    /// `max_junction_temp_c` from the model, else the per-package-class default
    /// (150 C for power packages, 125 C otherwise).
    pub fn tj_max_c(&self) -> f64 {
        self.ratings
            .max_junction_temp_c
            .unwrap_or_else(|| crate::thermal::default_tj_max(&self.footprint))
    }
}

/// What a resistor's power rating was derived from. The rating is only as good
/// as the package evidence behind it, and an overstress verdict that rests on a
/// guessed package must say so rather than read as a measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResistorPowerBasis {
    /// A recognised chip-resistor size code (the imperial token in the
    /// footprint). The rating is the standard figure for that package.
    ChipPackage,
    /// A recognised through-hole axial body: the 1/4 W industry default.
    ThtAxial,
    /// No usable package evidence at all. No rating is derived, and the device
    /// is reported as an uncovered gap instead of being handed a number.
    Unknown,
}

/// A footprint-derived resistor power rating and the evidence behind it.
#[derive(Debug, Clone, Copy)]
pub struct ResistorPower {
    /// The derived rating (W). `None` only for [`ResistorPowerBasis::Unknown`],
    /// where deriving anything would be an invention.
    pub watts: Option<f64>,
    pub basis: ResistorPowerBasis,
}

/// One message per unreadable PACKAGE rather than per part, naming the count and
/// a few representative references.
///
/// A board with fifty resistors from one unparseable library is one coverage hole
/// with fifty instances, not fifty holes, and rendering it as fifty is how an
/// honesty channel gets tuned out.
pub fn aggregate_power_coverage_gaps(metas: &[DeviceMeta]) -> Vec<String> {
    let mut by_package: std::collections::BTreeMap<&str, Vec<&str>> =
        std::collections::BTreeMap::new();
    for m in metas {
        if let Some(pkg) = m.power_coverage_package() {
            let refs = by_package.entry(pkg).or_default();
            if !refs.contains(&m.reference.as_str()) {
                refs.push(m.reference.as_str());
            }
        }
    }
    by_package
        .into_iter()
        .map(|(pkg, mut refs)| {
            refs.sort_unstable();
            const SHOWN: usize = 5;
            let listed = refs
                .iter()
                .take(SHOWN)
                .copied()
                .collect::<Vec<_>>()
                .join(", ");
            let rest = refs.len().saturating_sub(SHOWN);
            let named = if rest > 0 {
                format!("{listed} and {rest} more")
            } else {
                listed
            };
            format!(
                "stress: {} resistor(s) have no power rating and no readable package \"{}\" \
                 ({}), so their overpower checks did not run. Give the parts a model with \
                 ratings.max_power_w, or a footprint / BOM line naming the package, to cover \
                 them.",
                refs.len(),
                pkg,
                named,
            )
        })
        .collect()
}

/// Derive a resistor's power rating from its footprint package size. Standard
/// chip-resistor ratings: 01005 1/32 W, 0201 1/20 W, 0402 1/16 W, 0603 1/10 W,
/// 0805 1/8 W, 1206 1/4 W.
///
/// ## Why an unrecognised package gets no rating
///
/// A flat "conservative" default for anything unrecognised cannot exist, because
/// there is no direction that is safe. A 1/4 W default **exceeds** the real
/// rating of an 0402 (1/16 W) by 4x and an 0603 (1/10 W) by 2.5x, so it silently
/// suppresses genuine overpower findings. A 1/16 W floor **undercuts** every part
/// above the smallest, so it invents overpower faults on correct designs: a real
/// 0603 behind a custom footprint name dissipating 80 mW is inside its 100 mW
/// rating but outside a guessed 62.5 mW one.
///
/// So the size is either read or the part abstains. A recognised chip code (by
/// imperial or metric token) gives the standard rating; a recognised small axial
/// body gives its genuine 1/4 W; everything else derives nothing and becomes a
/// named coverage gap naming the unlock.
pub fn resistor_power_from_footprint(footprint: &str) -> ResistorPower {
    let f = footprint.to_ascii_uppercase();
    let chip = |w: f64| ResistorPower {
        watts: Some(w),
        basis: ResistorPowerBasis::ChipPackage,
    };
    let unknown = || ResistorPower {
        watts: None,
        basis: ResistorPowerBasis::Unknown,
    };
    // A name carrying ONLY a metric code must be read as metric, before the
    // imperial pass gets to it. "R_0402Metric" is metric 0402, an imperial
    // 01005 at 1/32 W; letting `contains("0402")` claim it rates the part
    // 1/16 W, double its real limit, and suppresses genuine overpower findings.
    // KiCad's dual form ("R_0201_0603Metric") carries a separate imperial token
    // and is deliberately left to the imperial pass below, which is what keeps
    // the 0201-is-metric-0603 collision correct.
    match classify_metric_only(&f) {
        // A metric-only name whose code we know.
        MetricCode::Rated(w) => return chip(w),
        // A metric-only name whose code we do NOT know must abstain here, not fall
        // through: "R_2010Metric" is a 2.0 x 1.0 mm body, and letting the imperial
        // pass match its "2010" substring rates it as a 3/4 W imperial 2010, an
        // order of magnitude out, and suppresses real overpower findings.
        MetricCode::Unrecognised => return unknown(),
        MetricCode::Absent => {}
    }
    // Match the imperial size token anywhere in the footprint string
    // (e.g. "Resistor_SMD:R_0402_1005Metric"). The imperial code is paired with
    // its metric code, and the metric code of a small part collides with the
    // imperial code of a larger one: imperial 0201 → metric 0603
    // ("R_0201_0603Metric"), imperial 01005 → metric 0402. So the smallest
    // packages MUST be matched first, before the larger imperial tokens they
    // embed as a metric suffix, or they are silently over-rated.
    if let Some(w) = imperial_chip_watts(|code| f.contains(code)) {
        chip(w)
    } else if let Some(w) = din_axial_rating(&f) {
        // A DIN body code IS size evidence, and the codes are not interchangeable:
        // DIN0204 is a 1/8 W body while DIN0207 is 1/4 W, so treating them alike
        // over-rates the smaller one twofold and suppresses its overpower check.
        // An axial footprint with no DIN code carries no size evidence and falls
        // through to the abstention below.
        ResistorPower {
            watts: Some(w),
            basis: ResistorPowerBasis::ThtAxial,
        }
    } else {
        // The package class may be known but its SIZE is not, or nothing is known
        // at all. Either way there is no rating to derive: a real 0603 behind a
        // custom footprint name is a 1/10 W part, and handing it any floor either
        // invents an overpower fault (if the floor is lower than the truth) or
        // suppresses a real one (if it is higher). It becomes a named coverage
        // gap instead, which is the honest form of not knowing.
        unknown()
    }
}

/// Every imperial chip size code the rating table recognises, smallest body
/// first (the ordering [`resistor_power_from_footprint`] depends on), and the
/// standard rating (W) that body carries.
const IMPERIAL_CHIP_WATTS: &[(&str, f64)] = &[
    ("01005", 1.0 / 32.0),
    ("0201", 1.0 / 20.0),
    ("0402", 1.0 / 16.0),
    ("0603", 1.0 / 10.0),
    ("0805", 1.0 / 8.0),
    ("1206", 1.0 / 4.0),
    ("1210", 1.0 / 2.0),
    ("2010", 3.0 / 4.0),
    ("2512", 1.0),
    ("1812", 1.0),
    ("2220", 1.0),
];

/// The rating of the first [`IMPERIAL_CHIP_WATTS`] code `wanted` accepts.
fn imperial_chip_watts(wanted: impl Fn(&str) -> bool) -> Option<f64> {
    IMPERIAL_CHIP_WATTS
        .iter()
        .find(|(code, _)| wanted(code))
        .map(|(_, w)| *w)
}

/// The chip rating for a footprint that carries only the METRIC size code.
///
/// Reached only after the imperial pass has failed, which is what keeps the
/// well-known collisions safe: an imperial 0201 is metric 0603 and an imperial
/// 01005 is metric 0402, so a metric code must never be consulted while an
/// imperial token is still available.
enum MetricCode {
    /// A metric-only name with a code in the table.
    Rated(f64),
    /// A metric-only name whose code is not in the table.
    Unrecognised,
    /// No usable metric-only code: either no METRIC suffix at all, or KiCad's
    /// dual form whose imperial token is authoritative.
    Absent,
}

/// The run of ASCII digits that ends `s`, in order ("R_0402" -> "0402").
fn trailing_digits(s: &str) -> String {
    let mut d: Vec<char> = s.chars().rev().take_while(char::is_ascii_digit).collect();
    d.reverse();
    d.into_iter().collect()
}

fn classify_metric_only(f: &str) -> MetricCode {
    // (metric code, imperial equivalent, rating W)
    const TABLE: &[(&str, f64)] = &[
        ("0402", 1.0 / 32.0), // imperial 01005
        ("0603", 1.0 / 20.0), // imperial 0201
        ("1005", 1.0 / 16.0), // imperial 0402
        ("1608", 1.0 / 10.0), // imperial 0603
        ("2012", 1.0 / 8.0),  // imperial 0805
        ("3216", 1.0 / 4.0),  // imperial 1206
        ("3225", 1.0 / 2.0),  // imperial 1210
        ("5025", 3.0 / 4.0),  // imperial 2010
        ("6332", 1.0),        // imperial 2512
        ("4532", 1.0),        // imperial 1812
        ("5750", 1.0),        // imperial 2220
    ];
    // The metric code is the digit run immediately before "METRIC".
    let Some(idx) = f.find("METRIC") else {
        return MetricCode::Absent;
    };
    let metric = trailing_digits(&f[..idx]);
    if metric.is_empty() {
        return MetricCode::Absent;
    }
    // Dual-code detection has to be STRUCTURAL, not "does any other number
    // appear". Real KiCad names carry pad dimensions
    // ("R_0402Metric_Pad0.74x0.62mm"), and counting those as a second code sends
    // a metric-only name to the imperial pass, which reads its 0402 as imperial
    // 0402 (1/16 W) instead of metric 0402 (imperial 01005, 1/32 W).
    //
    // The dual form is exactly "<imperial>_<metric>METRIC", so look only at the
    // token immediately before the metric code, separated by a single '_'.
    let before = &f[..idx - metric.len()];
    if let Some(prev) = before.strip_suffix('_') {
        let prev_token = trailing_digits(prev);
        if !prev_token.is_empty() && imperial_chip_watts(|c| c == prev_token).is_some() {
            // The imperial token is authoritative; leave it to the imperial pass.
            return MetricCode::Absent;
        }
    }
    match TABLE.iter().find(|(code, _)| *code == metric.as_str()) {
        Some((_, w)) => MetricCode::Rated(*w),
        None => MetricCode::Unrecognised,
    }
}

/// The rating of a recognised DIN axial resistor body, from its DIN code.
///
/// The standard IEC/DIN body-to-power mapping. Anything outside it (a bare
/// `R_Axial` with no code, a vertical or cement body, a `Power` variant) carries
/// no size evidence and gets no rating: the codes differ by up to 16x, so a
/// blanket axial default is not a conservative guess in either direction.
fn din_axial_rating(f: &str) -> Option<f64> {
    const TABLE: &[(&str, f64)] = &[
        ("DIN0204", 0.125),
        ("DIN0207", 0.25),
        ("DIN0309", 0.5),
        ("DIN0411", 1.0),
        ("DIN0414", 2.0),
        ("DIN0516", 3.0),
        ("DIN0617", 5.0),
    ];
    TABLE
        .iter()
        .find(|(code, _)| f.contains(code))
        .map(|(_, w)| *w)
}

/// Strip a multi-unit stamping suffix from a device name, yielding the package
/// reference. Multi-unit packages stamp one IR device per unit with a suffix
/// on the reference designator: `_q<N>` (dual BJTs, opamp/comparator channels),
/// `_s<N>` (analog-switch banks), `_e<N>` (passive-array elements). "Q1_q2"
/// and "Q1_q1" are two dice in the one physical package "Q1".
///
/// This is THE unit-suffix rule: the binder uses it to apply a package's
/// ratings to every unit, [`StressMonitor::evaluate`] uses it to pool sibling
/// dissipation through the shared package, and hauksbee-ci mirrors it
/// (`key_belongs_to_ref`) to aggregate per-unit keys under the bare ref. Only
/// an all-digit tail after the marker counts, so "SW1_heater" or "U1_qspi" are
/// left alone (and "SW1" never claims "SW10"'s units).
pub fn strip_unit_suffix(name: &str) -> &str {
    ["_q", "_s", "_e"]
        .into_iter()
        .find_map(|marker| {
            name.rsplit_once(marker)
                .filter(|(_, n)| n.chars().all(|c| c.is_ascii_digit()))
                .map(|(base, _)| base)
        })
        .unwrap_or(name)
}

/// A supply-rail absolute-maximum watch for a part that is not an analog
/// device in the solver (an MCU / logic IC): the part's supply NET is checked
/// against the model's `max_voltage_v` each chunk. These parts deliberately get
/// no whole-device [`DeviceMeta`] (their per-pin currents are covered by the
/// pin-driver metas). Without this check a rail driven far past the chip's
/// absolute-maximum Vcc would raise nothing at all, which is an honesty gap the
/// model DB can close: it carries the rating (e.g. ATmega328P
/// `max_voltage_v = 6.0`).
#[derive(Debug, Clone)]
pub struct SupplyWatch {
    /// Component reference designator the fault names (e.g. "U1").
    pub reference: String,
    /// The supply-net node to sample (the part's VCC/VDD net).
    pub node: NodeId,
    /// Absolute-maximum supply voltage (V) from the model's ratings.
    pub max_v: f64,
}

/// Sustain/raised state for one [`SupplyWatch`] (same filter as continuous
/// device ratings: a solver transient must not cook a chip).
#[derive(Debug, Clone, Default)]
struct WatchTrack {
    over_chunks: u32,
    raised: bool,
}

/// Per-device running state for the sustain filter.
#[derive(Debug, Clone, Default)]
struct DeviceTrack {
    /// Consecutive chunks each continuous fault-kind has been violated.
    over_chunks: HashMap<&'static str, u32>,
    /// Faults already raised for this device (so we don't spam every chunk).
    raised: HashMap<&'static str, bool>,
    /// Live stress fraction (0..1), worst rating utilisation this chunk.
    stress: f64,
    /// Whether the device has been destroyed (destructive mode).
    destroyed: bool,
}

/// The stress monitor: holds device metadata and per-device tracking, evaluates
/// one chunk at a time.
#[derive(Debug, Clone)]
pub struct StressMonitor {
    metas: Vec<DeviceMeta>,
    tracks: Vec<DeviceTrack>,
    /// Supply-rail absolute-maximum watches for MCU/logic packages (no analog
    /// device to meter; the rating is checked against the rail node directly).
    supply_watches: Vec<SupplyWatch>,
    watch_tracks: Vec<WatchTrack>,
    /// Destructive mode: mutate the circuit on fault.
    pub destructive: bool,
    /// Ambient temperature (C) the steady-state junction estimate sits on top
    /// of. Defaults to [`crate::thermal::DEFAULT_AMBIENT_C`] (25 C).
    pub ambient_c: f64,
    /// reference -> live stress fraction (0..1), for component-state frames.
    stress_by_ref: HashMap<String, f64>,
    /// reference -> live estimated junction temperature (C), for the thermal
    /// view / component-state frames. Only populated for dissipating devices.
    temp_by_ref: HashMap<String, f64>,
    /// Per-device dissipated energy (J) integrated over the accepted solver
    /// steps of the chunk about to be evaluated (index-aligned with `metas`).
    /// Drained by [`Self::evaluate`]; empty means "no deposit, fall back to
    /// the endpoint's instantaneous power".
    chunk_energy_j: Vec<f64>,
    /// Simulated time (s) the deposited energy covers. Zero means no deposit.
    chunk_elapsed_s: f64,
    /// Run-cumulative per-device evidence, index-aligned with `metas`.
    run: Vec<RunStats>,
}

/// What the whole run saw of one device, as opposed to one chunk of it.
///
/// A CONTINUOUS current rating is a thermal limit, exactly like the wattage
/// rating the module doc above already time-weights, so it is judged on a
/// first-order lag of |I| rather than on a chunk endpoint: a start-up inrush
/// into bulk capacitance is a real peak that the part's PULSED rating covers,
/// and comparing it against the continuous rating reddens correct boards.
/// A lag, not a run average: an average over however long the user happened to
/// simulate hides a load step that arrives late in a long run.
#[derive(Debug, Clone, Default)]
struct RunStats {
    /// Highest instantaneous |I| (A) any chunk endpoint sampled.
    peak_a: f64,
    /// |I| (A) through a one-pole lag of time constant [`IDM_PULSE_S`]: what
    /// the continuous rating is judged on. Rises to a sustained level within a
    /// few time constants and barely moves for an excursion shorter than one.
    lagged_a: f64,
    /// Whether [`Self::lagged_a`] ever reached the continuous current rating,
    /// which is what separates a real overload from a pulse the card cannot
    /// judge.
    lag_over_limit: bool,
    /// Lowest gate drive (V, folded into N-channel space) seen while a MOSFET
    /// carried a meaningful current. `None` if it never conducted.
    worst_vgs: Option<f64>,
    /// Hottest estimated junction temperature (C). `None` if it never dissipated.
    peak_tj_c: Option<f64>,
}

/// The fraction of its continuous rating at which a MOSFET counts as
/// CONDUCTING for the gate-drive check. Leakage and a decaying gate transient
/// both put microamps through a channel, and judging the datasheet's gate
/// drive against those would qualify every switched-off part on the board.
/// A thousandth of the rating still sits three decades above the IDSS these
/// datasheets quote (25 uA for a 33 A IRF540N), so nothing leaky reaches it,
/// while a part switching a fraction of its rating is judged like any other.
const CONDUCTING_ID_FRACTION: f64 = 1e-3;

/// Time constant (s) of the lag a CONTINUOUS current rating is judged through.
/// MOSFET and diode pulsed ratings are specified at a 300 us pulse width
/// (Infineon IRF540NPbF Rev. 2.1 note 3, and the same convention across AOS,
/// onsemi and Vishay parts), so anything shorter is pulsed by the datasheets'
/// own definition. Rounded up to 1 ms, one order above the solver's chunk.
const IDM_PULSE_S: f64 = 1e-3;

/// Fraction of the specified VGS a gate may fall short by and still count as
/// driven. A gate series resistor's static drop into the pulldown is a few mV,
/// not a drive deficiency, and no datasheet specifies its test VGS finer.
const GATE_DRIVE_TOLERANCE: f64 = 0.02;

impl Default for StressMonitor {
    fn default() -> Self {
        StressMonitor::new(Vec::new())
    }
}

impl StressMonitor {
    /// Build a monitor over the device metadata gathered at bind time.
    pub fn new(metas: Vec<DeviceMeta>) -> Self {
        let n = metas.len();
        StressMonitor {
            metas,
            tracks: vec![DeviceTrack::default(); n],
            supply_watches: Vec::new(),
            watch_tracks: Vec::new(),
            destructive: false,
            ambient_c: crate::thermal::DEFAULT_AMBIENT_C,
            stress_by_ref: HashMap::new(),
            temp_by_ref: HashMap::new(),
            chunk_energy_j: vec![0.0; n],
            chunk_elapsed_s: 0.0,
            run: vec![RunStats::default(); n],
        }
    }

    /// What the run measured about a part that no per-chunk fault check can
    /// say. NONE of these is a fault; each is evidence attached to the result.
    ///
    /// * A card's `min_current_a` never reached: the model regulates where the
    ///   real part is not guaranteed to. MEASURED, so it undermines.
    /// * A MOSFET conducting below its card's `rds_on_vgs`: the RDS(on) the
    ///   model used is an extrapolation past the last gate voltage the vendor
    ///   tested, so it qualifies; below `vgs_th_max` the datasheet does not
    ///   guarantee the part is on at all, so that undermines.
    /// * A current peak above a continuous rating on a card with NO pulsed
    ///   rating to judge it against: qualified, naming how long it lasted.
    pub fn run_disclosures(&self) -> Vec<Assumption> {
        let mut out = Vec::new();
        for (m, s) in self.metas.iter().zip(&self.run) {
            let (r, rt) = (&m.reference, &m.ratings);
            if let Some(floor) = rt.min_current_a.filter(|f| s.peak_a < *f) {
                let (ma, floor_ma) = (s.peak_a * 1e3, floor * 1e3);
                out.push(Assumption::operating_limit_violated(
                    &format!("{r}-min-current"),
                    r,
                    &format!(
                        "{r} carries {ma:.2} mA, below the {floor_ma:.2} mA its datasheet card \
                         requires for regulation, so the voltage it holds here is not guaranteed"
                    ),
                    // Neutral about WHICH network feeds it: a shunt reference is fed
                    // by its series resistor, a series regulator by its load and its
                    // divider, and naming only the shunt's cure misdirects the other.
                    "raise its current above the card's minimum (a preload resistor, or a \
                     lower-value feed network), then re-run",
                ));
            }
            let under_drive = s
                .worst_vgs
                .zip(rt.rds_on_vgs)
                .filter(|(vgs, spec)| *vgs < spec * (1.0 - GATE_DRIVE_TOLERANCE));
            if let Some((vgs, spec)) = under_drive {
                let th = rt.vgs_th_max.unwrap_or(f64::NEG_INFINITY);
                let (id, fix) = (format!("{r}-gate-drive"), "drive the gate to at least the VGS the card specifies RDS(on) at (a gate driver or a level shifter), or fit a logic-level part, then re-run");
                let gap = format!("{r} conducts at VGS = {vgs:.2} V, below the {spec:.2} V its datasheet specifies RDS(on) at, so its on-resistance at this drive is not guaranteed by the datasheet");
                out.push(if vgs < th {
                    Assumption::operating_limit_violated(&id, r, &format!("{gap}; {vgs:.2} V is also below its {th:.2} V maximum VGS(th), so the datasheet does not guarantee it is on at all"), fix)
                } else {
                    Assumption::partial_model_for_component(&id, r, "", &gap, fix)
                });
            }
            // The instantaneous peak went over the continuous rating but the
            // lag never did, so no fault was raised: honest only while the card
            // carries no pulsed rating to judge that peak against. A lag over
            // the rating is the continuous fault's business, not a note's.
            if let Some(cont) = rt
                .max_current_a
                .filter(|c| !s.lag_over_limit && rt.max_surge_current_a.is_none() && s.peak_a > *c)
            {
                let pk = s.peak_a;
                out.push(Assumption::partial_model_for_component(&format!("{r}-peak-current"), r, "",
                    &format!("{r} peaks at {pk:.3} A, above the {cont:.3} A its card rates CONTINUOUSLY, and the card carries no pulsed (IDM) rating for the peak to be judged against"),
                    "add the datasheet's pulsed drain/forward current to the card, then re-run"));
            }
        }
        out
    }

    /// A junction temperature judged on a theta_JA inferred from the package,
    /// because the card carries none, qualifies each claim it feeds.
    /// `claims(reference, decisive)` names those claims' scopes; `decisive` is
    /// true when twice the inferred theta_JA would put the peak at or over the
    /// part's Tj limit, so a verdict drawn from the limit rests on the guess.
    pub fn inferred_theta_ja(&self, claims: impl Fn(&str, bool) -> Vec<Scope>) -> Vec<Assumption> {
        let mut out = Vec::new();
        for (m, s) in self.metas.iter().zip(&self.run) {
            let Some(peak) = s.peak_tj_c.filter(|_| m.ratings.theta_ja_c_per_w.is_none()) else {
                continue;
            };
            let (r, theta, limit) = (&m.reference, m.theta_ja_c_per_w(), m.tj_max_c());
            let decisive = 2.0 * peak - self.ambient_c >= limit;
            let key = format!("{r}-theta-ja");
            let text = format!("{r}'s junction temperature (peak {peak:.1} C)");
            let how = format!(
                "a theta_JA of {theta:.0} C/W inferred from its package ('{}'), because its card \
                 carries no theta_ja_c_per_w",
                m.footprint
            );
            let fix = format!("add the datasheet's theta_ja_c_per_w to {r}'s card, then re-run");
            out.extend(claims(r, decisive).into_iter().map(|scope| {
                let subject = Subject::new(&key, &text);
                Assumption::reduced_fidelity(AssumptionSource::Binder, subject, scope, &how, &fix)
            }));
        }
        out
    }

    /// Every device whose overpower check could not run because its package was
    /// unreadable, in message form. Deduped and order-stable, the same discipline
    /// as the scheduler's coverage messages, so a CI report can chain it straight
    /// into `coverage_warnings`.
    ///
    /// [`crate::binder::BoundBoard::power_coverage_gaps`] is the same list for
    /// callers holding a bound board rather than a live monitor; between them the
    /// CI report, the evidence map and the TUI all carry these.
    pub fn power_coverage_gaps(&self) -> Vec<String> {
        let mut gaps = aggregate_power_coverage_gaps(&self.metas);
        // A MOSFET that conducted but whose card states no VGS for its RDS(on)
        // is NOT CHECKED for gate-drive adequacy, and an abstention nobody can
        // see is indistinguishable from a pass.
        gaps.extend(self.metas.iter().zip(&self.run)
            .filter(|(m, s)| s.worst_vgs.is_some() && m.ratings.rds_on_vgs.is_none())
            .map(|(m, _)| format!("stress: {} conducts, but its model card states no VGS for its RDS(on), so its gate-drive adequacy was not checked. Add ratings.rds_on_vgs and ratings.vgs_th_max from the datasheet to cover it.", m.reference)));
        gaps
    }

    /// Number of monitored devices.
    pub fn device_count(&self) -> usize {
        self.metas.len()
    }

    /// Install the supply-rail watches (built at bind time from the MCU
    /// bindings' VCC nets + model ratings). Replaces any existing set.
    pub fn set_supply_watches(&mut self, watches: Vec<SupplyWatch>) {
        self.watch_tracks = vec![WatchTrack::default(); watches.len()];
        self.supply_watches = watches;
    }

    /// Clear all per-run tracking so a replay starts from a clean slate: the
    /// consecutive-over-limit counters, the already-raised set (so a fault that
    /// fired last run can fire again), the live stress fractions, and the
    /// destroyed flags. The device metadata (`metas`) and the `destructive` /
    /// `ambient_c` config are preserved, only the accumulated run state resets.
    /// The circuit itself is restored separately by the scheduler (this monitor
    /// does not own it), so clearing `destroyed` here stays consistent with a
    /// pristine circuit only when that restore also happens.
    pub fn reset_tracks(&mut self) {
        for track in &mut self.tracks {
            *track = DeviceTrack::default();
        }
        for track in &mut self.watch_tracks {
            *track = WatchTrack::default();
        }
        self.stress_by_ref.clear();
        self.temp_by_ref.clear();
        self.clear_chunk_energy();
    }

    /// Per-device dissipation (W) at one solved operating point, index-aligned
    /// with the monitored metas (destroyed devices read 0). This is the
    /// per-accepted-step half of the time-weighted thermal path: the streaming
    /// sink calls it at each accepted step, integrates the powers over the
    /// step widths, and hands the chunk's total to
    /// [`Self::deposit_chunk_energy`]. Split from the deposit so the sink can
    /// run under a shared borrow of the scheduler while the march owns the
    /// circuit.
    pub fn step_powers(
        &self,
        circuit: &Circuit,
        node_v: &dyn Fn(NodeId) -> f64,
        branch_current: &dyn Fn(DeviceId) -> Option<f64>,
    ) -> Vec<f64> {
        self.metas
            .iter()
            .enumerate()
            .map(|(i, meta)| {
                if self.tracks[i].destroyed {
                    0.0
                } else {
                    operating_point(circuit, meta, node_v, branch_current).power_w
                }
            })
            .collect()
    }

    /// Raise `chk` as a fault on device `i`, at most once per fault kind for
    /// the life of the run, applying the destructive consequence when
    /// destructive mode is on. Returns whether this call destroyed the device
    /// (`false` for a kind already raised, which is a no-op).
    fn raise_once(
        &mut self,
        i: usize,
        meta: &DeviceMeta,
        chk: &Check,
        t: f64,
        circuit: &mut Circuit,
        faults: &mut Vec<FaultEvent>,
    ) -> bool {
        if self.tracks[i]
            .raised
            .get(chk.kind.as_str())
            .copied()
            .unwrap_or(false)
        {
            return false;
        }
        self.tracks[i].raised.insert(chk.kind.as_str(), true);
        let destroyed = self.maybe_destroy(circuit, meta, chk.kind);
        if destroyed {
            self.tracks[i].destroyed = true;
        }
        faults.push(FaultEvent {
            component: meta.reference.clone(),
            kind: chk.kind,
            value: chk.value,
            limit: chk.limit,
            t,
            destroyed,
        });
        destroyed
    }

    /// Add integrated per-device dissipated energy (J, index-aligned with the
    /// metas) covering `elapsed_s` seconds of accepted solver steps to the
    /// pending chunk deposit. The next [`Self::evaluate`] turns the deposit
    /// into time-weighted average power for the junction-temperature check and
    /// drains it. Additive so a subdivided chunk (fallback ladder rung 4)
    /// deposits each quarter's energy in turn. Non-finite or non-positive
    /// `elapsed_s` deposits nothing.
    /// A slice that does not cover every meta deposits nothing: advancing
    /// `elapsed` while some device's energy silently stays zero would report
    /// a fabricated 0 W average for it instead of falling back to the
    /// endpoint operating point.
    pub fn deposit_chunk_energy(&mut self, energy_j: &[f64], elapsed_s: f64) {
        if !(elapsed_s > 0.0) || !elapsed_s.is_finite() {
            return;
        }
        if energy_j.len() != self.chunk_energy_j.len() {
            return;
        }
        for (slot, e) in self.chunk_energy_j.iter_mut().zip(energy_j) {
            *slot += e;
        }
        self.chunk_elapsed_s += elapsed_s;
    }

    /// Convenience for direct drivers (tests, chunk-less callers): integrate
    /// one accepted step of width `dt` at the given solved state into the
    /// pending chunk deposit, rectangle rule (`power(now) * dt`). Exact for
    /// piecewise-constant waveforms whose switches land on accepted steps,
    /// which is how the co-sim stamps firmware pin edges.
    pub fn accumulate_step(
        &mut self,
        circuit: &Circuit,
        node_v: &dyn Fn(NodeId) -> f64,
        branch_current: &dyn Fn(DeviceId) -> Option<f64>,
        dt: f64,
    ) {
        if !(dt > 0.0) || !dt.is_finite() {
            return;
        }
        let powers = self.step_powers(circuit, node_v, branch_current);
        self.deposit_chunk_energy(&powers.iter().map(|p| p * dt).collect::<Vec<f64>>(), dt);
    }

    /// Zero the pending chunk energy deposit.
    fn clear_chunk_energy(&mut self) {
        for e in &mut self.chunk_energy_j {
            *e = 0.0;
        }
        self.chunk_elapsed_s = 0.0;
    }

    /// Live stress fraction per component reference (0..1).
    pub fn stress_by_ref(&self) -> &HashMap<String, f64> {
        &self.stress_by_ref
    }

    /// Live estimated steady-state junction temperature (C) per component
    /// reference, for dissipating devices.
    pub fn temp_by_ref(&self) -> &HashMap<String, f64> {
        &self.temp_by_ref
    }

    /// Evaluate every monitored device for the chunk just solved.
    ///
    /// `node_v(node)` returns the node voltage; `branch_current(id)` returns the
    /// branch current for a `Vsource`/`Inductor` device if the layout owns one.
    /// `t` is the current sim time. Returns any faults newly raised this chunk.
    pub fn evaluate(
        &mut self,
        circuit: &mut Circuit,
        node_v: &dyn Fn(NodeId) -> f64,
        branch_current: &dyn Fn(DeviceId) -> Option<f64>,
        t: f64,
    ) -> Vec<FaultEvent> {
        let mut faults = Vec::new();

        // ── Pass 1: operating points, then pooled per-package dissipation ────
        // Multi-unit packages stamp one IR device (and one meta) per unit, but
        // the dice share ONE package: one mould compound, one leadframe, one
        // junction→ambient path. theta_JA is a property of that shared path,
        // so the temperature-driving power is the SUM of every live sibling's
        // dissipation, evaluating each unit against only its own power
        // under-reads Tj by exactly the siblings' share (a dual BJT with both
        // halves at 0.23 W in a ~440 C/W SOT-363 really sits near 126 C, not
        // the 76 C a single unit's power suggests) and silently missed real
        // overtemperature faults. Group by the stripped package reference
        // ([`strip_unit_suffix`]); a single-unit part pools to exactly its own
        // power, so nothing changes for singletons. Destroyed units no longer
        // dissipate and are excluded.
        //
        // Sampling every operating point up front (before pass 2 can mutate
        // the circuit destructively) also means every check this chunk sees
        // the one solved state; sampling lazily would let a device evaluated
        // after an earlier device's same-chunk destruction see mutated
        // parameters under stale node voltages. The next chunk re-solves
        // either way.
        let ops: Vec<Option<OperatingPoint>> = self
            .metas
            .iter()
            .enumerate()
            .map(|(i, meta)| {
                if self.tracks[i].destroyed {
                    None
                } else {
                    Some(operating_point(circuit, meta, node_v, branch_current))
                }
            })
            .collect();
        // Run-cumulative evidence. The lag is stepped with the exact solution
        // of dI/dt = (i - I)/tau over the chunk, so a variable chunk length
        // changes nothing about what it reports: that is what turns the
        // continuous current rating from an endpoint sample into a thermal one.
        // No deposit means no timebase for this chunk, so the endpoint stands
        // in for the lag exactly as it stands in for the deposited power below.
        let alpha = if self.chunk_elapsed_s > 0.0 {
            1.0 - (-self.chunk_elapsed_s / IDM_PULSE_S).exp()
        } else {
            1.0
        };
        for (i, op) in ops.iter().enumerate() {
            let Some(op) = op else { continue };
            let limit = self.metas[i].ratings.max_current_a;
            let run = &mut self.run[i];
            run.peak_a = run.peak_a.max(op.current_a);
            run.lagged_a += (op.current_a - run.lagged_a) * alpha;
            run.lag_over_limit |= limit.is_some_and(|c| run.lagged_a > c);
            let conducting = op.current_a > limit.unwrap_or(0.0) * CONDUCTING_ID_FRACTION;
            if let Some(v) = op.vgs.filter(|_| conducting) {
                run.worst_vgs = Some(run.worst_vgs.map_or(v, |w: f64| w.min(v)));
            }
        }
        // Heating power per device. When the scheduler deposited accepted-step
        // energy for this chunk, the physical figure is the TIME-WEIGHTED
        // average `energy / elapsed`: a firmware PWM waveform switches inside
        // the chunk, so the endpoint's instantaneous power reads peak or zero
        // depending on phase, while the junction (and a resistor's continuous
        // wattage rating, which is the same thermal physics) responds to the
        // duty-cycle average. This average feeds BOTH the per-unit Overpower
        // check and, pooled per package, the junction-temperature check.
        // Without a deposit (direct unit-test drives), the endpoint operating
        // point is the fallback, exact for waveforms constant over the chunk.
        let thermal_elapsed = self.chunk_elapsed_s;
        let unit_avg_w: Vec<f64> = self
            .metas
            .iter()
            .enumerate()
            .map(|(i, _)| {
                if ops[i].is_none() {
                    0.0 // destroyed: dissipates nothing
                } else if thermal_elapsed > 0.0 {
                    self.chunk_energy_j.get(i).copied().unwrap_or(0.0) / thermal_elapsed
                } else {
                    ops[i].as_ref().map_or(0.0, |op| op.power_w)
                }
            })
            .collect();
        let mut package_power: HashMap<String, f64> = HashMap::new();
        for (i, meta) in self.metas.iter().enumerate() {
            if unit_avg_w[i] > 0.0 {
                *package_power
                    .entry(strip_unit_suffix(&meta.reference).to_string())
                    .or_insert(0.0) += unit_avg_w[i];
            }
        }
        // The deposit covers exactly the chunk this evaluate closes; drain it
        // so the next chunk starts from zero (no cross-chunk carry-over).
        self.clear_chunk_energy();

        // ── Pass 2: per-device checks ────────────────────────────────────────
        // Iterate by index so we can borrow tracks mutably alongside metas.
        for i in 0..self.metas.len() {
            let meta = self.metas[i].clone();
            let Some(op) = &ops[i] else {
                // Destroyed devices stay at full stress and raise nothing more.
                // Their junction stops dissipating, so a previously-reported
                // temperature cools to ambient rather than freezing at the
                // last hot reading.
                self.stress_by_ref.insert(meta.reference.clone(), 1.0);
                if let Some(t) = self.temp_by_ref.get_mut(&meta.reference) {
                    *t = self.ambient_c;
                }
                continue;
            };
            // Checks judge the endpoint operating point, EXCEPT power: a
            // continuous wattage rating is thermal, so it compares the same
            // time-weighted average the junction estimate uses (endpoint
            // fallback when no deposit exists). Endpoint power would make a
            // sub-chunk PWM part's Overpower verdict depend on pulse phase.
            let peak_a = op.current_a;
            let op = OperatingPoint {
                current_a: self.run[i].lagged_a,
                power_w: unit_avg_w[i],
                ..*op
            };
            let op = &op;
            let mut checks = build_checks(&meta, op, peak_a);

            // Thermal: turn the package's pooled dissipation into a
            // steady-state junction temperature and check it against the
            // device's max Tj. Treated as a continuous rating so a
            // switching-edge power spike does not trip it.
            //
            // Note the deliberate asymmetry with Overpower above: max_power_w
            // is a per-UNIT rating in the model DB (bjt.toml's dual-pair
            // entries note "this entry models a single transistor in the
            // pair" and comment the ratings "per transistor"), so Overpower
            // compares each unit's own dissipation against it, pooling there
            // would false-trip a package whose halves are individually fine.
            // Tj is the opposite: the heat path is shared, so only the pooled
            // figure is physical.
            //
            // Every unit row reports the shared package Tj (to first order the
            // dice sit at one temperature), because per-unit keys are what the
            // CI max_temp aggregation and the UI heat-map already consume, a
            // synthetic package-level row would be a key no consumer looks up.
            // Consequently each sibling raises its own Overtemperature fault
            // on the same chunk: both junctions genuinely are over-limit, and
            // CI's fault matching names units, not bare package refs.
            let package_w = package_power
                .get(strip_unit_suffix(&meta.reference))
                .copied()
                .unwrap_or(0.0);
            if package_w > 0.0 {
                let tj = crate::thermal::junction_temp_c(
                    self.ambient_c,
                    package_w,
                    meta.theta_ja_c_per_w(),
                );
                self.temp_by_ref.insert(meta.reference.clone(), tj);
                let peak = &mut self.run[i].peak_tj_c;
                *peak = Some(peak.map_or(tj, |p| p.max(tj)));
                checks.push(Check {
                    kind: FaultKind::Overtemperature,
                    value: tj,
                    limit: meta.tj_max_c(),
                    surge: false,
                });
            } else if let Some(t) = self.temp_by_ref.get_mut(&meta.reference) {
                // A package that stopped dissipating settles back to ambient
                // in the steady-state model; freezing the last hot reading
                // would report a temperature no longer supported by any power.
                *t = self.ambient_c;
            }

            let mut worst_stress = 0.0f64;
            for chk in &checks {
                let frac = if chk.kind == FaultKind::Overtemperature {
                    thermal_stress_frac(chk.value, chk.limit, self.ambient_c)
                } else if chk.limit > 0.0 {
                    (chk.value / chk.limit).abs()
                } else {
                    0.0
                };
                worst_stress = worst_stress.max(frac);

                if chk.surge {
                    // Surge ceiling: trips instantly.
                    if frac > 1.0 + OVER_LIMIT_NOISE_MARGIN {
                        self.raise_once(i, &meta, chk, t, circuit, &mut faults);
                    }
                    continue;
                }

                // Continuous rating: needs to be sustained.
                let counter = self.tracks[i]
                    .over_chunks
                    .entry(chk.kind.as_str())
                    .or_insert(0);
                if frac > 1.0 + OVER_LIMIT_NOISE_MARGIN {
                    *counter += 1;
                } else {
                    *counter = 0;
                }
                if *counter >= SUSTAIN_CHUNKS
                    && self.raise_once(i, &meta, chk, t, circuit, &mut faults)
                {
                    break;
                }
            }

            self.tracks[i].stress = worst_stress.min(1.0);
            self.stress_by_ref
                .insert(meta.reference.clone(), worst_stress.min(1.0));
        }

        // ── Pass 3: supply-rail absolute-maximum watches (MCU/logic Vcc) ─────
        // These parts have no analog device to meter; the honest check is the
        // rail node's voltage against the model's absolute-maximum supply
        // rating. Same sustain filter as any continuous rating.
        for i in 0..self.supply_watches.len() {
            let w = self.supply_watches[i].clone();
            let v = node_v(w.node);
            let frac = if w.max_v > 0.0 {
                (v / w.max_v).max(0.0)
            } else {
                0.0
            };
            // The package's stress heat-map entry: keep the worst of the pin
            // metas' fraction (keyed "<ref>:<pin>") and this rail fraction
            // (keyed on the bare ref, which the UI heat-map reads).
            let entry = self.stress_by_ref.entry(w.reference.clone()).or_insert(0.0);
            *entry = entry.max(frac.min(1.0));
            let track = &mut self.watch_tracks[i];
            if frac > 1.0 + OVER_LIMIT_NOISE_MARGIN {
                track.over_chunks += 1;
            } else {
                track.over_chunks = 0;
            }
            if track.over_chunks >= SUSTAIN_CHUNKS && !track.raised {
                track.raised = true;
                faults.push(FaultEvent {
                    component: w.reference,
                    kind: FaultKind::Overvoltage,
                    value: v,
                    limit: w.max_v,
                    t,
                    // Chip-cooking is not modeled destructively; the fault
                    // reports, the circuit is left intact.
                    destroyed: false,
                });
            }
        }
        faults
    }

    /// In destructive mode, mutate the circuit to enact the failure. Returns
    /// whether the device was destroyed. `kind` is the tripping fault, because a
    /// diode's destructive consequence depends on it: over-current burns the
    /// junction OPEN, while reverse over-voltage past breakdown
    /// (avalanche/punch-through) fails it CLOSED.
    fn maybe_destroy(&self, circuit: &mut Circuit, meta: &DeviceMeta, kind: FaultKind) -> bool {
        if !self.destructive {
            return false;
        }
        let idx = meta.device.0 as usize;
        let Some(dev) = circuit.devices.get_mut(idx) else {
            return false;
        };
        match dev {
            // Resistor / fuse: opens (fusible failure).
            Device::Resistor { ohms, .. } => {
                *ohms = 1e12;
                true
            }
            // Diode / LED: over-current opens the junction; reverse over-voltage
            // shorts it. Replace the diode with a resistor across its nodes (the
            // device count / layout is unchanged) whose value encodes which
            // failure occurred: a near-open for over-current, a small series
            // short for reverse breakdown.
            Device::Diode { name, a, k, .. } => {
                let (name, a, k) = (name.clone(), *a, *k);
                let ohms = match kind {
                    FaultKind::Overvoltage => 1e-2, // reverse breakdown fails CLOSED
                    _ => 1e12,                      // over-current burns OPEN
                };
                *dev = Device::resistor(name, a, k, ohms);
                true
            }
            _ => false,
        }
    }
}

/// The live operating point of a device this chunk.
struct OperatingPoint {
    /// Gate drive (V) folded into N-channel space. `None` for a non-MOSFET.
    vgs: Option<f64>,
    /// Through-current magnitude (A).
    current_a: f64,
    /// Across-voltage, signed (V): for diodes, anode−cathode; for caps, the
    /// terminal voltage; for two-terminals generally `Va − Vb`.
    voltage_v: f64,
    /// Power dissipation (W).
    power_w: f64,
}

impl OperatingPoint {
    /// A two-terminal point: `|I|`, the signed across-voltage, dissipation.
    fn new(current_a: f64, voltage_v: f64, power_w: f64) -> Self {
        Self {
            vgs: None,
            current_a,
            voltage_v,
            power_w,
        }
    }
}

/// Compute a device's operating point from the chunk's solved state.
fn operating_point(
    circuit: &Circuit,
    meta: &DeviceMeta,
    node_v: &dyn Fn(NodeId) -> f64,
    branch_current: &dyn Fn(DeviceId) -> Option<f64>,
) -> OperatingPoint {
    let dev = circuit.devices.get(meta.device.0 as usize);
    match dev {
        Some(Device::Resistor { a, b, ohms, .. }) => {
            let v = node_v(*a) - node_v(*b);
            let i = if *ohms > 0.0 { v / *ohms } else { 0.0 };
            OperatingPoint::new(i.abs(), v, (v * i).abs())
        }
        Some(Device::Diode { a, k, model, .. }) => {
            let vd = node_v(*a) - node_v(*k);
            let id = diode_current(model, vd, circuit.temp_c);
            OperatingPoint::new(id.abs(), vd, (vd * id).abs())
        }
        Some(Device::Capacitor { a, b, .. }) => {
            // An ideal capacitor's through-current is displacement current,
            // it needs dv/dt across chunks, not one voltage sample, and it
            // dissipates no real power. Every capacitor check (over-voltage,
            // reverse bias) is voltage-based, so the zeros disable nothing.
            let v = node_v(*a) - node_v(*b);
            OperatingPoint::new(0.0, v, 0.0)
        }
        Some(Device::Inductor { a, b, .. }) => {
            // The winding current lives in the inductor's branch unknown
            // (like a Vsource's), not in a node-voltage difference, without
            // it the surge-current check could never fire for a coil. Power
            // stays zero: an ideal inductor *stores* v·i rather than
            // dissipating it, and reporting it as heat would false-trip the
            // power-gated thermal check on every energised coil.
            let i = branch_current(meta.device).unwrap_or(0.0);
            OperatingPoint::new(i.abs(), node_v(*a) - node_v(*b), 0.0)
        }
        Some(Device::Bjt { c, b, e, model, .. }) => {
            // Gummel-Poon transport at the sampled node voltages, polarity
            // folded; the same equations the solver stamps, so the monitor
            // sees the operating point the solve actually settled at.
            let sign = match model.polarity {
                hauksbee_ir::Polarity::N => 1.0,
                hauksbee_ir::Polarity::P => -1.0,
            };
            let vt = hauksbee_ir::thermal_voltage_c(circuit.temp_c);
            // Temperature-corrected saturation current, consistent with the
            // temp-corrected Vt above and with the solver's stamp (which pairs
            // `is_at(t)` with the temp-corrected thermal voltage).
            let is = model.is_at(circuit.temp_c);
            let vbe = sign * (node_v(*b) - node_v(*e));
            let vbc = sign * (node_v(*b) - node_v(*c));
            let ex = |v: f64, n: f64| ((v / (n * vt)).clamp(-100.0, 200.0)).exp();
            let cf = is * (ex(vbe, model.nf) - 1.0);
            let cr = is * (ex(vbc, model.nr) - 1.0);
            let ic = (cf - cr) - cr / model.br;
            let ib = cf / model.bf + cr / model.br;
            let vce = node_v(*c) - node_v(*e);
            let i_worst = ic.abs().max(ib.abs()).min(1e3);
            OperatingPoint::new(
                i_worst,
                vce,
                (vce * ic).abs().min(1e6) + (sign * vbe * ib).abs().min(1e6),
            )
        }
        Some(Device::VSwitch {
            a,
            b,
            ctrl_p,
            ctrl_n,
            von,
            ron,
            roff,
            ..
        }) => {
            // Channel current through the switch at its present state.
            let vc = node_v(*ctrl_p) - node_v(*ctrl_n);
            let r = if vc >= *von { *ron } else { *roff };
            let v = node_v(*a) - node_v(*b);
            let i = (v / r.max(1e-3)).abs();
            OperatingPoint::new(i, v, v.abs() * i)
        }
        Some(Device::Mosfet {
            d, g, s, b, model, ..
        }) => {
            // Shichman-Hodges level-1 channel at the sampled node voltages,
            // the same blended-overdrive equations the solver stamps (see
            // `mos_channel` in hauksbee-solve), so the monitor sees the
            // current the simulated channel actually carries. This arm used
            // to hardcode current/power to zero, which silently disabled the
            // Overcurrent, Overpower, and power-gated Overtemperature checks
            // for every MOSFET.
            //
            // Fold polarity into N-channel space and let the higher terminal
            // act as the drain (the level-1 channel is symmetric; the solver
            // performs the same swap).
            let sign = match model.polarity {
                hauksbee_ir::Polarity::N => 1.0,
                hauksbee_ir::Polarity::P => -1.0,
            };
            let mut vd = sign * node_v(*d);
            let vg = sign * node_v(*g);
            let mut vs = sign * node_v(*s);
            if vd < vs {
                std::mem::swap(&mut vd, &mut vs);
            }
            let vgs = vg - vs;
            let vds_f = vd - vs;

            // Body-effect threshold shift, matching the solver's expression.
            // `gamma == 0` (most models) never reads the bulk voltage.
            let mut vth = model.vto;
            if model.gamma > 0.0 {
                if let Some(bulk) = b {
                    let phi = model.phi.max(1e-6);
                    let vbs = sign * node_v(*bulk) - vs;
                    let arg = (phi - vbs).max(0.0);
                    vth = model.vto + model.gamma * (arg.sqrt() - phi.sqrt());
                }
            }

            // Blended overdrive `vov_eff = 2nVt·softplus(vov/(2nVt))`: the
            // square law above threshold, an exponential subthreshold tail
            // below (see `mos_channel` for why the blend, not two branches).
            let vt = hauksbee_ir::thermal_voltage_c(circuit.temp_c);
            let two_nvt = 2.0 * model.n_sub.max(1.0) * vt;
            let u = (vgs - vth) / two_nvt;
            // Numerically stable softplus ln(1 + e^u).
            let softplus = if u > 40.0 {
                u
            } else if u < -40.0 {
                u.exp()
            } else {
                u.exp().ln_1p()
            };
            let vov_eff = two_nvt * softplus;
            // Channel-length modulation is always applied here (the solver
            // gates it on a sim option the monitor cannot see); lambda is 0
            // for most models, and when it isn't, including it errs toward
            // the slightly *higher* current, conservative for a limit check.
            let clm = 1.0 + model.lambda * vds_f;
            let ids = if vds_f < vov_eff {
                // Triode.
                model.beta() * (vov_eff * vds_f - 0.5 * vds_f * vds_f) * clm
            } else {
                // Saturation.
                0.5 * model.beta() * vov_eff * vov_eff * clm
            };
            // Report the real (unfolded) drain-source voltage; the fold and
            // swap preserve its magnitude, so |vds·ids| is the channel
            // dissipation either way. Clamps mirror the BJT arm.
            let vds = node_v(*d) - node_v(*s);
            // `vgs` rides along folded: it is what decides whether the card's
            // RDS(on) applies at all, and only this arm can compute it.
            OperatingPoint {
                vgs: Some(vgs),
                ..OperatingPoint::new(ids.abs().min(1e3), vds, (vds_f * ids).abs().min(1e6))
            }
        }
        Some(Device::Vsource { .. }) => {
            // Supply / regulator output leg: the sourced current is the
            // branch unknown. Voltage and power stay zero ON PURPOSE; this
            // IR device is the regulator's ideal *output* source only. Its
            // across-voltage is its own setpoint (checking the rail against
            // itself is meaningless), and the real pass-element dissipation
            // is (Vin − Vout)·I, which needs the input node this device does
            // not carry. Only the Overcurrent check applies (see
            // `build_checks`'s Vreg arm).
            let i = branch_current(meta.device).unwrap_or(0.0);
            OperatingPoint::new(i.abs(), 0.0, 0.0)
        }
        _ => OperatingPoint::new(0.0, 0.0, 0.0),
    }
}

/// One limit check: value vs limit, flagged surge or continuous.
struct Check {
    kind: FaultKind,
    value: f64,
    limit: f64,
    surge: bool,
}

/// Build the applicable limit checks for a device's operating point.
///
/// `op.current_a` is the [`IDM_PULSE_S`] LAG, which is what a continuous
/// rating means; `peak_a` is the instantaneous sample, which is what the
/// pulsed/surge ceiling means. Handing the peak to a continuous rating is how
/// an inrush into bulk capacitance reddens a board whose settled current is
/// comfortably inside its part's rating.
fn build_checks(meta: &DeviceMeta, op: &OperatingPoint, peak_a: f64) -> Vec<Check> {
    let mut checks = Vec::new();
    let r = &meta.ratings;

    // Surge current (instantaneous ceiling), for any device with a surge spec.
    if let Some(surge) = r.max_surge_current_a {
        checks.push(Check {
            kind: FaultKind::SurgeCurrent,
            value: peak_a,
            limit: surge,
            surge: true,
        });
    }

    let mut check = |kind, value, limit: Option<f64>| {
        if let Some(limit) = limit {
            checks.push(Check {
                kind,
                value,
                limit,
                surge: false,
            });
        }
    };
    // Only the reverse magnitude counts against a blocking-voltage rating.
    let reverse_v = (-op.voltage_v).max(0.0);

    match meta.kind {
        ComponentKind::Diode => {
            check(FaultKind::Overcurrent, op.current_a, r.max_current_a);
            check(FaultKind::Overvoltage, reverse_v, r.max_voltage_v);
        }
        ComponentKind::Passive => {
            // Resistor power (rated or footprint-derived).
            check(FaultKind::Overpower, op.power_w, meta.power_rating_w());
            // Continuous current rating; the natural home for an inductor's
            // rated / saturation current. Skip it for passives and a coil's
            // steady-state current limit goes silently unenforced (an inductor's
            // power_w is 0, so Overpower/Overtemperature are dead there too).
            check(FaultKind::Overcurrent, op.current_a, r.max_current_a);
            // Polarized capacitor reverse bias: any reverse beyond ~0.5 V.
            check(
                FaultKind::ReverseBias,
                reverse_v,
                r.polarized.then_some(0.5),
            );
            check(FaultKind::Overvoltage, op.voltage_v.abs(), r.max_voltage_v);
        }
        ComponentKind::BjtNpn
        | ComponentKind::BjtPnp
        | ComponentKind::Nmos
        | ComponentKind::Pmos => {
            check(FaultKind::Overcurrent, op.current_a, r.max_current_a);
            check(FaultKind::Overvoltage, op.voltage_v.abs(), r.max_voltage_v);
            check(FaultKind::Overpower, op.power_w, r.max_power_w);
        }
        ComponentKind::Vreg => check(FaultKind::Overcurrent, op.current_a, r.max_current_a),
        // An analog switch is metered per package; the kinds after it get PER-PIN metas, not a package meta: an MCU or logic IC
        // has no single through-current, but every pin it drives is stamped as a
        // Thevenin PinDriver whose hidden Vsource's branch unknown IS that pin's
        // source/sink current. The binder monitors each driver Vsource
        // (reference "<ref>:<pin>", see `gather_device_meta`), so `op.current_a`
        // here is a genuine pin current and this check fires on a real per-pin
        // violation.
        ComponentKind::AnalogSwitch
        | ComponentKind::Mcu
        | ComponentKind::Digital
        | ComponentKind::ShiftRegister
        | ComponentKind::Dac
        | ComponentKind::Adc => check(FaultKind::PinOvercurrent, op.current_a, r.max_pin_current_a),
        _ => {}
    }
    checks
}

/// Forward current from the diode's TERMINAL voltage `vd` at temperature
/// `temp_c`, clamped to a sane range so a runaway forward bias does not
/// overflow.
///
/// The terminal voltage is not the junction voltage when the model carries a
/// series resistance: the junction sits on an intrinsic anode and `rs` bridges
/// it out, so the terminals read `vj + i(vj)*rs`. Feeding that straight into
/// the exponential reports a current the device is not carrying, and this
/// monitor is what raises over-current and over-power faults, so the error
/// lands directly on a verdict. A red LED at `rs = 6` reads 81 A instead of
/// 56 mA that way, because the exponential turns a 340 mV offset into three
/// orders of magnitude.
///
/// So recover the junction first, solving `vj + i(vj)*rs = vd` for `vj`.
/// Newton on a monotone scalar, started from the terminal voltage, which is an
/// upper bound: a handful of iterations and no allocation.
fn diode_current(model: &hauksbee_ir::DiodeModel, vd: f64, temp_c: f64) -> f64 {
    let vt = hauksbee_ir::thermal_voltage_c(temp_c) * model.n;
    if vt <= 0.0 {
        return 0.0;
    }
    if model.rs > 0.0 && vd > 0.0 {
        let is = model.is_at(temp_c);
        let mut vj = vd;
        for _ in 0..60 {
            let e = (vj / vt).clamp(-100.0, 200.0).exp();
            let i = is * (e - 1.0);
            let g = is * e / vt;
            let df = 1.0 + g * model.rs;
            if !df.is_finite() || df <= 0.0 {
                break;
            }
            let step = (vj + i * model.rs - vd) / df;
            vj -= step;
            if step.abs() < 1e-12 {
                break;
            }
        }
        let e = (vj / vt).clamp(-100.0, 200.0).exp();
        return (is * (e - 1.0)).clamp(-1e3, 1e3);
    }
    // Forward: Shockley, matching the solver's diode_eval (which never clamps
    // an accepted junction voltage: real LEDs sit at vd/nVt > 40, and a 40
    // clamp silently caps the computed current far below the real one).
    // Reverse beyond breakdown: small leakage (ignored).
    let exp_arg = (vd / vt).clamp(-100.0, 200.0);
    // Temperature-corrected saturation current, matching the solver's
    // `diode_eval` (which uses `is_at(t)` whenever it uses the temp-corrected
    // Vt). Pairing a temp-corrected Vt with the nominal `is` understated the
    // forward current of a hot junction; the monitor's over-current/over-power
    // checks saw a cooler device than the solve actually settled at.
    let i = model.is_at(temp_c) * (exp_arg.exp() - 1.0);
    i.clamp(-1e3, 1e3)
}

/// Thermal utilisation as a RISE-based fraction: (Tj − ambient)/(Tj_max −
/// ambient). An idle part reads ~0, comparable with the power/current/voltage
/// checks, which are all true 0-at-idle ratios, and a part at its junction
/// limit reads 1.0. An absolute-Celsius ratio (Tj/Tj_max) instead floors every
/// dissipating part at ~ambient/Tj_max (~0.2), giving the exported heat-map a
/// spurious floor. The trip threshold is the same either way: frac > 1 ⟺ Tj >
/// Tj_max under both. Clamped at 0 so a below-ambient junction never reads
/// negative.
fn thermal_stress_frac(tj_c: f64, tj_max_c: f64, ambient_c: f64) -> f64 {
    let span = tj_max_c - ambient_c;
    if span > 0.0 {
        ((tj_c - ambient_c) / span).max(0.0)
    } else {
        0.0
    }
}

#[cfg(test)]
mod monitor_temp_tests {
    use super::*;

    /// R15: the exported thermal stress fraction is rise-based, so a lightly-
    /// loaded part reads ~0, not the ~0.2 floor the absolute Tj/Tj_max ratio gave
    /// every dissipating device, while the fault trip (frac > 1) is unchanged.
    #[test]
    fn thermal_stress_is_rise_based_not_absolute() {
        // Ambient 25 C, Tj_max 125 C.
        // A part barely above ambient reads ~0, not 25/125 = 0.20.
        assert!(
            thermal_stress_frac(26.0, 125.0, 25.0) < 0.02,
            "idle part ~0"
        );
        // Exactly ambient reads 0.
        assert_eq!(thermal_stress_frac(25.0, 125.0, 25.0), 0.0);
        // Halfway up the rise band reads 0.5.
        assert!((thermal_stress_frac(75.0, 125.0, 25.0) - 0.5).abs() < 1e-9);
        // At the junction limit reads exactly 1.0 (the trip boundary is preserved).
        assert!((thermal_stress_frac(125.0, 125.0, 25.0) - 1.0).abs() < 1e-9);
        // Over the limit reads > 1 (still trips).
        assert!(thermal_stress_frac(150.0, 125.0, 25.0) > 1.0);
        // A below-ambient junction never reads negative.
        assert_eq!(thermal_stress_frac(10.0, 125.0, 25.0), 0.0);
    }

    #[test]
    fn a_metric_only_code_is_read_as_metric_not_imperial() {
        // "R_0402Metric" is metric 0402, an imperial 01005 at 1/32 W. Reading the
        // 0402 as imperial rates it 1/16 W, double its real limit, and suppresses
        // real overpower findings. Same for metric 0603 (imperial 0201, 1/20 W).
        for (f, want) in [
            ("Resistor_SMD:R_0402Metric", 1.0 / 32.0),
            ("Resistor_SMD:R_0603Metric", 1.0 / 20.0),
            ("Resistor_SMD:R_1005Metric", 1.0 / 16.0),
            ("Resistor_SMD:R_3216Metric", 1.0 / 4.0),
            // The imperial size code is paired with its metric code, and a small
            // part's metric code equals a larger part's imperial code. Matching
            // the metric suffix first over-rates tiny parts by up to 2x.
            ("Resistor_SMD:R_0201_0603Metric", 1.0 / 20.0),
            ("Resistor_SMD:R_01005_0402Metric", 1.0 / 32.0),
            ("R_0402_1005Metric", 1.0 / 16.0),
            ("R_0603_1608Metric", 1.0 / 10.0),
            ("R_0805_2012Metric", 1.0 / 8.0),
            ("R_1206_3216Metric", 1.0 / 4.0),
        ] {
            assert_resistor_rating(f, ResistorPowerBasis::ChipPackage, Some(want));
        }
        // Real KiCad names carry pad dimensions after the size code. Treating
        // those digits as a second size code sent the name to the imperial pass,
        // which read metric 0402 as imperial 0402 and doubled the rating.
        for (f, want) in [
            ("Resistor_SMD:R_0402Metric_Pad0.74x0.62mm", 1.0 / 32.0),
            ("Resistor_SMD:R_0603Metric_Pad0.98x0.95mm", 1.0 / 20.0),
            ("Resistor_SMD:R_3216Metric_Pad1.42x1.75mm", 1.0 / 4.0),
        ] {
            assert_resistor_rating(f, ResistorPowerBasis::ChipPackage, Some(want));
        }
        // KiCad's dual form carries a separate imperial token, which stays
        // authoritative: 0201 imperial (1/20 W), NOT 0603 metric read as imperial.
        for (f, want) in [
            ("Resistor_SMD:R_0201_0603Metric", 1.0 / 20.0),
            ("Resistor_SMD:R_01005_0402Metric", 1.0 / 32.0),
            ("Resistor_SMD:R_0402_1005Metric", 1.0 / 16.0),
            ("Resistor_SMD:R_1206_3216Metric", 1.0 / 4.0),
            ("Resistor_SMD:R_0402_1005Metric_Pad0.72x0.64mm", 1.0 / 16.0),
            ("Resistor_SMD:R_0201_0603Metric_Pad0.64x0.40mm", 1.0 / 20.0),
        ] {
            assert_resistor_rating(f, ResistorPowerBasis::ChipPackage, Some(want));
        }
    }

    #[test]
    fn unreadable_resistor_packages_abstain_instead_of_guessing() {
        // Metric 2010 is a 2.0 x 1.0 mm body, not imperial 2010; generic THT,
        // vertical, cement, and uncoded axial bodies likewise give no wattage.
        // Guessing in either direction invents or suppresses overpower findings.
        for f in [
            "Resistor_SMD:R_2010Metric",
            "Resistor_SMD:R_1020Metric_Pad0.5x0.5mm",
            "Resistor_THT:R_Vertical",
            "Resistor_THT:R_Cement_L20mm_W7mm_Px15mm",
            "Resistor_THT:R_Axial_Power_L11.9mm_W4.5mm_P15.24mm",
            "Resistor_THT:R_Axial_Custom",
        ] {
            assert_resistor_rating(f, ResistorPowerBasis::Unknown, None);
        }
    }

    /// One monitored device: every test below wants the same carrier around a
    /// reference, a kind, a footprint and some ratings.
    fn meta(reference: &str, kind: ComponentKind, footprint: &str, ratings: Ratings) -> DeviceMeta {
        DeviceMeta {
            reference: reference.into(),
            device: DeviceId(0),
            kind,
            footprint: footprint.into(),
            ratings,
        }
    }

    fn passive(reference: &str, footprint: &str) -> DeviceMeta {
        meta(
            reference,
            ComponentKind::Passive,
            footprint,
            Ratings::default(),
        )
    }

    fn assert_resistor_rating(footprint: &str, basis: ResistorPowerBasis, watts: Option<f64>) {
        let rating = resistor_power_from_footprint(footprint);
        assert_eq!(rating.basis, basis, "{footprint}");
        assert_eq!(rating.watts, watts, "{footprint}");
    }

    fn resistor_monitor(ohms: f64, ratings: Ratings) -> (Circuit, NodeId, StressMonitor) {
        let mut circuit = Circuit::new();
        let node = circuit.node("A");
        let device = circuit.add(Device::resistor("R1", node, NodeId::GROUND, ohms));
        let meta = DeviceMeta {
            device,
            ..meta("R1", ComponentKind::Passive, "", ratings)
        };
        (circuit, node, StressMonitor::new(vec![meta]))
    }

    fn supply_monitor(reference: &str, max_v: f64) -> (Circuit, NodeId, StressMonitor) {
        let mut circuit = Circuit::new();
        let node = circuit.node("VCC");
        let mut monitor = StressMonitor::new(Vec::new());
        monitor.set_supply_watches(vec![SupplyWatch {
            reference: reference.into(),
            node,
            max_v,
        }]);
        (circuit, node, monitor)
    }

    fn drive(
        monitor: &mut StressMonitor,
        circuit: &mut Circuit,
        node: NodeId,
        volts: f64,
        chunks: u32,
    ) -> Vec<FaultEvent> {
        let node_v = |n| if n == node { volts } else { 0.0 };
        (0..chunks)
            .flat_map(|k| monitor.evaluate(circuit, &node_v, &|_| None, f64::from(k) * 1e-3))
            .collect()
    }

    #[test]
    fn non_resistor_passives_get_no_footprint_wattage() {
        // ComponentKind::Passive also covers capacitors, inductors and beads.
        // Their limits are current and voltage, not a chip-resistor wattage, and
        // handing an 0805 inductor an 0805 resistor's 1/8 W invents an overpower
        // fault out of ordinary coil heating. Lowering the unknown-SMD floor to
        // 1/16 W makes that misfire easier, so the gating matters more, not less.
        for (r, f) in [
            ("L1", "Inductor_SMD:L_0805_2012Metric"),
            ("C3", "Capacitor_SMD:C_0402_1005Metric"),
            ("FB1", "Inductor_SMD:L_0603_1608Metric"),
            ("C7", "Capacitor_THT:CP_Radial_D6.3mm_P2.50mm"),
            ("L2", "Inductor_SMD:L_Bourns-SRN4018"),
        ] {
            let meta = passive(r, f);
            assert!(
                meta.power_rating_w().is_none(),
                "{r} ({f}) must get no derived power rating, got {:?}",
                meta.power_rating_w()
            );
            assert!(
                meta.power_coverage_gap().is_none(),
                "{r} is not a resistor, so it is not an overpower coverage hole"
            );
        }
    }

    #[test]
    fn resistors_still_get_their_rating_through_the_gate() {
        // The gate must not starve real resistors, whatever names them.
        for (r, f, want) in [
            ("R1", "Resistor_SMD:R_0402_1005Metric", 1.0 / 16.0),
            ("R2", "R_0603_1608Metric", 1.0 / 10.0),
            (
                "R3",
                "Resistor_THT:R_Axial_DIN0207_L6.3mm_D2.5mm_P10.16mm_Horizontal",
                0.25,
            ),
            ("RN1", "Resistor_SMD:R_Array_Concave_4x0603", 1.0 / 10.0),
        ] {
            let meta = passive(r, f);
            let got = meta.power_rating_w();
            assert!(
                got.is_some_and(|w| (w - want).abs() < 1e-12),
                "{r} ({f}) must rate {want} W, got {got:?}"
            );
        }
    }

    #[test]
    fn din_axial_bodies_rate_per_code_not_alike() {
        // The DIN codes are size evidence and they are NOT interchangeable:
        // DIN0204 is a 1/8 W body, DIN0207 a 1/4 W one. Rating them alike
        // over-rates the smaller twofold and suppresses its overpower check.
        for (f, want) in [
            (
                "Resistor_THT:R_Axial_DIN0204_L3.6mm_D1.6mm_P7.62mm_Horizontal",
                0.125,
            ),
            (
                "Resistor_THT:R_Axial_DIN0207_L6.3mm_D2.5mm_P10.16mm_Horizontal",
                0.25,
            ),
            (
                "Resistor_THT:R_Axial_DIN0411_L9.9mm_D3.6mm_P12.70mm_Horizontal",
                1.0,
            ),
        ] {
            assert_resistor_rating(f, ResistorPowerBasis::ThtAxial, Some(want));
        }
    }

    #[test]
    fn an_unreadable_package_is_a_named_coverage_gap() {
        // No package derives no rating; the abstention names the part and unlock.
        let meta = passive("R7", "");
        assert_resistor_rating("", ResistorPowerBasis::Unknown, None);
        assert!(meta.power_rating_w().is_none());
        let gap = meta.power_coverage_gap().expect("gap must be reported");
        assert!(gap.contains("R7"), "names the part: {gap}");
        assert!(
            gap.contains("max_power_w") && gap.contains("BOM"),
            "names the unlock: {gap}"
        );
        let mon = StressMonitor::new(vec![meta]);
        assert_eq!(mon.power_coverage_gaps().len(), 1);

        // A custom SMD package is the same visible abstention, never a guess.
        let custom = passive("R9", "Resistor_SMD:R_CustomHouseFootprint");
        assert_resistor_rating(&custom.footprint, ResistorPowerBasis::Unknown, None);
        let gap = custom.power_coverage_gap().expect("a named gap");
        assert!(gap.contains("R9") && gap.contains("BOM"), "{gap}");
    }

    #[test]
    fn many_parts_sharing_one_unreadable_package_make_one_note_not_fifty() {
        // Real libraries produce names this cannot parse ("R-US_0207/10"), and a
        // board can carry dozens of them. Fifty near-identical notes is precisely
        // the volume at which users stop reading the honesty channel, so they
        // aggregate by package with a count and representative refs.
        let metas: Vec<DeviceMeta> = (1..=50)
            .map(|i| passive(&format!("R{i}"), "R-US_0207/10"))
            .collect();
        let gaps = aggregate_power_coverage_gaps(&metas);
        assert_eq!(gaps.len(), 1, "one package, one note: {gaps:?}");
        let g = &gaps[0];
        assert!(g.contains("50 resistor(s)"), "states the count: {g}");
        assert!(g.contains("R-US_0207/10"), "names the package: {g}");
        assert!(
            g.contains("and 45 more"),
            "names some and counts the rest: {g}"
        );
        assert!(g.contains("max_power_w"), "names the unlock: {g}");

        // Two distinct unreadable packages are two distinct holes.
        let mixed = vec![
            passive("R1", "R-US_0207/10"),
            passive("R2", "Resistor_SMD:R_CustomHouse"),
        ];
        assert_eq!(aggregate_power_coverage_gaps(&mixed).len(), 2);
    }

    #[test]
    fn a_readable_package_leaves_no_coverage_gap() {
        // The gap list must stay empty on ordinary boards, or it is noise.
        for f in [
            "Resistor_SMD:R_0402_1005Metric",
            "Resistor_SMD:R_3216Metric",
            "Resistor_THT:R_Axial_DIN0207_L6.3mm_D2.5mm_P10.16mm_Horizontal",
        ] {
            let meta = passive("R1", f);
            assert!(meta.power_coverage_gap().is_none(), "{f}");
            assert!(meta.power_rating_w().is_some(), "{f}");
        }
    }

    #[test]
    fn passive_inductor_gets_a_continuous_current_check() {
        // R13: a passive with a continuous current rating (an inductor's
        // rated/saturation current) must produce an Overcurrent check. It was
        // omitted, only surge was ever checked for passives.
        let ratings = Ratings {
            max_current_a: Some(2.0),
            ..Default::default()
        };
        let meta = meta("L1", ComponentKind::Passive, "", ratings);
        let op = OperatingPoint::new(3.0, 0.1, 0.0);
        let checks = build_checks(&meta, &op, op.current_a);
        let oc = checks
            .iter()
            .find(|c| c.kind == FaultKind::Overcurrent && !c.surge)
            .expect("passive continuous over-current check present");
        assert_eq!(oc.limit, 2.0);
        assert_eq!(oc.value, 3.0);
    }

    #[test]
    fn a_continuous_current_rating_judges_a_lag_not_an_endpoint_or_a_run_average() {
        // A 1 A part on a 1 ohm load, driven chunk by chunk at 50 us each.
        // (name, per-chunk multiples of the rating, expect a fault). The first
        // row is the c11 shape: 1.7x for 0.4 ms - eight chunks, twice what the
        // sustain filter alone suppresses - then settled inside the rating. The
        // last row is what a run-length AVERAGE hides: 22 ms of idle drags such
        // a mean under the rating however hard the part is driven afterwards.
        let dt = 5e-5;
        for (name, amps, want) in [
            (
                "inrush then settle",
                [vec![1.7; 8], vec![0.5; 60]].concat(),
                false,
            ),
            ("steady overload", vec![2.0; 60], true),
            (
                "idle then a late 2x step",
                [vec![0.0; 450], vec![2.0; 60]].concat(),
                true,
            ),
        ] {
            let ratings = Ratings {
                max_current_a: Some(1.0),
                ..Default::default()
            };
            let (mut circuit, node, mut monitor) = resistor_monitor(1.0, ratings);
            let raised = amps.iter().enumerate().any(|(k, i)| {
                monitor.deposit_chunk_energy(&[0.0], dt);
                let v = *i;
                monitor
                    .evaluate(
                        &mut circuit,
                        &|n| if n == node { v } else { 0.0 },
                        &|_| None,
                        k as f64 * dt,
                    )
                    .iter()
                    .any(|f| f.kind == FaultKind::Overcurrent)
            });
            assert_eq!(raised, want, "{name}");
        }
    }

    #[test]
    fn a_package_guessed_theta_ja_qualifies_the_claims_it_feeds() {
        // (volts across 100 ohm, the card's theta_JA, whether a claim is named
        // and whether 2x the guessed 200 C/W reaches the 125 C limit): idle,
        // 10 mW (a 2 C rise), 1 W (a 200 C rise), and a card that states it.
        for (volts, theta, want) in [
            (0.0, None, None),
            (1.0, None, Some(false)),
            (10.0, None, Some(true)),
            (10.0, Some(50.0), None),
        ] {
            let ratings = Ratings {
                theta_ja_c_per_w: theta,
                ..Default::default()
            };
            let (mut circuit, node, mut monitor) = resistor_monitor(100.0, ratings);
            drive(&mut monitor, &mut circuit, node, volts, 2);
            let seen = std::cell::Cell::new(None);
            let out = monitor.inferred_theta_ja(|_, decisive| {
                seen.set(Some(decisive));
                vec![Scope::Board]
            });
            assert_eq!(
                (seen.get(), out.len()),
                (want, usize::from(want.is_some())),
                "{volts} V"
            );
            if let Some(a) = out.first() {
                assert!(
                    a.because().contains("200 C/W inferred from its package"),
                    "{a:?}"
                );
                assert!(
                    a.replacement().contains("theta_ja_c_per_w to R1's card"),
                    "{a:?}"
                );
            }
        }
    }

    #[test]
    fn reset_tracks_lets_a_sustained_fault_re_raise_on_replay() {
        // R17: the monitor accumulates `raised` / `over_chunks` across a run. A
        // replay (engine reset -> re-run the same chunks) must see the SAME
        // fault fire again. Before reset_tracks existed, the stale `raised` flag
        // silently swallowed the fault on the second run.
        // 1/10 W part carrying 10 V / 100 Ω = 1 W: a 10x sustained overpower.
        let ratings = Ratings {
            max_power_w: Some(0.1),
            ..Default::default()
        };
        let (mut circuit, node, mut monitor) = resistor_monitor(100.0, ratings);
        let raise_once = |monitor: &mut StressMonitor, circuit: &mut Circuit| {
            drive(monitor, circuit, node, 10.0, SUSTAIN_CHUNKS + 2)
                .iter()
                .filter(|f| f.kind == FaultKind::Overpower && f.component == "R1")
                .count()
        };

        assert_eq!(
            raise_once(&mut monitor, &mut circuit),
            1,
            "first run raises the overpower fault once"
        );
        assert!(
            monitor.stress_by_ref().get("R1").copied().unwrap_or(0.0) >= 1.0,
            "stress pegged"
        );

        // Without a reset the fault stays latched (raised) and does not re-fire.
        assert_eq!(
            raise_once(&mut monitor, &mut circuit),
            0,
            "latched: no re-raise without reset"
        );

        // reset_tracks clears the latch AND the live stress; the replay re-raises.
        monitor.reset_tracks();
        assert!(
            monitor.stress_by_ref().is_empty(),
            "reset clears the live stress map"
        );
        assert_eq!(
            raise_once(&mut monitor, &mut circuit),
            1,
            "after reset the fault re-raises on replay"
        );
    }

    #[test]
    fn supply_watch_raises_overvoltage_past_the_mcu_abs_max() {
        // An MCU has no whole-device meta (per-pin currents only), so its
        // abs-max supply rating is enforced through a SupplyWatch on the Vcc
        // NODE: a rail driven to 100 V on a 6 V-max part must raise a
        // sustained overvoltage fault naming the part, and a nominal rail
        // must raise nothing.
        let (mut circuit, node, mut monitor) = supply_monitor("U1", 6.0);

        // Nominal 5 V: no fault, modest stress fraction.
        assert!(
            drive(&mut monitor, &mut circuit, node, 5.0, SUSTAIN_CHUNKS + 2).is_empty(),
            "5 V on a 6 V-max part is not a fault"
        );
        let frac = monitor.stress_by_ref().get("U1").copied().unwrap_or(0.0);
        assert!(
            (frac - 5.0 / 6.0).abs() < 1e-9,
            "stress fraction 5/6: {frac}"
        );

        // 100 V: sustained overvoltage, raised exactly once, naming the part.
        let raised = drive(&mut monitor, &mut circuit, node, 100.0, SUSTAIN_CHUNKS + 2);
        assert_eq!(raised.len(), 1, "raised once, not per chunk");
        assert_eq!(raised[0].component, "U1");
        assert_eq!(raised[0].kind, FaultKind::Overvoltage);
        assert_eq!(raised[0].limit, 6.0);
        assert_eq!(raised[0].value, 100.0);

        // reset_tracks clears the latch: a replay re-raises.
        monitor.reset_tracks();
        assert_eq!(
            drive(&mut monitor, &mut circuit, node, 100.0, SUSTAIN_CHUNKS + 2,).len(),
            1,
            "watch re-raises after reset_tracks"
        );
    }

    /// Two-sided pin for [`OVER_LIMIT_NOISE_MARGIN`]: a rail sitting at its
    /// limit plus FLOAT NOISE (the MNT Reform mb3.0 shape: the charger rail
    /// rides Q15 at exactly its 30.0 V abs-max, and which side of 30.0 the
    /// converged transient settles on moves with any picoamp change anywhere
    /// in the matrix) raises nothing, while an excursion the solve genuinely
    /// resolves still trips. Without the margin, the noise side flips on
    /// unrelated numerics changes; that is a manufactured fault.
    #[test]
    fn over_limit_noise_is_not_a_fault_but_a_real_excursion_is() {
        let (mut circuit, node, mut monitor) = supply_monitor("Q15", 30.0);

        // Nanovolt-scale over the limit (measured on the mb3.0 fuzz:
        // +1.8e-9 V on 30 V, 6e-11 relative): numerical noise, no fault.
        assert!(
            drive(
                &mut monitor,
                &mut circuit,
                node,
                30.0 * (1.0 + 1e-10),
                SUSTAIN_CHUNKS + 2
            )
            .is_empty(),
            "an over-limit excursion below the solve's resolution must not fault"
        );

        // 0.01% over (30.003 V): a resolvable excursion, faults as before.
        let raised = drive(
            &mut monitor,
            &mut circuit,
            node,
            30.0 * 1.0001,
            SUSTAIN_CHUNKS + 2,
        );
        assert_eq!(raised.len(), 1, "a real excursion still trips exactly once");
        assert_eq!(raised[0].component, "Q15");
    }

    #[test]
    fn diode_destruction_direction_depends_on_the_fault() {
        // R13: over-current burns a diode OPEN (huge R); reverse over-voltage
        // fails it CLOSED (a small series short). The consequence must follow the
        // tripping fault, not be a fixed "always open".
        let ohms_after = |kind: FaultKind| -> f64 {
            let mut c = Circuit::new();
            let a = c.node("A");
            let k = c.node("K");
            let id = c.add(Device::Diode {
                name: "D1".into(),
                a,
                k,
                model: hauksbee_ir::DiodeModel::default(),
            });
            let d = Ratings::default();
            let meta = DeviceMeta {
                device: id,
                ..meta("D1", ComponentKind::Diode, "", d)
            };
            let mut mon = StressMonitor::new(vec![meta.clone()]);
            mon.destructive = true;
            assert!(mon.maybe_destroy(&mut c, &meta, kind));
            match &c.devices[id.0 as usize] {
                Device::Resistor { ohms, .. } => *ohms,
                other => panic!("expected a resistor after destruction, got {other:?}"),
            }
        };
        assert!(
            ohms_after(FaultKind::Overcurrent) > 1e9,
            "over-current opens"
        );
        assert!(
            ohms_after(FaultKind::Overvoltage) < 1.0,
            "reverse over-voltage shorts"
        );
    }

    #[test]
    fn diode_current_tracks_temperature_corrected_saturation() {
        // At a fixed forward bias, a hot junction carries MORE current than the
        // nominal-Is formula gives (Is rises steeply with temperature). The
        // monitor must use is_at(T), the same correction the solver applies, so
        // its over-current / over-power checks see the real hot-junction current.
        let model = hauksbee_ir::DiodeModel::default();
        let vd = 0.6;
        let i_cold = diode_current(&model, vd, 27.0);
        let i_hot = diode_current(&model, vd, 125.0);
        assert!(
            i_hot > i_cold * 2.0,
            "hot-junction current must be well above cold: cold={i_cold:e}, hot={i_hot:e}"
        );
        // And the reported current must equal the temp-corrected Shockley value,
        // not the nominal-Is one.
        let vt_hot = hauksbee_ir::thermal_voltage_c(125.0) * model.n;
        let expected = model.is_at(125.0) * ((vd / vt_hot).exp() - 1.0);
        assert!(
            (i_hot - expected).abs() <= expected.abs() * 1e-9,
            "diode_current must use is_at(T): got {i_hot:e}, expected {expected:e}"
        );
        // A nominal-Is result would be materially smaller, guard the gap.
        let nominal = model.is * ((vd / vt_hot).exp() - 1.0);
        assert!(
            i_hot > nominal * 1.5,
            "temp-corrected current must exceed the nominal-Is current it replaced"
        );
    }
}
