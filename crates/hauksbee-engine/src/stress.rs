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
        if matches!(self.kind, ComponentKind::Passive) {
            return Some(resistor_power_from_footprint(&self.footprint));
        }
        None
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

/// Derive a resistor's power rating from its footprint package size. Standard
/// chip-resistor ratings: 01005 1/32 W, 0201 1/20 W, 0402 1/16 W, 0603 1/10 W,
/// 0805 1/8 W, 1206 1/4 W; through-hole / unknown defaults to 1/4 W.
pub fn resistor_power_from_footprint(footprint: &str) -> f64 {
    let f = footprint.to_ascii_uppercase();
    // Match the imperial size token anywhere in the footprint string
    // (e.g. "Resistor_SMD:R_0402_1005Metric"). The imperial code is paired with
    // its metric code, and the metric code of a small part collides with the
    // imperial code of a larger one: imperial 0201 → metric 0603
    // ("R_0201_0603Metric"), imperial 01005 → metric 0402. So the smallest
    // packages MUST be matched first, before the larger imperial tokens they
    // embed as a metric suffix, or they are silently over-rated.
    if f.contains("01005") {
        1.0 / 32.0
    } else if f.contains("0201") {
        1.0 / 20.0
    } else if f.contains("0402") {
        1.0 / 16.0
    } else if f.contains("0603") {
        1.0 / 10.0
    } else if f.contains("0805") {
        1.0 / 8.0
    } else if f.contains("1206") {
        1.0 / 4.0
    } else if f.contains("1210") {
        1.0 / 2.0
    } else if f.contains("2010") {
        3.0 / 4.0
    } else if f.contains("2512") {
        1.0
    } else {
        // THT axial / unknown SMD: conservative 1/4 W.
        1.0 / 4.0
    }
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
            destructive: false,
            ambient_c: crate::thermal::DEFAULT_AMBIENT_C,
            stress_by_ref: HashMap::new(),
            temp_by_ref: HashMap::new(),
        }
    }

    /// Number of monitored devices.
    pub fn device_count(&self) -> usize {
        self.metas.len()
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
        self.stress_by_ref.clear();
        self.temp_by_ref.clear();
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
        let mut package_power: HashMap<String, f64> = HashMap::new();
        for (i, meta) in self.metas.iter().enumerate() {
            if let Some(op) = &ops[i] {
                if op.power_w > 0.0 {
                    *package_power
                        .entry(strip_unit_suffix(&meta.reference).to_string())
                        .or_insert(0.0) += op.power_w;
                }
            }
        }

        // ── Pass 2: per-device checks ────────────────────────────────────────
        // Iterate by index so we can borrow tracks mutably alongside metas.
        for i in 0..self.metas.len() {
            let meta = self.metas[i].clone();
            let Some(op) = &ops[i] else {
                // Destroyed devices stay at full stress and raise nothing more.
                self.stress_by_ref.insert(meta.reference.clone(), 1.0);
                continue;
            };
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
fn diode_current(model: &hauksbee_ir::DiodeModel, vd: f64, temp_c: f64) -> f64 {
    let vt = hauksbee_ir::thermal_voltage_c(temp_c) * model.n;
    if vt <= 0.0 {
        return 0.0;
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
        let w = |f: &str| resistor_power_from_footprint(f);
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
