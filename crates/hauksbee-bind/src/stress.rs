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
//!
//! Non-destructive mode reports continuously and never mutates the circuit.

use std::collections::{BTreeSet, HashMap};

use hauksbee_ir::evidence::{Assumption, AssumptionSource, Scope, Subject};
use hauksbee_ir::{Circuit, Device, DeviceId, NodeId};
use hauksbee_models::derating::{Carrier, Curve, Derating, TempRef};
use hauksbee_models::schema::{ComponentKind, Ratings};
use hauksbee_solve::{mos_body_vth, mos_channel};

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
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Overcurrent => "overcurrent",
            Self::SurgeCurrent => "surge_current",
            Self::Overpower => "overpower",
            Self::Overvoltage => "overvoltage",
            Self::ReverseBias => "reverse_bias",
            Self::PinOvercurrent => "pin_overcurrent",
            Self::Short => "short",
            Self::Overtemperature => "overtemperature",
        }
    }

    /// Inverse of [`Self::as_str`]; unknown strings map to `Overcurrent`.
    // Infallible (unknown maps to Overcurrent), so FromStr and its Err type do not fit.
    #[allow(clippy::should_implement_trait)]
    #[must_use]
    pub fn from_str(s: &str) -> Self {
        match s {
            "surge_current" => Self::SurgeCurrent,
            "overpower" => Self::Overpower,
            "overvoltage" => Self::Overvoltage,
            "reverse_bias" => Self::ReverseBias,
            "pin_overcurrent" => Self::PinOvercurrent,
            "overtemperature" => Self::Overtemperature,
            "short" => Self::Short,
            _ => Self::Overcurrent,
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
    /// The card's current and power rows by temperature; where present they,
    /// not the scalar ratings, set the limits.
    pub derating: Derating,
}

impl DeviceMeta {
    /// The devices carrying path `pins`' current, when the binder measured it.
    fn carriers(&self, pins: &[String]) -> Option<&[Carrier]> {
        let path = self.derating.carriers.iter().find(|(p, _)| p == pins);
        path.map(|(_, d)| d.as_slice()).filter(|d| !d.is_empty())
    }

    /// Whether this device carries path `pins`' current.
    fn on(&self, pins: &[String]) -> bool {
        (self.carriers(pins)).is_some_and(|d| d.iter().any(|c| c.0 == self.device))
    }

    /// The paths `curve` rates this device cannot be judged on one by one:
    /// every one, when any went unmeasured or none carries the device.
    fn path_gap<'a>(&self, curve: &'a Curve) -> Vec<&'a [String]> {
        let paths = curve.paths();
        let measured = paths.iter().all(|p| self.carriers(p).is_some());
        let measured = measured && paths.iter().any(|p| self.on(p));
        paths.into_iter().filter(|_| !measured).collect()
    }

    /// The `(value, limit)` `curve`, read at `at`, holds this device to: with
    /// no path rows, `own` against the least favourable row; else the worst of
    /// every unit's `amps` summed against the whole-part rows (a printed total)
    /// and each path's carriers' against its rows or, with a
    /// [`Self::path_gap`], this unit's heaviest terminal against the least
    /// favourable row.
    fn binding(
        &self,
        curve: &Curve,
        own: f64,
        amps: &dyn Fn(&[Carrier]) -> f64,
        at: &dyn Fn(TempRef) -> f64,
        tj_max_c: f64,
    ) -> Bound {
        let paths = curve.paths();
        if paths.is_empty() {
            return curve.limit(at, tj_max_c).map(|l| (own, l));
        }
        let gap = !self.path_gap(curve).is_empty();
        let unit = (gap.then(|| curve.limit(at, tj_max_c))).flatten();
        let unit = unit.map(|l| (amps(&[(self.device, u8::MAX)]), l));
        let frac = |&(v, l): &(f64, f64)| v / l.max(f64::MIN_POSITIVE);
        let rows = std::iter::once(&[][..]).chain(paths.into_iter().filter(|p| !gap && self.on(p)));
        let limit = |p| curve.on(p).limit(at, tj_max_c);
        let checks = rows.filter_map(|p| Some((amps(self.carriers(p)?), limit(p)?)));
        let all = unit.into_iter().chain(checks);
        all.max_by(|a, b| frac(a).total_cmp(&frac(b)))
    }

    /// Effective power rating (W): explicit `max_power_w`, else derived from the
    /// resistor footprint size. `None` if no power limit is known.
    #[must_use]
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
            .take_while(char::is_ascii_alphabetic)
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
    #[must_use]
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

    #[must_use]
    pub fn power_coverage_gap(&self) -> Option<String> {
        let package = self.power_coverage_package()?;
        Some(format!(
            "stress: {} has no power rating and no readable package \"{package}\", so its \
             overpower check did not run. Give the part a model with ratings.max_power_w, \
             or a footprint / BOM line naming the package, to cover it.",
            self.reference,
        ))
    }

    /// Effective junction-to-ambient thermal resistance (C/W): the card's
    /// `theta_ja_c_per_w` row for this package and mounting, else its scalar,
    /// else derived from the footprint package class. Always returns a value (the footprint default is the
    /// conservative fallback), so every dissipating device gets a temperature.
    #[must_use]
    pub fn theta_ja_c_per_w(&self) -> f64 {
        (self.derating.theta_ja().or(self.ratings.theta_ja_c_per_w))
            .unwrap_or_else(|| crate::thermal::theta_ja_from_footprint(&self.footprint, self.kind))
    }

    /// Effective maximum junction temperature (C): explicit
    /// `max_junction_temp_c` from the model, else the per-package-class default
    /// (150 C for power packages, 125 C otherwise).
    #[must_use]
    pub fn tj_max_c(&self) -> f64 {
        self.ratings
            .max_junction_temp_c
            .unwrap_or_else(|| crate::thermal::default_tj_max(&self.footprint))
    }
}

/// What a resistor's power rating was derived from.
///
/// The rating is only as good as the package evidence behind it, and an
/// overstress verdict that rests on a guessed package must say so rather than
/// read as a measurement.
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
#[must_use]
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
            const SHOWN: usize = 5;
            refs.sort_unstable();
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
#[must_use]
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
        return chip(w);
    }
    // A DIN body code IS size evidence, and the codes are not interchangeable:
    // DIN0204 is a 1/8 W body while DIN0207 is 1/4 W, so treating them alike
    // over-rates the smaller one twofold and suppresses its overpower check.
    // An axial footprint with no DIN code carries no size evidence and falls
    // through to the abstention below.
    if let Some(w) = din_axial_rating(&f) {
        return ResistorPower {
            watts: Some(w),
            basis: ResistorPowerBasis::ThtAxial,
        };
    }
    // The package class may be known but its SIZE is not, or nothing is known
    // at all. Either way there is no rating to derive: a real 0603 behind a
    // custom footprint name is a 1/10 W part, and handing it any floor either
    // invents an overpower fault (if the floor is lower than the truth) or
    // suppresses a real one (if it is higher). It becomes a named coverage
    // gap instead, which is the honest form of not knowing.
    unknown()
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
    /// No usable metric-only code: either no METRIC suffix at all, or `KiCad`'s
    /// dual form whose imperial token is authoritative.
    Absent,
}

/// The run of ASCII digits that ends `s`, in order ("`R_0402`" -> "0402").
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

/// Strip a multi-unit stamping suffix from a device name, yielding the package reference.
///
/// Multi-unit packages stamp one IR device per unit with a suffix
/// on the reference designator: `_q<N>` (dual BJTs, opamp/comparator channels),
/// `_s<N>` (analog-switch banks), `_e<N>` (passive-array elements). "`Q1_q2`"
/// and "`Q1_q1`" are two dice in the one physical package "Q1".
///
/// This is THE unit-suffix rule: the binder uses it to apply a package's
/// ratings to every unit, [`StressMonitor::evaluate`] uses it to pool sibling
/// dissipation through the shared package, and hauksbee-ci mirrors it
/// (`key_belongs_to_ref`) to aggregate per-unit keys under the bare ref. Only
/// an all-digit tail after the marker counts, so "`SW1_heater`" or "`U1_qspi`" are
/// left alone (and "SW1" never claims "SW10"'s units).
#[must_use]
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

/// Each path `derating`'s rows rate on their own, with the devices carrying it.
///
/// Those are the `units` (part `reference`'s monitored devices) with a
/// current-carrying terminal on a node of its roles, each with those terminals
/// ([`Carrier`]), when every role has one in `roles` and no other device named
/// for the part touches them, else none; the empty path lists every unit, for
/// the whole-part rows (a printed total) to bind their sum.
#[must_use]
pub fn rated_paths(
    circuit: &Circuit,
    reference: &str,
    derating: &Derating,
    roles: &HashMap<String, NodeId, impl std::hash::BuildHasher>,
    units: &[DeviceId],
) -> Vec<(Vec<String>, Vec<Carrier>)> {
    rated_paths_indexed(circuit, &DevicesByNode::new(circuit), reference, derating, roles, units)
}

/// Every device touching each node (in circuit order), so a path's carriers
/// are found among its nodes' neighbours instead of by a scan of the whole
/// circuit per path: on an 800-part board the scan made binding quadratic.
#[derive(Debug, Default)]
pub struct DevicesByNode(HashMap<NodeId, Vec<DeviceId>>);

impl DevicesByNode {
    /// Index `circuit` by the nodes each device touches ([`Device::nodes`]).
    #[must_use]
    pub fn new(circuit: &Circuit) -> Self {
        let mut by_node: HashMap<NodeId, Vec<DeviceId>> = HashMap::new();
        for (id, d) in circuit.iter() {
            for n in d.nodes() {
                let at = by_node.entry(n).or_default();
                if at.last() != Some(&id) {
                    at.push(id);
                }
            }
        }
        Self(by_node)
    }

    /// The devices touching any of `nodes` that `keep` accepts, in circuit
    /// order, each once. A rail touches thousands; few are kept.
    fn touching(&self, nodes: &[NodeId], keep: impl Fn(DeviceId) -> bool) -> Vec<DeviceId> {
        let mut ids: Vec<DeviceId> = (nodes.iter())
            .filter_map(|n| self.0.get(n))
            .flatten()
            .copied()
            .filter(|&id| keep(id))
            .collect();
        ids.sort_unstable_by_key(|id| id.0);
        ids.dedup();
        ids
    }
}

/// [`rated_paths`] over a prebuilt [`DevicesByNode`] of `circuit`.
#[must_use]
pub fn rated_paths_indexed(
    circuit: &Circuit,
    index: &DevicesByNode,
    reference: &str,
    derating: &Derating,
    roles: &HashMap<String, NodeId, impl std::hash::BuildHasher>,
    units: &[DeviceId],
) -> Vec<(Vec<String>, Vec<Carrier>)> {
    let carriers = |pins: &[String]| {
        let nodes: Option<Vec<NodeId>> = pins.iter().map(|p| roles.get(p).copied()).collect();
        let on = |n: &NodeId| nodes.iter().flatten().any(|m| m == n);
        let named = |id: DeviceId| {
            units.contains(&id) || circuit.devices[id.0 as usize].name().contains(reference)
        };
        let touching = index.touching(nodes.as_deref().unwrap_or_default(), named);
        let (ours, stray): (Vec<_>, Vec<_>) = (touching.into_iter())
            .map(|id| (id, &circuit.devices[id.0 as usize]))
            .partition(|(id, _)| units.contains(id));
        let bits = |d: &Device| (d.nodes().into_iter().take(3).zip(0..)).filter(|t| on(&t.0));
        let ours = (ours.into_iter()).map(|(id, d)| (id, bits(d).fold(0, |m, t| m | (1 << t.1))));
        ours.filter(|c| c.1 != 0 && stray.is_empty()).collect()
    };
    let paths = (derating.current.paths().into_iter()).chain(derating.power.paths());
    let paths: BTreeSet<_> = paths.map(|p| (p.to_vec(), carriers(p))).collect();
    let whole =
        (!paths.is_empty()).then(|| (Vec::new(), units.iter().map(|&u| (u, u8::MAX)).collect()));
    whole.into_iter().chain(paths).collect()
}

/// A supply-rail absolute-maximum watch for a part that is not an analog device
/// in the solver (an MCU / logic IC): the part's supply NET is checked against
/// the model's `max_voltage_v` each chunk.
///
/// These parts deliberately get no whole-device [`DeviceMeta`] (their per-pin
/// currents are covered by the pin-driver metas). Without this check a rail
/// driven far past the chip's absolute-maximum Vcc would raise nothing at all,
/// which is an honesty gap the model DB can close: it carries the rating (e.g.
/// `ATmega328P` `max_voltage_v = 6.0`).
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
    /// Whether the run set `ambient_c` rather than taking the default.
    pub ambient_given: bool,
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
    /// Each meta's (power rating, `theta_JA`, Tj max): footprint parses, so resolved once.
    limits: Vec<(Option<f64>, f64, f64)>,
    /// Where each regulator meta's dissipation comes from (see [`RegulatorHeat`]).
    reg_heat: Vec<Option<RegulatorHeat>>,
    /// What the board, not any one part's run, undermines: a rail nothing powered.
    pub board_caveats: Vec<Assumption>,
}

/// How a regulator's dissipation is estimated.
///
/// Its monitored device is the ideal output source, which carries no input, so without this a regulator
/// read as dissipating nothing and every `max_temp` on one was skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegulatorHeat {
    /// A linear pass element: `(V(input) - V(out)) * I_out`.
    Linear { input: NodeId },
    /// A power-path part: the linear system rail as [`Self::Linear`], plus a
    /// second pass path from the same `input` out through the charge-port
    /// ammeter `port`, `(V(input) - V(port.p)) * I_port`.
    LinearWithPort { input: NodeId, port: DeviceId },
    /// An averaged converter: the input power its in-circuit draw, a behavioural
    /// current source, takes from its `p` node, minus the (signed) output
    /// power. The draw carries the card's efficiency both ways, so this is
    /// `|Pout| * (1/eff - 1)` forward and `|Pout| * (1 - eff)` under reverse
    /// flow: every conversion loss, the inductor's included, charged to the
    /// package and never negative.
    Converter { draw: DeviceId },
}

impl RegulatorHeat {
    fn watts(
        self,
        circuit: &Circuit,
        source: DeviceId,
        node_v: &dyn Fn(NodeId) -> f64,
        branch_current: &dyn Fn(DeviceId) -> Option<f64>,
    ) -> f64 {
        let Some(Device::Vsource { p, .. }) = circuit.devices.get(source.0 as usize) else {
            return 0.0;
        };
        let i_out = -branch_current(source).unwrap_or(0.0);
        let (input, i_in, i_out) = match self {
            Self::Linear { input } | Self::LinearWithPort { input, .. } => {
                (input, i_out.abs(), i_out.abs())
            }
            Self::Converter { draw } => {
                let (input, i_in) = crate::behavioral::behavioral_draw(circuit, draw, node_v);
                (input, i_in, i_out)
            }
        };
        let rail = node_v(input).mul_add(i_in, -node_v(*p) * i_out).max(0.0);
        let charge = match self {
            Self::LinearWithPort { input, port } => match circuit.devices.get(port.0 as usize) {
                Some(Device::Vsource { p: bat, .. }) => {
                    let i_port = branch_current(port).unwrap_or(0.0).abs();
                    ((node_v(input) - node_v(*bat)) * i_port).max(0.0)
                }
                _ => 0.0,
            },
            _ => 0.0,
        };
        rail + charge
    }
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
    /// [`OperatingPoint::terminals`] through the same lag: a rated path's measure.
    lagged_terminals: [f64; 3],
    /// Whether [`Self::lagged_a`] ever reached the continuous current rating,
    /// which is what separates a real overload from a pulse the card cannot
    /// judge.
    lag_over_limit: bool,
    /// Lowest gate drive (V, folded into N-channel space) seen while a MOSFET
    /// carried a meaningful current. `None` if it never conducted.
    worst_vgs: Option<f64>,
    /// Hottest estimated junction temperature (C). `None` if it never dissipated.
    peak_tj_c: Option<f64>,
    /// With the ambient defaulted, a current or power check that passed at it
    /// would have failed at [`HOT_PROBE_AMBIENT_C`] or the hottest printed row.
    hot_flip: bool,
    /// On a package-guessed `theta_JA`, a derated current or power check that
    /// passed would have failed at twice the guess.
    theta_flip: bool,
    /// The endpoint current and the derated continuous limit in force when it
    /// stood furthest over that limit.
    peak_vs_limit: Option<(f64, f64)>,
    /// The deposits `(elapsed s, energy J)` inside the trailing [`TJ_AVG_S`]
    /// window, with their running sums: the junction heats on this average.
    heat: std::collections::VecDeque<(f64, f64)>,
    heat_sum: (f64, f64),
}

impl RunStats {
    /// Add one chunk's deposit and return the average power (W) over the
    /// trailing [`TJ_AVG_S`] (all of it while the run is shorter).
    fn heat_w(&mut self, elapsed_s: f64, energy_j: f64) -> f64 {
        self.heat.push_back((elapsed_s, energy_j));
        self.heat_sum = (self.heat_sum.0 + elapsed_s, self.heat_sum.1 + energy_j);
        while self.heat.len() > 1 {
            let (t, e) = self.heat[0];
            if self.heat_sum.0 - t < TJ_AVG_S {
                break;
            }
            self.heat.pop_front();
            self.heat_sum = (self.heat_sum.0 - t, self.heat_sum.1 - e);
        }
        self.heat_sum.1 / self.heat_sum.0
    }
}

/// Window (s) the junction-heating power is averaged over. The steady-state
/// `Ta + P*theta_JA` estimate used to take one chunk's power, so a
/// microseconds-long switching or ringing excursion read as a sustained load
/// (a load switch "reached" 5e6 C). Junction-to-ambient thermal time
/// constants of even the smallest packages are far longer than a
/// millisecond, so this still reads a sustained load in full after one window;
/// what it no longer does is extrapolate a single chunk to steady state.
const TJ_AVG_S: f64 = 1e-3;

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
/// (Infineon `IRF540NPbF` Rev. 2.1 note 3, and the same convention across AOS,
/// onsemi and Vishay parts), so anything shorter is pulsed by the datasheets'
/// own definition. Rounded up to 1 ms, one order above the solver's chunk.
const IDM_PULSE_S: f64 = 1e-3;

/// The ambient (C) a defaulted ambient is also judged at, so a card whose
/// rows all sit at 25 C still discloses a default its verdict rests on.
const HOT_PROBE_AMBIENT_C: f64 = 85.0;

/// Fraction of the specified VGS a gate may fall short by and still count as
/// driven. A gate series resistor's static drop into the pulldown is a few mV,
/// not a drive deficiency, and no datasheet specifies its test VGS finer.
const GATE_DRIVE_TOLERANCE: f64 = 0.02;

impl Default for StressMonitor {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl StressMonitor {
    /// Build a monitor over the device metadata gathered at bind time.
    #[must_use]
    pub fn new(metas: Vec<DeviceMeta>) -> Self {
        let n = metas.len();
        Self {
            limits: (metas.iter())
                .map(|m| (m.power_rating_w(), m.theta_ja_c_per_w(), m.tj_max_c()))
                .collect(),
            metas,
            tracks: vec![DeviceTrack::default(); n],
            supply_watches: Vec::new(),
            watch_tracks: Vec::new(),
            destructive: false,
            ambient_c: crate::thermal::DEFAULT_AMBIENT_C,
            ambient_given: false,
            stress_by_ref: HashMap::new(),
            temp_by_ref: HashMap::new(),
            chunk_energy_j: vec![0.0; n],
            chunk_elapsed_s: 0.0,
            run: vec![RunStats::default(); n],
            reg_heat: vec![None; n],
            board_caveats: Vec::new(),
        }
    }

    /// Estimate the dissipation of the regulator whose output source is
    /// `source` by `heat`.
    pub fn watch_regulator_heat(&mut self, source: DeviceId, heat: RegulatorHeat) {
        for (meta, slot) in self.metas.iter().zip(&mut self.reg_heat) {
            if meta.device == source {
                *slot = Some(heat);
            }
        }
    }

    /// The operating point of meta `i`, with a regulator's estimated dissipation.
    fn op_of(
        &self,
        circuit: &Circuit,
        i: usize,
        node_v: &dyn Fn(NodeId) -> f64,
        branch_current: &dyn Fn(DeviceId) -> Option<f64>,
    ) -> OperatingPoint {
        let meta = &self.metas[i];
        let mut op = operating_point(circuit, meta, node_v, branch_current);
        if let Some(heat) = self.reg_heat[i] {
            op.power_w = heat.watts(circuit, meta.device, node_v, branch_current);
        }
        op
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
    #[must_use]
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
            if let Some((pk, cont)) = s
                .peak_vs_limit
                .filter(|(pk, c)| !s.lag_over_limit && rt.max_surge_current_a.is_none() && pk > c)
            {
                out.push(Assumption::partial_model_for_component(&format!("{r}-peak-current"), r, "",
                    &format!("{r} peaks at {pk:.3} A, above the {cont:.3} A its card rates CONTINUOUSLY, and the card carries no pulsed (IDM) rating for the peak to be judged against"),
                    "add the datasheet's pulsed drain/forward current to the card, then re-run"));
            }
        }
        out
    }

    /// What a card's temperature-dependent ratings leave open in this run:
    /// an ambient nobody set, when the part's hottest printed rating
    /// temperature would have flipped its current or power verdict; a limit
    /// read past the hottest row on a card with no Tj,max; and the package and
    /// mounting rows a part was fitted with on no evidence.
    #[must_use]
    pub fn rating_disclosures(&self) -> Vec<Assumption> {
        let (mut out, mut said) = (Vec::new(), std::collections::HashSet::new());
        for ((m, s), limits) in self.metas.iter().zip(&self.run).zip(&self.limits) {
            let r = strip_unit_suffix(&m.reference);
            let fresh = (m.derating.caveats.iter()).filter(|a| said.insert((r, a.id().as_str())));
            out.extend(fresh.cloned());
            let Some(hot) = m.derating.hottest() else {
                continue;
            };
            if s.hot_flip && said.insert((r, "ambient")) {
                let at = format!("{} C", self.ambient_c);
                out.push(Assumption::default_parameter_for_component(
                    r,
                    r,
                    "ambient_c",
                    &at,
                ));
            }
            let curves = [&m.derating.current, &m.derating.power];
            let gap = curves.map(|c| m.path_gap(c)).concat();
            if !gap.is_empty() && !m.derating.carriers.is_empty() && said.insert((r, "paths")) {
                out.push(Assumption::partial_model_for_component(&format!("{r}-rating-paths"), r, "",
                    &format!("{r}'s card rates current or power per path {gap:?}, but this model does not carry each through its pins, so {} is held to the least favourable row", m.reference),
                    "bind every rated path's pins to a net the part's model stamps, then re-run"));
            }
            let seen = s.peak_tj_c.unwrap_or(self.ambient_c).max(self.ambient_c);
            if m.ratings.max_junction_temp_c.is_none() && seen > hot && said.insert((r, "tj")) {
                out.push(Assumption::partial_model_for_component(&format!("{r}-tj-max"), r, "",
                    &format!("{r}'s current and power limits past their hottest printed row ({hot} C) fall to zero at a {} C junction limit assumed from its package, because its card carries no max_junction_temp_c", limits.2),
                    "add the datasheet's max_junction_temp_c to the card, then re-run"));
            }
        }
        out
    }

    /// Every disclosure a verdict with no claims of its own carries: the
    /// board's, the ratings', the run's, and a guessed `theta_JA` wherever it is decisive.
    #[must_use]
    pub fn disclosures(&self) -> Vec<Assumption> {
        let mut said = self.board_caveats.clone();
        said.extend(self.rating_disclosures());
        said.extend(self.run_disclosures());
        said.extend(self.inferred_theta_ja(|_, d| d.then_some(Scope::Board).into_iter().collect()));
        said
    }

    /// A junction temperature judged on a `theta_JA` inferred from the package,
    /// because the card carries none, qualifies each claim it feeds.
    /// `claims(reference, decisive)` names those claims' scopes; `decisive` is
    /// true when twice the inferred `theta_JA` would put the peak at or over the
    /// part's Tj limit, or flip a case- or junction-referred current or power
    /// rating, so a verdict drawn from the limit rests on the guess.
    pub fn inferred_theta_ja(&self, claims: impl Fn(&str, bool) -> Vec<Scope>) -> Vec<Assumption> {
        let mut out = Vec::new();
        for (m, s) in self.metas.iter().zip(&self.run) {
            let carded = m.ratings.theta_ja_c_per_w.is_some() || m.derating.theta_ja().is_some();
            let Some(peak) = s.peak_tj_c.filter(|_| !carded) else {
                continue;
            };
            let (r, theta, limit) = (&m.reference, m.theta_ja_c_per_w(), m.tj_max_c());
            let decisive = s.theta_flip || 2.0f64.mul_add(peak, -self.ambient_c) >= limit;
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
    #[must_use]
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
    #[must_use]
    pub const fn device_count(&self) -> usize {
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
        self.tracks.fill_with(DeviceTrack::default);
        self.watch_tracks.fill_with(WatchTrack::default);
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
        (0..self.metas.len())
            .map(|i| {
                // An ideal capacitor or inductor stores energy and dissipates
                // none (`operating_point` reports zero power for both), so a
                // per-step operating point for one, often half the parts on a
                // board, is work spent to learn zero. A regulator's estimated
                // heat is the exception, so a meta carrying one is still read.
                let stores_only = self.reg_heat[i].is_none()
                    && matches!(
                        circuit.devices.get(self.metas[i].device.0 as usize),
                        Some(Device::Capacitor { .. } | Device::Inductor { .. })
                    );
                if self.tracks[i].destroyed || stores_only {
                    0.0
                } else {
                    self.op_of(circuit, i, node_v, branch_current).power_w
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
        if self.tracks[i].raised.insert(chk.kind.as_str(), true) == Some(true) {
            return false;
        }
        let destroyed = self.maybe_destroy(circuit, meta, chk.kind);
        self.tracks[i].destroyed |= destroyed;
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
        if elapsed_s <= 0.0 || !elapsed_s.is_finite() || energy_j.len() != self.chunk_energy_j.len()
        {
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
        if dt <= 0.0 || !dt.is_finite() {
            return;
        }
        let powers = self.step_powers(circuit, node_v, branch_current);
        self.deposit_chunk_energy(&powers.iter().map(|p| p * dt).collect::<Vec<f64>>(), dt);
    }

    /// Zero the pending chunk energy deposit.
    fn clear_chunk_energy(&mut self) {
        self.chunk_energy_j.fill(0.0);
        self.chunk_elapsed_s = 0.0;
    }

    /// Live stress fraction per component reference (0..1).
    #[must_use]
    pub const fn stress_by_ref(&self) -> &HashMap<String, f64> {
        &self.stress_by_ref
    }

    /// Live estimated steady-state junction temperature (C) per component
    /// reference, for dissipating devices.
    #[must_use]
    pub const fn temp_by_ref(&self) -> &HashMap<String, f64> {
        &self.temp_by_ref
    }

    /// Judge one solved DC operating point as a steady state: the same
    /// ratings, derating and package pooling as [`Self::evaluate`], with the
    /// operating point held for the sustain window so a continuous rating can
    /// trip. A board resting in this state for any length of time is exactly
    /// the sustained case the window exists to recognise.
    pub fn evaluate_steady(
        &mut self,
        circuit: &mut Circuit,
        node_v: &dyn Fn(NodeId) -> f64,
        branch_current: &dyn Fn(DeviceId) -> Option<f64>,
    ) -> Vec<FaultEvent> {
        self.clear_chunk_energy();
        (0..SUSTAIN_CHUNKS)
            .flat_map(|_| self.evaluate(circuit, node_v, branch_current, 0.0))
            .collect()
    }

    /// Evaluate every monitored device for the chunk just solved.
    ///
    /// `node_v(node)` returns the node voltage; `branch_current(id)` returns the
    /// branch current for a `Vsource`/`Inductor` device if the layout owns one.
    /// `t` is the current sim time. Returns any faults newly raised this chunk.
    #[allow(clippy::too_many_lines)] // the three documented passes share the sampled state
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
        let ops: Vec<Option<OperatingPoint>> = (0..self.metas.len())
            .map(|i| {
                (!self.tracks[i].destroyed).then(|| self.op_of(circuit, i, node_v, branch_current))
            })
            .collect();
        // Carrier paths ask for the first LIVE meta with an ID. Index that once
        // per solved snapshot instead of scanning every meta for every path/check.
        let mut live_meta = HashMap::with_capacity(ops.len());
        for (i, op) in ops.iter().enumerate() {
            if op.is_some() {
                live_meta.entry(self.metas[i].device).or_insert(i);
            }
        }
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
            let terminals = run.lagged_terminals.iter_mut().zip(op.terminals);
            terminals.for_each(|(lag, a)| *lag += (a - *lag) * alpha);
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
        let unit_avg_w: Vec<f64> = (ops.iter().enumerate())
            .map(|(i, op)| match op {
                None => 0.0, // destroyed: dissipates nothing
                Some(_) if thermal_elapsed > 0.0 => self.chunk_energy_j[i] / thermal_elapsed,
                Some(op) => op.power_w,
            })
            .collect();
        let mut package_power: HashMap<String, f64> = HashMap::new();
        for (i, meta) in self.metas.iter().enumerate() {
            let heat_w = match (&ops[i], self.chunk_energy_j.get(i)) {
                (Some(_), Some(&e)) if thermal_elapsed > 0.0 => {
                    self.run[i].heat_w(thermal_elapsed, e)
                }
                _ => unit_avg_w[i],
            };
            if heat_w > 0.0 {
                *package_power
                    .entry(strip_unit_suffix(&meta.reference).to_string())
                    .or_insert(0.0) += heat_w;
            }
        }
        // The deposit covers exactly the chunk this evaluate closes; drain it
        // so the next chunk starts from zero (no cross-chunk carry-over).
        self.clear_chunk_energy();

        // ── Pass 2: per-device checks ────────────────────────────────────────
        // Iterate by index so we can borrow tracks mutably alongside metas.
        for i in 0..self.metas.len() {
            let mut meta = self.metas[i].clone();
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
            let (peak_a, own) = (op.current_a, (self.run[i].lagged_a, unit_avg_w[i]));
            let (mut rated_w, theta_ja, tj_max) = self.limits[i];
            let package_w = package_power
                .get(strip_unit_suffix(&meta.reference))
                .copied()
                .unwrap_or(0.0);
            // A card printing its current or power rating at several
            // temperatures is judged on the row for where the part sits now,
            // and one rating paths on the current and power of each.
            let sum = |ids: &[Carrier]| {
                let unit = |&(d, bits): &Carrier| {
                    let &j = live_meta.get(&d)?;
                    let on = (self.run[j].lagged_terminals.iter())
                        .zip(0..)
                        .filter(|t| bits & (1 << t.1) != 0);
                    Some((on.map(|t| *t.0).fold(0.0, f64::max), unit_avg_w[j]))
                };
                (ids.iter().filter_map(unit)).fold((0.0, 0.0), |(a, w), u| (a + u.0, w + u.1))
            };
            let read = |theta, ta| derated(&self.metas[i], theta, ta, package_w, tj_max, own, &sum);
            let now = read(theta_ja, self.ambient_c);
            meta.ratings.max_current_a = now.0.map(|c| c.1).or(meta.ratings.max_current_a);
            meta.ratings.max_power_w = now.1.map(|w| w.1).or(meta.ratings.max_power_w);
            // A switch's pins take the card's path rows, a printed total their sum.
            if meta.kind == ComponentKind::AnalogSwitch && !meta.derating.current.paths().is_empty()
            {
                meta.ratings.max_pin_current_a =
                    now.0.map(|c| c.1).or(meta.ratings.max_pin_current_a);
            }
            rated_w = now.1.map(|w| w.1).or(rated_w);
            let op = &OperatingPoint {
                current_a: now.0.map_or(own.0, |c| c.0),
                power_w: now.1.map_or(own.1, |w| w.0),
                ..*op
            };
            // Whether the verdict would flip on limits read somewhere else: a
            // defaulted ambient's hot probe, or twice a guessed theta_JA.
            let over = |c: Option<(f64, f64)>| {
                c.is_some_and(|(v, l)| v > l * (1.0 + OVER_LIMIT_NOISE_MARGIN))
            };
            let flips = |(a, w)| (over(a) && !over(now.0)) || (over(w) && !over(now.1));
            let hot = (meta.derating.hottest()).map(|h| h.max(HOT_PROBE_AMBIENT_C));
            let hot_flip = hot.is_some_and(|h| !self.ambient_given && flips(read(theta_ja, h)));
            let guessed = meta.ratings.theta_ja_c_per_w.is_none();
            let theta_flip = guessed && flips(read(2.0 * theta_ja, self.ambient_c));
            let run = &mut self.run[i];
            run.lag_over_limit |= meta.ratings.max_current_a.is_some_and(|c| op.current_a > c);
            if let Some(c) = meta
                .ratings
                .max_current_a
                .filter(|c| run.peak_vs_limit.is_none_or(|(p, l)| peak_a / c > p / l))
            {
                run.peak_vs_limit = Some((peak_a, c));
            }
            run.hot_flip |= hot_flip;
            run.theta_flip |= theta_flip;
            let mut checks = build_checks(&meta, op, peak_a, rated_w);

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
            if package_w > 0.0 {
                let tj = crate::thermal::junction_temp_c(self.ambient_c, package_w, theta_ja);
                self.temp_by_ref.insert(meta.reference.clone(), tj);
                let peak = &mut self.run[i].peak_tj_c;
                *peak = Some(peak.map_or(tj, |p| p.max(tj)));
                checks.push(Check {
                    kind: FaultKind::Overtemperature,
                    value: tj,
                    limit: tj_max,
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
                } else if chk.limit > 0.0 || chk.value.abs() > 0.0 {
                    // A limit read down to zero past Tj,max holds any current over it.
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
    /// |I| (A) into each of the device's first three nodes (`Device::nodes`
    /// order): `current_a` through a two-terminal's pair, `|IC|`, `|IB|` and
    /// `|IC + IB|` on a BJT, the channel's at a MOSFET's drain and source.
    terminals: [f64; 3],
}

impl OperatingPoint {
    /// A two-terminal point: `|I|`, the signed across-voltage, dissipation.
    const fn new(current_a: f64, voltage_v: f64, power_w: f64) -> Self {
        Self {
            vgs: None,
            current_a,
            voltage_v,
            power_w,
            terminals: [current_a, current_a, 0.0],
        }
    }
}

/// Compute a device's operating point from the chunk's solved state.
#[allow(clippy::too_many_lines)] // one arm per device kind
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
            let vbe = sign * (node_v(*b) - node_v(*e));
            let vbc = sign * (node_v(*b) - node_v(*c));
            let (ic, ib) = bjt_terminal_currents(model, vbe, vbc, vt, circuit.temp_c);
            let vce = node_v(*c) - node_v(*e);
            let i_worst = ic.abs().max(ib.abs()).min(1e3);
            let power = (vce * ic).abs().min(1e6) + (sign * vbe * ib).abs().min(1e6);
            let mut op = OperatingPoint::new(i_worst, vce, power);
            op.terminals = [ic, ib, ic + ib].map(|i| i.abs().min(1e3));
            op
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
            // The solver's own Level 1 channel and body effect
            // (`hauksbee_solve::mos_channel`) at the sampled node voltages, so
            // the monitor sees the current the simulated channel carries,
            // never a drifted copy of the equations. This arm used
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
            let (mut rd, mut rs) = (model.rd.max(0.0), model.rs.max(0.0));
            if vd < vs {
                std::mem::swap(&mut vd, &mut vs);
                std::mem::swap(&mut rd, &mut rs);
            }
            let vgs = vg - vs;
            let vds_f = vd - vs;

            // `gamma == 0` (most models) never reads the bulk voltage.
            let vth = match b {
                Some(bulk) if model.gamma > 0.0 => {
                    mos_body_vth(model, sign.mul_add(node_v(*bulk), -vs)).0
                }
                _ => model.vto,
            };
            let nvt = model.n_sub.max(1.0) * hauksbee_ir::thermal_voltage_c(circuit.temp_c);
            // Channel-length modulation is always applied here (the solver
            // gates it on a sim option the monitor cannot see); lambda is 0
            // for most models, and when it isn't, including it errs toward
            // the slightly *higher* current, conservative for a limit check.
            let channel = |vgs: f64, vds: f64| {
                mos_channel(model.beta(), nvt, vgs - vth, vds, model.lambda, 0.0).0
            };
            // The solver puts the channel between internal nodes behind
            // `rd`/`rs`, so the terminal voltages overstate its drive: a 20 mOhm
            // channel behind 100 mOhm read at the terminals carries ~6x the
            // current and power the solve settled at. Solve the one current
            // both agree on, `I = channel(vgs - I*rs, vds - I*(rd+rs))`, by
            // bisection (the right side falls as I rises).
            let (mut lo, mut hi) = (0.0, channel(vgs, vds_f).max(0.0));
            for _ in 0..(if rd + rs > 0.0 { 60 } else { 0 }) {
                let mid = 0.5 * (lo + hi);
                if channel(vgs - mid * rs, vds_f - mid * (rd + rs)) > mid {
                    lo = mid;
                } else {
                    hi = mid;
                }
            }
            let ids = hi;
            // Report the real (unfolded) drain-source voltage; the fold and
            // swap preserve its magnitude, so |vds·ids| is the channel
            // dissipation either way. Clamps mirror the BJT arm.
            let vds = node_v(*d) - node_v(*s);
            // `vgs` rides along folded: it is what decides whether the card's
            // RDS(on) applies at all, and only this arm can compute it.
            let id = ids.abs().min(1e3);
            OperatingPoint {
                vgs: Some(vgs),
                terminals: [id, 0.0, id],
                ..OperatingPoint::new(id, vds, (vds_f * ids).abs().min(1e6))
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

/// A card's current and power limits read off its printed rows at ambient
/// `ta_c` with the package dissipating `package_w`, falling to zero at
/// `tj_max_c` past the hottest row: TA rows at the ambient, TC rows at the case
/// (Tj less `P * theta_JC`, or Tj itself, an upper bound, when the card has no
/// `theta_JC`), lead, board and TJ rows at Tj. Each is a `(value, limit)`
/// ([`DeviceMeta::binding`]) of `own` `(A, W)` or, per path, `sum(devices)`;
/// `None` where no row is tagged.
fn derated(
    meta: &DeviceMeta,
    theta_ja: f64,
    ta_c: f64,
    package_w: f64,
    tj_max_c: f64,
    own: (f64, f64),
    sum: &dyn Fn(&[Carrier]) -> (f64, f64),
) -> (Bound, Bound) {
    let tj = crate::thermal::junction_temp_c(ta_c, package_w, theta_ja);
    let jc = meta.ratings.theta_jc_c_per_w;
    let tc = jc.map_or(tj, |jc| package_w.max(0.0).mul_add(-jc, tj).max(ta_c));
    let at = |r| match r {
        TempRef::Ambient => ta_c,
        TempRef::Case => tc,
        TempRef::Lead | TempRef::Junction => tj,
    };
    let (d, amps, watts) = (&meta.derating, |d: &[_]| sum(d).0, |d: &[_]| sum(d).1);
    let power = meta.binding(&d.power, own.1, &watts, &at, tj_max_c);
    (meta.binding(&d.current, own.0, &amps, &at, tj_max_c), power)
}

/// The `(value, limit)` a current or power rating holds a device to.
type Bound = Option<(f64, f64)>;

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
fn build_checks(meta: &DeviceMeta, op: &OperatingPoint, peak_a: f64, w: Option<f64>) -> Vec<Check> {
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
            check(FaultKind::Overpower, op.power_w, w);
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

/// Collector and base current magnitudes `(ic, ib)` the BJT device `id`
/// carries at the node voltages `node_v`, read the way the stress monitor
/// reads them; `None` when `id` is not a BJT.
#[must_use]
pub fn bjt_currents(
    circuit: &Circuit,
    id: DeviceId,
    node_v: &dyn Fn(NodeId) -> f64,
) -> Option<(f64, f64)> {
    let Some(Device::Bjt { c, b, e, model, .. }) = circuit.devices.get(id.0 as usize) else {
        return None;
    };
    let sign = match model.polarity {
        hauksbee_ir::Polarity::N => 1.0,
        hauksbee_ir::Polarity::P => -1.0,
    };
    let vt = hauksbee_ir::thermal_voltage_c(circuit.temp_c);
    let vbe = sign * (node_v(*b) - node_v(*e));
    let vbc = sign * (node_v(*b) - node_v(*c));
    let (ic, ib) = bjt_terminal_currents(model, vbe, vbc, vt, circuit.temp_c);
    Some((ic.abs().min(1e3), ib.abs().min(1e3)))
}

/// Collector and base current `(ic, ib)` of a BJT, polarity-folded, from its
/// TERMINAL voltages `vbe`/`vbc`.
///
/// The solver puts the Gummel-Poon core behind the card's `rb`/`rc`/`re` on
/// internal nodes, so the terminals read the junctions plus the ohmic drops.
/// Feeding terminal voltages straight into the exponentials (as this arm once
/// did) turned a BC847 (`rb` = 10 ohm) carrying 250 mA of base current into
/// a reading of 1000 A (the clamp), 2 MW and a 1.25e9 C junction: the
/// fault fired, but every number it printed was false. Same physics as
/// [`diode_current`]: recover the junction voltages first, by 2-D Newton on
///
/// ```text
///   vbe = vbe_j + ib*rb + (ic + ib)*re
///   vbc = vbc_j + ib*rb - ic*rc
/// ```
///
/// with each junction step limited to 0.1 V so the exponentials cannot run
/// away from a terminal-voltage start far above the junction drop.
fn bjt_terminal_currents(
    model: &hauksbee_ir::BjtModel,
    vbe: f64,
    vbc: f64,
    vt: f64,
    temp_c: f64,
) -> (f64, f64) {
    let is = model.is_at(temp_c);
    let (fwd_vt, rev_vt) = (model.nf * vt, model.nr * vt);
    let core = |vbe_j: f64, vbc_j: f64| {
        let ef = (vbe_j / fwd_vt).clamp(-100.0, 200.0).exp();
        let er = (vbc_j / rev_vt).clamp(-100.0, 200.0).exp();
        let (cf, cr) = (is * (ef - 1.0), is * (er - 1.0));
        let (gf, gr) = (is * ef / fwd_vt, is * er / rev_vt);
        let ic = (cf - cr) - cr / model.br;
        let ib = cf / model.bf + cr / model.br;
        // d(ic)/d(vbe_j, vbc_j), d(ib)/d(vbe_j, vbc_j)
        let dic = (gf, -gr * (1.0 + 1.0 / model.br));
        let dib = (gf / model.bf, gr / model.br);
        (ic, ib, dic, dib)
    };
    let (rb0, rc, re) = (model.rb.max(0.0), model.rc.max(0.0), model.re.max(0.0));
    if rb0 + rc + re <= 0.0 {
        let (ic, ib, ..) = core(vbe, vbc);
        return (ic, ib);
    }
    // A junction never sits far above ~1 V; start there or below.
    let (mut xf, mut xr) = (vbe.min(0.8), vbc.min(0.8));
    for _ in 0..200 {
        let (ic, ib, dic, dib) = core(xf, xr);
        // A card that modulates its base resistance (IRB/RBM) sets it from
        // the base current, held fixed within each step as the solver does.
        let rb = model.base_resistance(ib, 1.0).max(0.0);
        let f1 = (ic + ib).mul_add(re, ib.mul_add(rb, xf)) - vbe;
        let f2 = ic.mul_add(-rc, ib.mul_add(rb, xr)) - vbc;
        let j11 = re.mul_add(dic.0 + dib.0, rb.mul_add(dib.0, 1.0));
        let j12 = re.mul_add(dic.1 + dib.1, rb * dib.1);
        let j21 = rb.mul_add(dib.0, -(rc * dic.0));
        let j22 = rc.mul_add(-dic.1, rb.mul_add(dib.1, 1.0));
        let det = j11.mul_add(j22, -(j12 * j21));
        if !det.is_finite() || det.abs() < 1e-300 {
            break;
        }
        let dx = (f1 * j22 - f2 * j12) / det;
        let dy = (j11 * f2 - j21 * f1) / det;
        let (dx, dy) = (dx.clamp(-0.1, 0.1), dy.clamp(-0.1, 0.1));
        xf -= dx;
        xr -= dy;
        if dx.abs() < 1e-12 && dy.abs() < 1e-12 {
            break;
        }
    }
    let (ic, ib, ..) = core(xf, xr);
    (ic, ib)
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
    let i = model.is_at(temp_c) * exp_arg.exp_m1();
    i.clamp(-1e3, 1e3)
}

/// Thermal utilisation as a RISE-based fraction: (Tj − `ambient)/(Tj_max` −
/// ambient). An idle part reads ~0, comparable with the power/current/voltage
/// checks, which are all true 0-at-idle ratios, and a part at its junction
/// limit reads 1.0. An absolute-Celsius ratio (`Tj/Tj_max`) instead floors every
/// dissipating part at ~`ambient/Tj_max` (~0.2), giving the exported heat-map a
/// spurious floor. The trip threshold is the same either way: frac > 1 ⟺ Tj >
/// `Tj_max` under both. Clamped at 0 so a below-ambient junction never reads
/// negative.
fn thermal_stress_frac(tj_c: f64, tj_max_c: f64, ambient_c: f64) -> f64 {
    let span = tj_max_c - ambient_c;
    if span > 0.0 {
        ((tj_c - ambient_c) / span).max(0.0)
    } else if tj_c > tj_max_c {
        f64::INFINITY // an ambient at or past Tj,max: over it however little the part heats
    } else {
        0.0
    }
}

#[cfg(test)]
mod monitor_temp_tests {
    use super::*;

    /// R15: the exported thermal stress fraction is rise-based, so a lightly-
    /// loaded part reads ~0, not the ~0.2 floor the absolute `Tj/Tj_max` ratio gave
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
        assert!(thermal_stress_frac(25.0, 125.0, 25.0).abs() < 1e-12);
        // Halfway up the rise band reads 0.5.
        assert!((thermal_stress_frac(75.0, 125.0, 25.0) - 0.5).abs() < 1e-9);
        // At the junction limit reads exactly 1.0 (the trip boundary is preserved).
        assert!((thermal_stress_frac(125.0, 125.0, 25.0) - 1.0).abs() < 1e-9);
        // Over the limit reads > 1 (still trips).
        assert!(thermal_stress_frac(150.0, 125.0, 25.0) > 1.0);
        // A below-ambient junction never reads negative.
        assert!(thermal_stress_frac(10.0, 125.0, 25.0).abs() < 1e-12);
        // An ambient past Tj,max is over it however little the part heats.
        assert!(thermal_stress_frac(161.0, 150.0, 160.0) > 1.0);
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
            // KiCad's dual form pairs the imperial size code with its metric one,
            // and a small part's metric code equals a larger part's imperial code,
            // so the imperial token stays authoritative: 0201 imperial (1/20 W),
            // NOT 0603 metric read as imperial. Matching the metric suffix first
            // over-rates tiny parts by up to 2x.
            ("Resistor_SMD:R_0201_0603Metric", 1.0 / 20.0),
            ("Resistor_SMD:R_01005_0402Metric", 1.0 / 32.0),
            ("Resistor_SMD:R_0402_1005Metric", 1.0 / 16.0),
            ("Resistor_SMD:R_1206_3216Metric", 1.0 / 4.0),
            ("R_0402_1005Metric", 1.0 / 16.0),
            ("R_0603_1608Metric", 1.0 / 10.0),
            ("R_0805_2012Metric", 1.0 / 8.0),
            ("R_1206_3216Metric", 1.0 / 4.0),
            // Real KiCad names carry pad dimensions after the size code. Treating
            // those digits as a second size code sent the name to the imperial
            // pass, which read metric 0402 as imperial 0402 and doubled the rating.
            ("Resistor_SMD:R_0402Metric_Pad0.74x0.62mm", 1.0 / 32.0),
            ("Resistor_SMD:R_0603Metric_Pad0.98x0.95mm", 1.0 / 20.0),
            ("Resistor_SMD:R_3216Metric_Pad1.42x1.75mm", 1.0 / 4.0),
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
            derating: Derating::default(),
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

    /// A synthetic dual-input power mux: IN1 to OUT rated 500 mA, IN2 to OUT
    /// 140 mA (or as given), 550 mA in total, the scalars the least favourable
    /// row, each path a 1 ohm unit into a grounded OUT. Each path is judged on
    /// its own current and the total on their sum; a path this model cannot
    /// measure (a pad left off the board, another device of the part on its
    /// net) or a role the card's pins lack holds each unit to the least
    /// favourable row, and only the unmeasured path says so, the total still
    /// binding the sum. An analog switch's pins, once judged on the scalar
    /// alone, take the same rows, and keep the scalar with no row per path.
    #[test]
    fn a_rating_printed_per_path_judges_the_current_through_that_path() {
        let row = |a: f64, pins: &str| {
            format!("[[models.source.characteristic]]\nparameter = \"IOUT\"\nrating = \"max_current_a\"\nmax = {a}\nunit = \"A\"\nconditions = \"TA = 25 °C\"\nbasis = \"b\"\npins = [{pins}]\n")
        };
        let (pa, sw) = (ComponentKind::Passive, ComponentKind::AnalogSwitch);
        // (kind, IN1 A, IN2 A, IN2's row A and roles, roles bound, a stray part device on IN1, fault, disclosed)
        #[rustfmt::skip]
        let cases = [
            (pa, 0.4, 0.0, 0.14, "\"in2\"", ["in1", "in2"], false, false, false),
            (pa, 0.0, 0.2, 0.14, "\"in2\"", ["in1", "in2"], false, true, false),
            (pa, 0.4, 0.13, 0.14, "\"in2\"", ["in1", "in2"], false, false, false),
            (pa, 0.45, 0.13, 0.14, "\"in2\"", ["in1", "in2"], false, true, false),
            (pa, 0.4, 0.0, 0.14, "\"in2\"", ["in1", "out"], false, true, true),
            (pa, 0.4, 0.0, 0.14, "\"in2\"", ["in1", "in2"], true, true, true),
            (pa, 0.4, 0.0, 0.14, "\"in3\"", ["in1", "in2"], false, true, false),
            (pa, 0.3, 0.3, 0.5, "\"in2\"", ["in1", "out"], false, true, true),
            (sw, 0.4, 0.0, 0.14, "\"in2\"", ["in1", "in2"], false, false, false),
            (sw, 0.0, 0.2, 0.14, "\"in2\"", ["in1", "in2"], false, true, false),
            (sw, 0.4, 0.13, 0.14, "\"in2\"", ["in1", "in2"], false, false, false),
            (sw, 0.45, 0.13, 0.14, "\"in2\"", ["in1", "in2"], false, true, false),
            (sw, 0.4, 0.0, 0.14, "", ["in1", "in2"], false, true, false),
        ];
        for (kind, i1, i2, lo, in2, bound, stray, fault, disclosed) in cases {
            let in1 = if in2.is_empty() { "" } else { "\"in1\"" };
            let rows = [row(0.5, in1), row(lo, in2), row(0.55, "")].concat();
            let text = format!("[[models]]\nid = \"mux\"\nkind = \"passive\"\n[models.pins]\n1 = \"in1\"\n2 = \"in2\"\n3 = \"out\"\n[models.ratings]\nmax_current_a = {min}\nmax_pin_current_a = {min}\n[models.source]\ntier = \"datasheet-derived\"\n{rows}", min = lo.min(0.5));
            let db: hauksbee_models::schema::DbFile = toml::from_str(&text).expect("a card");
            let entry = &db.models[0];
            let clean = hauksbee_models::derating::tag_problems(entry).is_empty();
            assert_eq!(clean, in2 != "\"in3\"", "{in2}");
            let mut circuit = Circuit::new();
            let (n1, n2) = (circuit.node("IN1"), circuit.node("IN2"));
            let unit = |n| Device::resistor("U1_e", n, NodeId::GROUND, 1.0);
            let units = [circuit.add(unit(n1)), circuit.add(unit(n2))];
            if stray {
                circuit.add(Device::resistor("Rq_U1", n1, NodeId::GROUND, 1e6));
            }
            let nodes = [("in1", n1), ("in2", n2), ("out", NodeId::GROUND)];
            let roles = nodes.into_iter().filter(|(r, _)| bound.contains(r));
            let roles: HashMap<_, _> = roles.map(|(r, n)| (r.to_owned(), n)).collect();
            let mut derating = Derating::of(entry);
            derating.carriers = rated_paths(&circuit, "U1", &derating, &roles, &units);
            let metas = units.map(|device| DeviceMeta {
                device,
                derating: derating.clone(),
                ..meta("U1_e0", kind, "", entry.ratings.clone())
            });
            let mut monitor = StressMonitor::new(metas.to_vec());
            let volts = HashMap::from([(n1, i1), (n2, i2)]);
            let node_v = |n| volts.get(&n).copied().unwrap_or(0.0);
            let mut chunk = |k| monitor.evaluate(&mut circuit, &node_v, &|_| None, f64::from(k));
            let raised: Vec<_> = (0..6).flat_map(&mut chunk).map(|f| f.kind).collect();
            let paths = |a: &Assumption| a.id().as_str().contains("U1-rating-paths");
            let said = monitor.disclosures().iter().any(paths);
            let case = format!("{kind:?}, {i1} A, {i2} A, {lo} A on {in2}, {bound:?}, {stray}");
            let over = [FaultKind::Overcurrent, FaultKind::PinOvercurrent][usize::from(kind == sw)];
            assert_eq!((raised.contains(&over), said), (fault, disclosed), "{case}");
        }
    }

    /// Duplicate metas use the first live device sample for path sums; an
    /// absent or destroyed carrier contributes nothing even when its meta exists.
    #[test]
    fn path_carriers_use_first_live_meta_and_skip_missing_devices() {
        let mut circuit = Circuit::new();
        let node = circuit.node("A");
        let first = circuit.add(Device::resistor("R1", node, NodeId::GROUND, 1.0));
        let second = circuit.add(Device::resistor("R2", node, NodeId::GROUND, 2.0));
        let pins = vec!["a".to_owned()];
        let mut derating = Derating {
            power: Curve(vec![(
                (TempRef::Ambient, None, None, pins.clone()),
                vec![(25.0, 1.8)],
            )]),
            ..Default::default()
        };
        derating.carriers = vec![(pins, vec![(first, 1), (second, 1), (DeviceId(999), 1)])];
        let mut monitor = StressMonitor::new(
            [("R1", first), ("R1_copy", first), ("R2", second)]
                .map(|(reference, device)| DeviceMeta {
                    device,
                    derating: derating.clone(),
                    ..meta(
                        reference,
                        ComponentKind::Passive,
                        "",
                        Ratings {
                            max_junction_temp_c: Some(1000.0),
                            theta_ja_c_per_w: Some(1.0),
                            ..Default::default()
                        },
                    )
                })
                .to_vec(),
        );
        let node_v = |n| if n == node { 1.0 } else { 0.0 };
        let mut run = |monitor: &mut StressMonitor| {
            let mut faults = Vec::new();
            for _ in 0..SUSTAIN_CHUNKS {
                monitor.deposit_chunk_energy(&[1.0, 3.0, 0.5], 1.0);
                faults.extend(monitor.evaluate(&mut circuit, &node_v, &|_| None, 0.0));
            }
            faults
        };
        assert!(
            run(&mut monitor).is_empty(),
            "first live: 1 W + 0.5 W < 1.8 W"
        );
        monitor.tracks[0].destroyed = true;
        assert!(
            run(&mut monitor)
                .iter()
                .any(|f| f.kind == FaultKind::Overpower),
            "next live duplicate carries 3 W"
        );
        monitor.tracks[1].destroyed = true;
        monitor.tracks[2] = DeviceTrack::default(); // force a fresh verdict, not a latched pass
        assert!(
            run(&mut monitor).is_empty(),
            "destroyed/missing carriers count zero"
        );
    }

    /// A BJT's path rows are each judged on their own terminal's current: at
    /// unity beta IB is IC and IE twice either. A base row over IB passes where
    /// the emitter's IC + IB would not, an emitter row under IE trips, and with
    /// the roles unbound the heaviest terminal, IE, meets the least favourable
    /// row.
    #[test]
    fn a_bjt_path_is_judged_on_its_own_terminal() {
        let mut circuit = Circuit::new();
        let [col, base] = ["C", "B"].map(|n| circuit.node(n));
        let model = hauksbee_ir::BjtModel {
            bf: 1.0,
            ..Default::default()
        };
        let (name, emitter) = ("Q1".to_owned(), NodeId::GROUND);
        let bjt = circuit.add(Device::Bjt {
            name,
            c: col,
            b: base,
            e: emitter,
            model,
        });
        let volts = HashMap::from([(col, 5.0), (base, 0.7)]);
        let node_v = |n| volts.get(&n).copied().unwrap_or(0.0);
        let unit = DeviceMeta {
            device: bjt,
            ..meta("Q1", ComponentKind::BjtNpn, "", Ratings::default())
        };
        let amps = operating_point(&circuit, &unit, &node_v, &|_| None).current_a;
        let roles = [("c", col), ("b", base), ("e", emitter)].map(|(r, n)| (r.to_owned(), n));
        let tags = |p: &str| (TempRef::Ambient, None, None, vec![p.to_owned()]);
        let row = |p, a| (tags(p), vec![(25.0, a * amps)]);
        // (IB row, IE row, both in IC, roles bound, trips)
        #[rustfmt::skip]
        let cases = [(1.2, 2.4, true, false), (1.2, 1.8, true, true), (20.0, 1.6, false, true)];
        for (ib_row, ie_row, bound, trips) in cases {
            let current = Curve(vec![row("b", ib_row), row("e", ie_row)]);
            let mut derating = Derating {
                current,
                ..Default::default()
            };
            let roles: HashMap<_, _> = roles.iter().filter(|_| bound).cloned().collect();
            derating.carriers = rated_paths(&circuit, "Q1", &derating, &roles, &[bjt]);
            let mut monitor = StressMonitor::new(vec![DeviceMeta {
                derating,
                ..unit.clone()
            }]);
            let mut chunk = |k| monitor.evaluate(&mut circuit, &node_v, &|_| None, f64::from(k));
            let tripped = (0..6)
                .flat_map(&mut chunk)
                .any(|f| f.kind == FaultKind::Overcurrent);
            assert_eq!(
                tripped, trips,
                "IB row {ib_row}, IE row {ie_row}, bound {bound}"
            );
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
        let checks = build_checks(&meta, &op, op.current_a, meta.power_rating_w());
        let oc = checks
            .iter()
            .find(|c| c.kind == FaultKind::Overcurrent && !c.surge)
            .expect("passive continuous over-current check present");
        assert!((oc.limit - 2.0).abs() < 1e-12);
        assert!((oc.value - 3.0).abs() < 1e-12);
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
            let raised = (0u32..).zip(amps.iter()).any(|(k, i)| {
                monitor.deposit_chunk_energy(&[0.0], dt);
                let v = *i;
                monitor
                    .evaluate(
                        &mut circuit,
                        &|n| if n == node { v } else { 0.0 },
                        &|_| None,
                        f64::from(k) * dt,
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
            let named = |a: &Assumption| {
                a.because().contains("200 C/W inferred from its package")
                    && a.replacement().contains("theta_ja_c_per_w to R1's card")
            };
            assert!(out.iter().all(named), "{out:?}");
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
        assert!((raised[0].limit - 6.0).abs() < 1e-12);
        assert!((raised[0].value - 100.0).abs() < 1e-12);

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
    fn bjt_currents_read_the_junction_behind_its_ohmic_resistances() {
        // A BC847-class card (rb 10, rc 1, re 0.5) carrying a real base
        // current: build the terminal voltages from a known internal state,
        // then ask the monitor to recover it. Reading the terminals as the
        // junction would clamp at 1 kA.
        let model = hauksbee_ir::BjtModel {
            is: 1e-14,
            bf: 150.0,
            br: 4.0,
            rb: 10.0,
            rc: 1.0,
            re: 0.5,
            ..hauksbee_ir::BjtModel::default()
        };
        let t = 27.0;
        let vt = hauksbee_ir::thermal_voltage_c(t);
        let is = model.is_at(t);
        let (v_fwd, v_rev) = (0.85, 0.80);
        let cf = is * (v_fwd / (model.nf * vt)).exp_m1();
        let cr = is * (v_rev / (model.nr * vt)).exp_m1();
        let ic = (cf - cr) - cr / model.br;
        let ib = cf / model.bf + cr / model.br;
        let vbe = (ic + ib).mul_add(model.re, ib.mul_add(model.rb, v_fwd));
        let vbc = ic.mul_add(-model.rc, ib.mul_add(model.rb, v_rev));
        assert!(vbe > 1.0, "terminal V_BE well above the junction: {vbe}");
        let (coll, base) = bjt_terminal_currents(&model, vbe, vbc, vt, t);
        assert!((base - ib).abs() <= ib.abs() * 1e-6, "ib {base} vs {ib}");
        assert!(
            (coll - ic).abs() <= ic.abs().max(1e-6) * 1e-6,
            "ic {coll} vs {ic}"
        );
        // Without series resistance the terminal IS the junction.
        let ideal = hauksbee_ir::BjtModel {
            rb: 0.0,
            rc: 0.0,
            re: 0.0,
            ..model
        };
        let (_, ib0) = bjt_terminal_currents(&ideal, 0.6, -2.0, vt, t);
        let want = is * (0.6 / vt).exp_m1() / ideal.bf;
        assert!((ib0 - want).abs() <= want * 1e-3, "{ib0} vs {want}");
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
        let expected = model.is_at(125.0) * (vd / vt_hot).exp_m1();
        assert!(
            (i_hot - expected).abs() <= expected.abs() * 1e-9,
            "diode_current must use is_at(T): got {i_hot:e}, expected {expected:e}"
        );
        // A nominal-Is result would be materially smaller, guard the gap.
        let nominal = model.is * (vd / vt_hot).exp_m1();
        assert!(
            i_hot > nominal * 1.5,
            "temp-corrected current must exceed the nominal-Is current it replaced"
        );
    }

    /// One chunk of heat must not read as a steady state: the junction heats on
    /// the trailing `TJ_AVG_S` average, so a 0.1 ms spike in an idle millisecond
    /// is a tenth of its power, and only a sustained load reaches the full rise.
    #[test]
    fn junction_heats_on_the_trailing_average_not_one_chunk() {
        let ratings = Ratings {
            theta_ja_c_per_w: Some(100.0),
            ..Default::default()
        };
        let (mut circuit, node, mut monitor) = resistor_monitor(100.0, ratings);
        let mut chunk = |k: u32, volts: f64| {
            let node_v = |n| if n == node { volts } else { 0.0 };
            monitor.accumulate_step(&circuit, &node_v, &|_| None, 1e-4);
            monitor.evaluate(&mut circuit, &node_v, &|_| None, f64::from(k) * 1e-4);
            monitor.temp_by_ref().get("R1").copied()
        };
        (0..9).for_each(|k| _ = chunk(k, 0.0));
        // 0.25 W for one chunk in ten: 25 C + 0.025 W * 100 C/W.
        let spike = chunk(9, 5.0);
        assert!(
            spike.is_some_and(|t| (t - 27.5).abs() < 1e-6),
            "one-chunk spike read {spike:?}"
        );
        let sustained = (10..20).map(|k| chunk(k, 5.0)).last().flatten();
        assert!(
            sustained.is_some_and(|t| (t - 50.0).abs() < 1e-6),
            "sustained read {sustained:?}"
        );
    }

    /// The monitored operating point of N-FET `model`, source grounded, gate
    /// at `vg` and drain at `vd`.
    fn nmos_op(model: hauksbee_ir::MosfetModel, vg: f64, vd: f64) -> OperatingPoint {
        let mut circuit = Circuit::new();
        let (d, g) = (circuit.node("D"), circuit.node("G"));
        let device = circuit.add(Device::Mosfet {
            name: "M1".into(),
            d,
            g,
            s: NodeId::GROUND,
            b: None,
            model,
        });
        let meta = DeviceMeta {
            device,
            ..meta("Q1", ComponentKind::Nmos, "", Ratings::default())
        };
        let node_v = |n| {
            [(d, vd), (g, vg)]
                .into_iter()
                .find(|p| p.0 == n)
                .map_or(0.0, |p| p.1)
        };
        operating_point(&circuit, &meta, &node_v, &|_| None)
    }

    /// The solver puts the channel behind rd/rs; reading it at the terminals
    /// hands a hard-on FET the current of a bare channel across the full vds.
    #[test]
    fn mosfet_power_is_read_behind_its_series_resistance() {
        let model = hauksbee_ir::MosfetModel {
            vto: 1.0,
            kp: 100.0,
            rd: 0.5,
            rs: 0.5,
            ..Default::default()
        };
        // A near-short channel behind 1 ohm carries just under 1 A at 1 V.
        let op = nmos_op(model, 10.0, 1.0);
        let (i, w) = (op.current_a, op.power_w);
        assert!((0.95..1.0).contains(&i), "{i} A");
        assert!((w - i).abs() < 1e-9, "{w} W at {i} A");
    }

    /// The monitor reads the solver's channel, not a copy: equal currents
    /// through threshold, where the old softplus copy read up to 1.73x high.
    #[test]
    fn mosfet_current_is_the_solvers_channel_near_threshold() {
        let model = hauksbee_ir::MosfetModel {
            vto: 1.0,
            kp: 0.1,
            lambda: 0.02,
            ..Default::default()
        };
        let nvt = model.n_sub.max(1.0) * hauksbee_ir::thermal_voltage_c(Circuit::new().temp_c);
        for vg in [0.8, 1.0, 1.05, 1.09, 2.0] {
            for vd in [0.01, 0.3, 5.0] {
                let got = nmos_op(model, vg, vd).current_a;
                let want = mos_channel(model.beta(), nvt, vg - 1.0, vd, model.lambda, 0.0).0;
                assert!((got / want - 1.0).abs() < 1e-12, "{vg} {vd}: {got} {want}");
            }
        }
    }

    /// A regulator's monitored device is its ideal output source, so its heat
    /// comes from the input it draws: (Vin - Vout) * Iout for a linear pass
    /// element, its in-circuit input draw minus output power for a converter.
    #[test]
    fn regulator_dissipation_is_estimated_from_its_input() {
        use hauksbee_ir::{BOutput::Current, SourceKind::Dc};
        let mut circuit = Circuit::new();
        let (vin, out) = (circuit.node("VIN"), circuit.node("OUT"));
        let src = circuit.add(Device::vsource("Vreg_U1", out, NodeId::GROUND, Dc(3.3)));
        let draw = Device::behavioral("Bbeh_U1_in", (vin, NodeId::GROUND), Current, "0.08", &[]);
        let draw = circuit.add(draw.unwrap());
        let node_v = |n| {
            if n == vin {
                5.0
            } else if n == out {
                3.3
            } else {
                0.0
            }
        };
        let branch = |id| (id == src).then_some(-0.1);
        let meta = DeviceMeta {
            device: src,
            ..meta("U1", ComponentKind::Vreg, "", Ratings::default())
        };
        for (heat, want) in [
            (None, 0.0),
            (Some(RegulatorHeat::Linear { input: vin }), 0.17),
            (Some(RegulatorHeat::Converter { draw }), 0.07),
        ] {
            let mut monitor = StressMonitor::new(vec![meta.clone()]);
            if let Some(heat) = heat {
                monitor.watch_regulator_heat(src, heat);
            }
            let watts = monitor.op_of(&circuit, 0, &node_v, &branch).power_w;
            assert!((watts - want).abs() < 1e-9, "{heat:?}: {watts} W");
        }
    }
}
