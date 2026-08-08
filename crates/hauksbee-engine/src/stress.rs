//! Fault / stress monitor (Feature 2).
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

use hauksbee_ir::{Circuit, Device, DeviceId, NodeId};
use hauksbee_models::schema::{ComponentKind, Ratings};

/// Consecutive chunks a continuous-rating violation must persist before it is
/// reported as a fault (filters switching-edge transients).
pub const SUSTAIN_CHUNKS: u32 = 4;

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
    pub fn power_coverage_gap(&self) -> Option<String> {
        if !matches!(self.kind, ComponentKind::Passive)
            || self.ratings.max_power_w.is_some()
            || !self.is_resistor_like()
        {
            return None;
        }
        let derived = resistor_power_from_footprint(&self.footprint);
        if derived.basis != ResistorPowerBasis::Unknown {
            return None;
        }
        Some(format!(
            "stress: {} has no power rating and no readable package \"{}\", so its \
             overpower check did not run. Give the part a model with ratings.max_power_w, \
             or a footprint / BOM line naming the package, to cover it.",
            self.reference,
            if self.footprint.is_empty() {
                "(none)"
            } else {
                self.footprint.as_str()
            },
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
        MetricCode::Unrecognised => {
            return ResistorPower {
                watts: None,
                basis: ResistorPowerBasis::Unknown,
            };
        }
        MetricCode::Absent => {}
    }
    // Match the imperial size token anywhere in the footprint string
    // (e.g. "Resistor_SMD:R_0402_1005Metric"). The imperial code is paired with
    // its metric code, and the metric code of a small part collides with the
    // imperial code of a larger one: imperial 0201 → metric 0603
    // ("R_0201_0603Metric"), imperial 01005 → metric 0402. So the smallest
    // packages MUST be matched first, before the larger imperial tokens they
    // embed as a metric suffix, or they are silently over-rated.
    if f.contains("01005") {
        chip(1.0 / 32.0)
    } else if f.contains("0201") {
        chip(1.0 / 20.0)
    } else if f.contains("0402") {
        chip(1.0 / 16.0)
    } else if f.contains("0603") {
        chip(1.0 / 10.0)
    } else if f.contains("0805") {
        chip(1.0 / 8.0)
    } else if f.contains("1206") {
        chip(1.0 / 4.0)
    } else if f.contains("1210") {
        chip(1.0 / 2.0)
    } else if f.contains("2010") {
        chip(3.0 / 4.0)
    } else if f.contains("2512") {
        chip(1.0)
    } else if f.contains("1812") {
        chip(1.0)
    } else if f.contains("2220") {
        chip(1.0)
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
        ResistorPower {
            watts: None,
            basis: ResistorPowerBasis::Unknown,
        }
    }
}

/// Every imperial chip size code the rating table recognises. Used to tell
/// KiCad's dual "imperial_metricMetric" form from a metric-only name.
const IMPERIAL_CHIP_CODES: &[&str] = &[
    "01005", "0201", "0402", "0603", "0805", "1206", "1210", "1812", "2010", "2220", "2512",
];

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
    let metric: String = {
        let head: Vec<char> = f[..idx].chars().collect();
        let mut digits: Vec<char> = head
            .iter()
            .rev()
            .take_while(|c| c.is_ascii_digit())
            .copied()
            .collect();
        digits.reverse();
        digits.into_iter().collect()
    };
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
        let prev_token: String = prev
            .chars()
            .rev()
            .take_while(|c| c.is_ascii_digit())
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        if !prev_token.is_empty() && IMPERIAL_CHIP_CODES.contains(&prev_token.as_str()) {
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
    name.rsplit_once("_q")
        .filter(|(_, n)| n.chars().all(|c| c.is_ascii_digit()))
        .map(|(b, _)| b)
        .or_else(|| {
            name.rsplit_once("_s")
                .filter(|(_, n)| n.chars().all(|c| c.is_ascii_digit()))
                .map(|(b, _)| b)
        })
        .or_else(|| {
            name.rsplit_once("_e")
                .filter(|(_, n)| n.chars().all(|c| c.is_ascii_digit()))
                .map(|(b, _)| b)
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
}

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
        }
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
        let mut seen = std::collections::BTreeSet::new();
        self.metas
            .iter()
            .filter_map(|m| m.power_coverage_gap())
            .filter(|m| seen.insert(m.clone()))
            .collect()
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
            let op = OperatingPoint {
                current_a: op.current_a,
                voltage_v: op.voltage_v,
                power_w: unit_avg_w[i],
            };
            let op = &op;
            let mut checks = build_checks(&meta, op);

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
                    if frac > 1.0
                        && !self.tracks[i]
                            .raised
                            .get(chk.kind.as_str())
                            .copied()
                            .unwrap_or(false)
                    {
                        self.tracks[i].raised.insert(chk.kind.as_str(), true);
                        let destroyed = self.maybe_destroy(circuit, &meta, chk.kind);
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
                    }
                    continue;
                }

                // Continuous rating: needs to be sustained.
                let counter = self.tracks[i]
                    .over_chunks
                    .entry(chk.kind.as_str())
                    .or_insert(0);
                if frac > 1.0 {
                    *counter += 1;
                } else {
                    *counter = 0;
                }
                let sustained = *counter >= SUSTAIN_CHUNKS;
                if sustained
                    && !self.tracks[i]
                        .raised
                        .get(chk.kind.as_str())
                        .copied()
                        .unwrap_or(false)
                {
                    self.tracks[i].raised.insert(chk.kind.as_str(), true);
                    let destroyed = self.maybe_destroy(circuit, &meta, chk.kind);
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
                    if destroyed {
                        break;
                    }
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
            if frac > 1.0 {
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
                *dev = Device::Resistor {
                    name,
                    a,
                    b: k,
                    ohms,
                    tc1: None,
                };
                true
            }
            _ => false,
        }
    }
}

/// The live operating point of a device this chunk.
struct OperatingPoint {
    /// Through-current magnitude (A).
    current_a: f64,
    /// Across-voltage, signed (V): for diodes, anode−cathode; for caps, the
    /// terminal voltage; for two-terminals generally `Va − Vb`.
    voltage_v: f64,
    /// Power dissipation (W).
    power_w: f64,
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
            OperatingPoint {
                current_a: i.abs(),
                voltage_v: v,
                power_w: (v * i).abs(),
            }
        }
        Some(Device::Diode { a, k, model, .. }) => {
            let vd = node_v(*a) - node_v(*k);
            let id = diode_current(model, vd, circuit.temp_c);
            OperatingPoint {
                current_a: id.abs(),
                voltage_v: vd,
                power_w: (vd * id).abs(),
            }
        }
        Some(Device::Capacitor { a, b, .. }) => {
            // An ideal capacitor's through-current is displacement current,
            // it needs dv/dt across chunks, not one voltage sample, and it
            // dissipates no real power. Every capacitor check (over-voltage,
            // reverse bias) is voltage-based, so the zeros disable nothing.
            let v = node_v(*a) - node_v(*b);
            OperatingPoint {
                current_a: 0.0,
                voltage_v: v,
                power_w: 0.0,
            }
        }
        Some(Device::Inductor { a, b, .. }) => {
            // The winding current lives in the inductor's branch unknown
            // (like a Vsource's), not in a node-voltage difference, without
            // it the surge-current check could never fire for a coil. Power
            // stays zero: an ideal inductor *stores* v·i rather than
            // dissipating it, and reporting it as heat would false-trip the
            // power-gated thermal check on every energised coil.
            let i = branch_current(meta.device).unwrap_or(0.0);
            OperatingPoint {
                current_a: i.abs(),
                voltage_v: node_v(*a) - node_v(*b),
                power_w: 0.0,
            }
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
            OperatingPoint {
                current_a: i_worst,
                voltage_v: vce,
                power_w: (vce * ic).abs().min(1e6) + (sign * vbe * ib).abs().min(1e6),
            }
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
            OperatingPoint {
                current_a: i,
                voltage_v: v,
                power_w: v.abs() * i,
            }
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
            OperatingPoint {
                current_a: ids.abs().min(1e3),
                voltage_v: vds,
                power_w: (vds_f * ids).abs().min(1e6),
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
            OperatingPoint {
                current_a: i.abs(),
                voltage_v: 0.0,
                power_w: 0.0,
            }
        }
        _ => OperatingPoint {
            current_a: 0.0,
            voltage_v: 0.0,
            power_w: 0.0,
        },
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
fn build_checks(meta: &DeviceMeta, op: &OperatingPoint) -> Vec<Check> {
    let mut checks = Vec::new();
    let r = &meta.ratings;

    // Surge current (instantaneous ceiling), for any device with a surge spec.
    if let Some(surge) = r.max_surge_current_a {
        checks.push(Check {
            kind: FaultKind::SurgeCurrent,
            value: op.current_a,
            limit: surge,
            surge: true,
        });
    }

    match meta.kind {
        ComponentKind::Diode => {
            if let Some(imax) = r.max_current_a {
                checks.push(Check {
                    kind: FaultKind::Overcurrent,
                    value: op.current_a,
                    limit: imax,
                    surge: false,
                });
            }
            // Reverse blocking voltage: only the reverse magnitude counts.
            if let Some(vmax) = r.max_voltage_v {
                let reverse = (-op.voltage_v).max(0.0);
                checks.push(Check {
                    kind: FaultKind::Overvoltage,
                    value: reverse,
                    limit: vmax,
                    surge: false,
                });
            }
        }
        ComponentKind::Passive => {
            // Resistor power (rated or footprint-derived).
            if let Some(pmax) = meta.power_rating_w() {
                checks.push(Check {
                    kind: FaultKind::Overpower,
                    value: op.power_w,
                    limit: pmax,
                    surge: false,
                });
            }
            // Continuous current rating; the natural home for an inductor's
            // rated / saturation current. Skip it for passives and a coil's
            // steady-state current limit goes silently unenforced (an inductor's
            // power_w is 0, so Overpower/Overtemperature are dead there too).
            if let Some(imax) = r.max_current_a {
                checks.push(Check {
                    kind: FaultKind::Overcurrent,
                    value: op.current_a,
                    limit: imax,
                    surge: false,
                });
            }
            // Polarized capacitor reverse bias: any reverse beyond ~0.5 V.
            if r.polarized {
                let reverse = (-op.voltage_v).max(0.0);
                checks.push(Check {
                    kind: FaultKind::ReverseBias,
                    value: reverse,
                    limit: 0.5,
                    surge: false,
                });
            }
            // Capacitor over-voltage.
            if let Some(vmax) = r.max_voltage_v {
                checks.push(Check {
                    kind: FaultKind::Overvoltage,
                    value: op.voltage_v.abs(),
                    limit: vmax,
                    surge: false,
                });
            }
        }
        ComponentKind::BjtNpn
        | ComponentKind::BjtPnp
        | ComponentKind::Nmos
        | ComponentKind::Pmos => {
            if let Some(imax) = r.max_current_a {
                checks.push(Check {
                    kind: FaultKind::Overcurrent,
                    value: op.current_a,
                    limit: imax,
                    surge: false,
                });
            }
            if let Some(vmax) = r.max_voltage_v {
                checks.push(Check {
                    kind: FaultKind::Overvoltage,
                    value: op.voltage_v.abs(),
                    limit: vmax,
                    surge: false,
                });
            }
            if let Some(pmax) = r.max_power_w {
                checks.push(Check {
                    kind: FaultKind::Overpower,
                    value: op.power_w,
                    limit: pmax,
                    surge: false,
                });
            }
        }
        ComponentKind::Vreg => {
            if let Some(imax) = r.max_current_a {
                checks.push(Check {
                    kind: FaultKind::Overcurrent,
                    value: op.current_a,
                    limit: imax,
                    surge: false,
                });
            }
        }
        ComponentKind::AnalogSwitch => {
            if let Some(ipin) = r.max_pin_current_a {
                checks.push(Check {
                    kind: FaultKind::PinOvercurrent,
                    value: op.current_a,
                    limit: ipin,
                    surge: false,
                });
            }
        }
        ComponentKind::Mcu
        | ComponentKind::Digital
        | ComponentKind::ShiftRegister
        | ComponentKind::Dac
        | ComponentKind::Adc => {
            // These kinds get PER-PIN metas, not a package meta: an MCU or
            // logic IC has no single through-current, but every pin it drives
            // is stamped as a Thevenin PinDriver whose hidden Vsource's branch
            // unknown IS that pin's source/sink current. The binder monitors
            // each driver Vsource (reference "<ref>:<pin>", see
            // `gather_device_meta`), so `op.current_a` here is a genuine pin
            // current and this check fires on a real per-pin violation.
            if let Some(ipin) = r.max_pin_current_a {
                checks.push(Check {
                    kind: FaultKind::PinOvercurrent,
                    value: op.current_a,
                    limit: ipin,
                    surge: false,
                });
            }
        }
        _ => {}
    }
    checks
}

/// Approximate diode current from the Shockley equation at terminal voltage
/// `vd` and temperature `temp_c`. Series resistance is ignored (first-order;
/// the solver already accounts for it in `vd`), and the result is clamped to a
/// sane range so a runaway forward bias does not overflow.
/// Forward current from the diode's TERMINAL voltage.
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
    fn resistor_rating_reads_imperial_not_the_metric_collision() {
        // The imperial size code is paired with its metric code, and a small
        // part's metric code equals a larger part's imperial code: imperial 0201
        // → metric 0603 ("R_0201_0603Metric"), imperial 01005 → metric 0402.
        // A substring match on "0603"/"0402" first therefore over-rated the tiny
        // parts by up to ~2× their real power, hiding genuine over-power faults.
        let w = |f: &str| resistor_power_from_footprint(f).watts.unwrap();
        assert!(
            (w("Resistor_SMD:R_0201_0603Metric") - 1.0 / 20.0).abs() < 1e-12,
            "0201 must rate at 1/20 W, not the 0603's 1/10 W"
        );
        assert!(
            (w("Resistor_SMD:R_01005_0402Metric") - 1.0 / 32.0).abs() < 1e-12,
            "01005 must rate at 1/32 W, not the 0402's 1/16 W"
        );
        // The larger imperial parts are unaffected (their metric suffix does not
        // embed a smaller imperial token that wins first).
        assert!((w("R_0402_1005Metric") - 1.0 / 16.0).abs() < 1e-12);
        assert!((w("R_0603_1608Metric") - 1.0 / 10.0).abs() < 1e-12);
        assert!((w("R_0805_2012Metric") - 1.0 / 8.0).abs() < 1e-12);
        assert!((w("R_1206_3216Metric") - 1.0 / 4.0).abs() < 1e-12);
    }

    #[test]
    fn an_unrecognised_smd_size_derives_nothing_rather_than_guessing() {
        // A 1/4 W default exceeds a real 0402 (1/16 W) by 4x and suppresses
        // genuine findings; a 1/16 W floor undercuts everything above the
        // smallest and invents them. No direction is conservative, so the size is
        // either read or the part abstains.
        let r = resistor_power_from_footprint("Resistor_SMD:R_CustomHouseFootprint");
        assert_eq!(r.basis, ResistorPowerBasis::Unknown);
        assert!(
            r.watts.is_none(),
            "an unreadable size must derive no rating, got {:?}",
            r.watts
        );
        // And the abstention is visible, naming the part and the unlock.
        let meta = passive("R9", "Resistor_SMD:R_CustomHouseFootprint");
        let gap = meta.power_coverage_gap().expect("a named gap");
        assert!(gap.contains("R9") && gap.contains("BOM"), "{gap}");
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
        ] {
            let r = resistor_power_from_footprint(f);
            assert_eq!(r.basis, ResistorPowerBasis::ChipPackage, "{f}");
            assert!(
                (r.watts.unwrap() - want).abs() < 1e-12,
                "{f} must rate {want} W, got {:?}",
                r.watts
            );
        }
        // Real KiCad names carry pad dimensions after the size code. Treating
        // those digits as a second size code sent the name to the imperial pass,
        // which read metric 0402 as imperial 0402 and doubled the rating.
        for (f, want) in [
            ("Resistor_SMD:R_0402Metric_Pad0.74x0.62mm", 1.0 / 32.0),
            ("Resistor_SMD:R_0603Metric_Pad0.98x0.95mm", 1.0 / 20.0),
            ("Resistor_SMD:R_3216Metric_Pad1.42x1.75mm", 1.0 / 4.0),
        ] {
            let r = resistor_power_from_footprint(f);
            assert!(
                (r.watts.unwrap() - want).abs() < 1e-12,
                "{f} must still read as metric-only and rate {want} W, got {:?}",
                r.watts
            );
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
            let r = resistor_power_from_footprint(f);
            assert!(
                (r.watts.unwrap() - want).abs() < 1e-12,
                "{f} must rate {want} W from its imperial token, got {:?}",
                r.watts
            );
        }
    }

    #[test]
    fn an_unsupported_metric_code_abstains_instead_of_matching_an_imperial_substring() {
        // "R_2010Metric" is a 2.0 x 1.0 mm body. Falling through to the imperial
        // pass matches its "2010" and rates it as a 3/4 W imperial 2010, an order
        // of magnitude out, which suppresses real overpower findings.
        for f in [
            "Resistor_SMD:R_2010Metric",
            "Resistor_SMD:R_1020Metric_Pad0.5x0.5mm",
        ] {
            let r = resistor_power_from_footprint(f);
            assert_eq!(r.basis, ResistorPowerBasis::Unknown, "{f}");
            assert!(r.watts.is_none(), "{f} got {:?}", r.watts);
        }
    }

    #[test]
    fn a_generic_through_hole_body_is_not_assumed_axial() {
        // A bare "THT" match claimed every through-hole resistor footprint. A
        // vertical body or a cement power resistor is not a 1/4 W axial, and
        // guessing that both suppresses faults on smaller parts and invents them
        // on larger ones.
        for f in [
            "Resistor_THT:R_Vertical",
            "Resistor_THT:R_Cement_L20mm_W7mm_Px15mm",
        ] {
            let r = resistor_power_from_footprint(f);
            assert_eq!(r.basis, ResistorPowerBasis::Unknown, "{f}");
            assert!(r.watts.is_none(), "{f} got {:?}", r.watts);
        }
    }

    #[test]
    fn a_power_axial_body_does_not_get_the_quarter_watt_default() {
        // A "Power" axial is 1 W and up. Handing it 1/4 W invents faults on a
        // correct design, so it abstains like any other unknown size.
        let r = resistor_power_from_footprint("Resistor_THT:R_Axial_Power_L11.9mm_W4.5mm_P15.24mm");
        assert_eq!(r.basis, ResistorPowerBasis::Unknown);
        assert!(r.watts.is_none(), "got {:?}", r.watts);
    }

    fn passive(reference: &str, footprint: &str) -> DeviceMeta {
        DeviceMeta {
            reference: reference.into(),
            device: DeviceId(0),
            kind: ComponentKind::Passive,
            footprint: footprint.into(),
            ratings: Ratings::default(),
        }
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
    fn a_metric_only_chip_name_is_not_floored_to_one_sixteenth() {
        // "R_3216Metric" is an imperial 1206, a 1/4 W part. Falling to the
        // unknown-SMD floor under-rates it 4x and invents overpower faults.
        let r = resistor_power_from_footprint("Resistor_SMD:R_3216Metric");
        assert_eq!(r.basis, ResistorPowerBasis::ChipPackage);
        assert!(
            (r.watts.unwrap() - 0.25).abs() < 1e-12,
            "metric 3216 is a 1206 at 1/4 W, got {:?}",
            r.watts
        );
        // And the imperial pass still wins where both codes are present, so the
        // 0201-is-metric-0603 collision stays correct.
        assert!(
            (resistor_power_from_footprint("Resistor_SMD:R_0201_0603Metric")
                .watts
                .unwrap()
                - 1.0 / 20.0)
                .abs()
                < 1e-12,
            "imperial must still be consulted first"
        );
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
            let r = resistor_power_from_footprint(f);
            assert_eq!(r.basis, ResistorPowerBasis::ThtAxial, "{f}");
            assert!(
                (r.watts.unwrap() - want).abs() < 1e-12,
                "{f} must rate {want} W, got {:?}",
                r.watts
            );
        }
    }

    #[test]
    fn an_axial_body_without_a_din_code_abstains() {
        // No code means no size evidence, and the codes span 0.125 W to 5 W, so
        // there is no defensible blanket axial default.
        for f in [
            "Resistor_THT:R_Axial_Power_L11.9mm_W4.5mm_P15.24mm",
            "Resistor_THT:R_Axial_Custom",
        ] {
            let r = resistor_power_from_footprint(f);
            assert_eq!(r.basis, ResistorPowerBasis::Unknown, "{f}");
            assert!(r.watts.is_none(), "{f} got {:?}", r.watts);
        }
    }

    #[test]
    fn no_package_evidence_abstains_instead_of_guessing() {
        // Nothing to read: deriving either 1/4 W or 1/16 W would be an invention
        // in opposite directions, so no rating is produced at all.
        let r = resistor_power_from_footprint("");
        assert_eq!(r.basis, ResistorPowerBasis::Unknown);
        assert!(r.watts.is_none(), "an unreadable package rates nothing");
    }

    #[test]
    fn an_unreadable_package_is_a_named_coverage_gap() {
        // And the abstention is loud: it names the part and what would close it.
        let meta = DeviceMeta {
            reference: "R7".into(),
            device: DeviceId(0),
            kind: ComponentKind::Passive,
            footprint: String::new(),
            ratings: Ratings::default(),
        };
        assert!(meta.power_rating_w().is_none());
        let gap = meta.power_coverage_gap().expect("gap must be reported");
        assert!(gap.contains("R7"), "names the part: {gap}");
        assert!(
            gap.contains("max_power_w") && gap.contains("BOM"),
            "names the unlock: {gap}"
        );
        let mon = StressMonitor::new(vec![meta]);
        assert_eq!(mon.power_coverage_gaps().len(), 1);
    }

    #[test]
    fn a_readable_package_leaves_no_coverage_gap() {
        // The gap list must stay empty on ordinary boards, or it is noise.
        for f in [
            "Resistor_SMD:R_0402_1005Metric",
            "Resistor_SMD:R_3216Metric",
            "Resistor_THT:R_Axial_DIN0207_L6.3mm_D2.5mm_P10.16mm_Horizontal",
        ] {
            let meta = DeviceMeta {
                reference: "R1".into(),
                device: DeviceId(0),
                kind: ComponentKind::Passive,
                footprint: f.into(),
                ratings: Ratings::default(),
            };
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
        let meta = DeviceMeta {
            reference: "L1".into(),
            device: DeviceId(0),
            kind: ComponentKind::Passive,
            footprint: String::new(),
            ratings,
        };
        let op = OperatingPoint {
            current_a: 3.0,
            voltage_v: 0.1,
            power_w: 0.0,
        };
        let checks = build_checks(&meta, &op);
        let oc = checks
            .iter()
            .find(|c| c.kind == FaultKind::Overcurrent && !c.surge)
            .expect("passive continuous over-current check present");
        assert_eq!(oc.limit, 2.0);
        assert_eq!(oc.value, 3.0);
    }

    #[test]
    fn reset_tracks_lets_a_sustained_fault_re_raise_on_replay() {
        // R17: the monitor accumulates `raised` / `over_chunks` across a run. A
        // replay (engine reset -> re-run the same chunks) must see the SAME
        // fault fire again. Before reset_tracks existed, the stale `raised` flag
        // silently swallowed the fault on the second run.
        let mut c = Circuit::new();
        let a = c.node("A");
        let g = c.node("GND");
        let id = c.add(Device::Resistor {
            name: "R1".into(),
            a,
            b: g,
            ohms: 100.0,
            tc1: None,
        });
        let meta = DeviceMeta {
            reference: "R1".into(),
            device: id,
            kind: ComponentKind::Passive,
            // 1/10 W part carrying 10 V / 100 Ω = 1 W: a 10x sustained overpower.
            footprint: String::new(),
            ratings: Ratings {
                max_power_w: Some(0.1),
                ..Default::default()
            },
        };
        let mut mon = StressMonitor::new(vec![meta]);
        // 10 V across A->GND; the other node reads 0.
        let node_v = |n: NodeId| if n == a { 10.0 } else { 0.0 };
        let no_branch = |_: DeviceId| None;

        // Drive the sustain filter to the raise point and count the fault.
        let raise_once = |mon: &mut StressMonitor, c: &mut Circuit| -> usize {
            let mut raises = 0;
            for k in 0..(SUSTAIN_CHUNKS + 2) {
                let faults = mon.evaluate(c, &node_v, &no_branch, k as f64 * 1e-3);
                raises += faults
                    .iter()
                    .filter(|f| f.kind == FaultKind::Overpower && f.component == "R1")
                    .count();
            }
            raises
        };

        assert_eq!(
            raise_once(&mut mon, &mut c),
            1,
            "first run raises the overpower fault once"
        );
        assert!(
            mon.stress_by_ref().get("R1").copied().unwrap_or(0.0) >= 1.0,
            "stress pegged"
        );

        // Without a reset the fault stays latched (raised) and does not re-fire.
        assert_eq!(
            raise_once(&mut mon, &mut c),
            0,
            "latched: no re-raise without reset"
        );

        // reset_tracks clears the latch AND the live stress; the replay re-raises.
        mon.reset_tracks();
        assert!(
            mon.stress_by_ref().is_empty(),
            "reset clears the live stress map"
        );
        assert_eq!(
            raise_once(&mut mon, &mut c),
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
        let mut c = Circuit::new();
        let vcc = c.node("+5V");
        let mut mon = StressMonitor::new(Vec::new());
        mon.set_supply_watches(vec![SupplyWatch {
            reference: "U1".into(),
            node: vcc,
            max_v: 6.0,
        }]);
        let no_branch = |_: DeviceId| None;

        // Nominal 5 V: no fault, modest stress fraction.
        let node_v5 = |n: NodeId| if n == vcc { 5.0 } else { 0.0 };
        for k in 0..(SUSTAIN_CHUNKS + 2) {
            let faults = mon.evaluate(&mut c, &node_v5, &no_branch, k as f64 * 1e-3);
            assert!(faults.is_empty(), "5 V on a 6 V-max part is not a fault");
        }
        let frac = mon.stress_by_ref().get("U1").copied().unwrap_or(0.0);
        assert!(
            (frac - 5.0 / 6.0).abs() < 1e-9,
            "stress fraction 5/6: {frac}"
        );

        // 100 V: sustained overvoltage, raised exactly once, naming the part.
        let node_v100 = |n: NodeId| if n == vcc { 100.0 } else { 0.0 };
        let mut raised = Vec::new();
        for k in 0..(SUSTAIN_CHUNKS + 2) {
            raised.extend(mon.evaluate(&mut c, &node_v100, &no_branch, k as f64 * 1e-3));
        }
        assert_eq!(raised.len(), 1, "raised once, not per chunk");
        assert_eq!(raised[0].component, "U1");
        assert_eq!(raised[0].kind, FaultKind::Overvoltage);
        assert_eq!(raised[0].limit, 6.0);
        assert_eq!(raised[0].value, 100.0);

        // reset_tracks clears the latch: a replay re-raises.
        mon.reset_tracks();
        let mut re_raised = 0;
        for k in 0..(SUSTAIN_CHUNKS + 2) {
            re_raised += mon
                .evaluate(&mut c, &node_v100, &no_branch, k as f64 * 1e-3)
                .len();
        }
        assert_eq!(re_raised, 1, "watch re-raises after reset_tracks");
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
            let meta = DeviceMeta {
                reference: "D1".into(),
                device: id,
                kind: ComponentKind::Diode,
                footprint: String::new(),
                ratings: Ratings::default(),
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
