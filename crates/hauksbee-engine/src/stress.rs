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
/// chip-resistor ratings: 0402 1/16 W, 0603 1/10 W, 0805 1/8 W, 1206 1/4 W;
/// through-hole / unknown defaults to 1/4 W.
pub fn resistor_power_from_footprint(footprint: &str) -> f64 {
    let f = footprint.to_ascii_uppercase();
    // Match the imperial size token anywhere in the footprint string
    // (e.g. "Resistor_SMD:R_0402_1005Metric").
    if f.contains("0402") {
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
        // Iterate by index so we can borrow tracks mutably alongside metas.
        for i in 0..self.metas.len() {
            let meta = self.metas[i].clone();
            if self.tracks[i].destroyed {
                // Destroyed devices stay at full stress and raise nothing more.
                self.stress_by_ref.insert(meta.reference.clone(), 1.0);
                continue;
            }
            let op = operating_point(circuit, &meta, node_v, branch_current);
            let mut checks = build_checks(&meta, &op);

            // Thermal: turn this chunk's dissipation into a steady-state junction
            // temperature and check it against the device's max Tj. Applies to
            // any device that dissipates (op.power_w > 0). Treated as a
            // continuous rating so a switching-edge power spike does not trip it.
            if op.power_w > 0.0 {
                let tj = crate::thermal::junction_temp_c(
                    self.ambient_c,
                    op.power_w,
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
                let frac = if chk.limit > 0.0 {
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
                        let destroyed = self.maybe_destroy(circuit, &meta);
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
                    let destroyed = self.maybe_destroy(circuit, &meta);
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
    /// whether the device was destroyed.
    fn maybe_destroy(&self, circuit: &mut Circuit, meta: &DeviceMeta) -> bool {
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
            // Diode / LED: over-current burns the junction open. Replace the
            // diode with a tiny open-circuit resistor across its nodes so the
            // device count / layout is unchanged but it no longer conducts.
            Device::Diode { name, a, k, .. } => {
                let (name, a, k) = (name.clone(), *a, *k);
                *dev = Device::Resistor {
                    name,
                    a,
                    b: k,
                    ohms: 1e12,
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
            // An ideal capacitor's through-current is displacement current —
            // it needs dv/dt across chunks, not one voltage sample — and it
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
            // (like a Vsource's), not in a node-voltage difference — without
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
            // folded — the same equations the solver stamps, so the monitor
            // sees the operating point the solve actually settled at.
            let sign = match model.polarity {
                hauksbee_ir::Polarity::N => 1.0,
                hauksbee_ir::Polarity::P => -1.0,
            };
            let vt = hauksbee_ir::thermal_voltage_c(circuit.temp_c);
            let vbe = sign * (node_v(*b) - node_v(*e));
            let vbc = sign * (node_v(*b) - node_v(*c));
            let ex = |v: f64, n: f64| ((v / (n * vt)).clamp(-100.0, 200.0)).exp();
            let cf = model.is * (ex(vbe, model.nf) - 1.0);
            let cr = model.is * (ex(vbc, model.nr) - 1.0);
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
            // Shichman-Hodges level-1 channel at the sampled node voltages —
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
            // the slightly *higher* current — conservative for a limit check.
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
            // branch unknown. Voltage and power stay zero ON PURPOSE — this
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

    // Surge current (instantaneous ceiling) — for any device with a surge spec.
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
    let i = model.is * (exp_arg.exp() - 1.0);
    i.clamp(-1e3, 1e3)
}
