//! Configurable power supplies (Feature 1).
//!
//! A power supply replaces the binder's ideal rail on a supply net with a
//! *behavioral source*: an [`PinDriver`](crate::drivers)-style Thevenin leg
//! whose target voltage is recomputed between solver chunks from the rail
//! current measured in the previous chunk. This mirrors the GPIO-driver
//! pattern — set a `Vsource` value, let MNA resolve the rest — but the update
//! rule encodes real supply behaviour (current limiting, output resistance,
//! ripple, battery droop and depletion).
//!
//! ## Why a Thevenin leg rather than a bare ideal `Vsource`
//!
//! To measure how much current a supply is delivering we need a branch whose
//! current the solver reports. A `Vsource` already owns a branch current in
//! MNA, so the simplest behavioral source is a `Vsource` driving the rail
//! through a (usually small) series resistor. The series resistor is also
//! exactly the physical output impedance for the [`PowerSupply::Wall`] and
//! battery cases, so it does double duty. The scheduler reads the `Vsource`'s
//! branch current each chunk and feeds it back into [`PowerSupply::update`].
//!
//! Discharge / V(SoC) curves below are piecewise-linear fits to published
//! per-cell discharge curves; each is cited inline.

use galvani_ir::{Circuit, Device, DeviceId, NodeId, SourceKind};

/// Series output resistance used for the "ideal" and current-source-limited
/// supplies — small enough to be electrically negligible (a few mΩ) but
/// non-zero so the `Vsource` branch current is well defined and so the rail
/// never becomes a hard short in MNA.
pub const STIFF_R_OHMS: f64 = 1e-3;

/// Battery / cell electrochemistry. Per-cell open-circuit voltage is a
/// piecewise-linear function of state-of-charge (SoC, 0..1) — see
/// [`Chemistry::ocv_per_cell`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chemistry {
    /// Lithium-ion / LiPo (graphite ‖ NMC/LCO), nominal 3.7 V/cell.
    LiIon,
    /// Primary alkaline (Zn‖MnO2), nominal 1.5 V/cell.
    Alkaline,
    /// Nickel–metal-hydride, nominal 1.2 V/cell.
    NiMh,
    /// Lithium iron phosphate, nominal 3.2 V/cell (very flat).
    LiFePO4,
}

impl Chemistry {
    /// Open-circuit terminal voltage of a *single cell* at the given
    /// state-of-charge (SoC ∈ [0,1]). Piecewise-linear interpolation over
    /// `(soc, volts)` knots taken from published discharge curves.
    ///
    /// Sources (typical room-temperature, low-rate discharge):
    /// - Li-ion: Panasonic NCR18650B datasheet discharge curve — 4.2 V full,
    ///   ~3.7 V nominal plateau, ~3.0 V knee, 2.5 V empty.
    /// - Alkaline: Energizer E91 AA application data — 1.6 V fresh sloping
    ///   steadily to ~0.9 V cutoff (alkalines have no plateau).
    /// - NiMH: Panasonic eneloop HR-3UTGB — 1.45 V full, flat ~1.25 V plateau,
    ///   1.0 V cutoff.
    /// - LiFePO4: A123 / generic LFP — 3.6 V surface-charge, extremely flat
    ///   ~3.3–3.2 V plateau, 2.5 V cutoff.
    pub fn ocv_per_cell(self, soc: f64) -> f64 {
        let s = soc.clamp(0.0, 1.0);
        let knots: &[(f64, f64)] = match self {
            Chemistry::LiIon => &[
                (0.00, 3.00),
                (0.05, 3.30),
                (0.15, 3.50),
                (0.30, 3.65),
                (0.60, 3.80),
                (0.85, 4.00),
                (1.00, 4.20),
            ],
            Chemistry::Alkaline => &[
                (0.00, 0.90),
                (0.10, 1.05),
                (0.30, 1.20),
                (0.60, 1.35),
                (0.85, 1.50),
                (1.00, 1.60),
            ],
            Chemistry::NiMh => &[
                (0.00, 1.00),
                (0.10, 1.15),
                (0.25, 1.22),
                (0.70, 1.25),
                (0.90, 1.30),
                (1.00, 1.45),
            ],
            Chemistry::LiFePO4 => &[
                (0.00, 2.50),
                (0.08, 3.10),
                (0.20, 3.20),
                (0.80, 3.30),
                (0.95, 3.35),
                (1.00, 3.60),
            ],
        };
        interp(knots, s)
    }
}

/// A configurable supply attached to one supply net.
#[derive(Debug, Clone)]
pub enum PowerSupply {
    /// Ideal constant-voltage rail (the default; preserves prior behaviour).
    Ideal { volts: f64 },
    /// Bench PSU in constant-voltage mode with constant-current foldback: holds
    /// `volts` until the load pulls more than `current_limit_a`, then drops the
    /// voltage to hold the current at the limit (smooth CV→CC transition).
    Bench { volts: f64, current_limit_a: f64 },
    /// Cheap wall adapter: nominal `volts` behind output resistance
    /// `r_out_ohms`, with a mains-frequency ripple of `ripple_vpp` peak-to-peak
    /// at `ripple_hz` superimposed.
    Wall {
        volts: f64,
        r_out_ohms: f64,
        ripple_vpp: f64,
        ripple_hz: f64,
    },
    /// USB source: nominal 5 V with a small per-amp droop and a hard foldback
    /// once the spec current limit is exceeded.
    Usb { spec: UsbSpec },
    /// Battery pack: `cells` in series of `chemistry`, draining a `capacity_mah`
    /// charge store from initial `soc`, behind `r_internal_ohms` per pack.
    ///
    /// `protection` is an optional BMS-style over-current cutoff. Real protected
    /// LiPo cells (and the DW01/FS312-class protection ICs on bare cells) trip a
    /// MOSFET open when the pack current stays above a threshold for a short
    /// delay. This is exactly the dynamic that DC misses and that the Inkplate
    /// WiFi cold-boot inrush hits: the pack is fine at steady state but the TX
    /// burst trips protection.
    Battery {
        chemistry: Chemistry,
        cells: u32,
        capacity_mah: f64,
        soc: f64,
        r_internal_ohms: f64,
        /// Optional over-current protection (BMS). `None` = unprotected pack.
        protection: Option<BatteryProtection>,
    },
}

/// A simple BMS over-current cutoff, modelling a protection IC + MOSFET.
///
/// When the pack current exceeds `trip_a` continuously for `delay_s`, the
/// protection latches open: the pack commands ~0 V (the MOSFET is off) until the
/// load drops below `reset_a` for `delay_s`, at which point it re-arms. Real
/// protection ICs (e.g. Seiko S-8261, Fortune DW01) work this way: an
/// over-current comparator with a fixed delay and an auto-recover on load
/// removal. Numbers are per-spec, supplied by the scenario.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BatteryProtection {
    /// Over-current trip threshold (A).
    pub trip_a: f64,
    /// Sustained time above `trip_a` before the cutoff latches (s).
    pub delay_s: f64,
    /// Current the load must drop below to begin re-arming (A). Defaults to
    /// `trip_a` when not given.
    pub reset_a: f64,
    /// Runtime latch state: true once tripped (open). Starts armed (false).
    #[serde(default)]
    pub tripped: bool,
    /// Internal: accumulated time the over/under-current condition has held (s).
    #[serde(default)]
    pub accum_s: f64,
}

impl BatteryProtection {
    /// New protection with a trip threshold and delay; reset threshold = trip.
    pub fn new(trip_a: f64, delay_s: f64) -> Self {
        BatteryProtection {
            trip_a,
            delay_s,
            reset_a: trip_a,
            tripped: false,
            accum_s: 0.0,
        }
    }

    /// Advance the protection state machine by `dt` seconds given the measured
    /// pack current `i_a`. Returns `true` if the cutoff is open (no output).
    fn update(&mut self, i_a: f64, dt: f64) -> bool {
        if !self.tripped {
            // Armed: integrate time spent above the trip threshold.
            if i_a > self.trip_a {
                self.accum_s += dt;
                if self.accum_s >= self.delay_s {
                    self.tripped = true;
                    self.accum_s = 0.0;
                }
            } else {
                // Drop the accumulator; brief spikes below the delay don't trip.
                self.accum_s = 0.0;
            }
        } else {
            // Tripped: integrate time spent below the reset threshold to re-arm.
            if i_a < self.reset_a {
                self.accum_s += dt;
                if self.accum_s >= self.delay_s {
                    self.tripped = false;
                    self.accum_s = 0.0;
                }
            } else {
                self.accum_s = 0.0;
            }
        }
        self.tripped
    }
}

/// USB power-profile current limits and nominal droop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsbSpec {
    /// USB 2.0 / BC1.2 SDP: 5 V, 0.5 A.
    V5_0_5A,
    /// USB BC1.2 CDP / typical charging port: 5 V, 1.5 A.
    V5_1_5A,
    /// USB-C default / DCP: 5 V, 3.0 A.
    V5_3A,
}

impl UsbSpec {
    /// Spec current limit (A).
    pub fn current_limit_a(self) -> f64 {
        match self {
            UsbSpec::V5_0_5A => 0.5,
            UsbSpec::V5_1_5A => 1.5,
            UsbSpec::V5_3A => 3.0,
        }
    }

    /// Effective cable + connector droop resistance (ohms). A real USB cable
    /// drops ~0.25 V at the rated current; we back that out into an ohmic droop.
    pub fn droop_ohms(self) -> f64 {
        // ~0.25 V at full rated current.
        0.25 / self.current_limit_a()
    }
}

impl Default for PowerSupply {
    fn default() -> Self {
        PowerSupply::Ideal { volts: 5.0 }
    }
}

impl PowerSupply {
    /// Nominal open-circuit voltage (no load), for seeding the first chunk and
    /// for diagnostics.
    pub fn nominal_volts(&self) -> f64 {
        match self {
            PowerSupply::Ideal { volts }
            | PowerSupply::Bench { volts, .. }
            | PowerSupply::Wall { volts, .. } => *volts,
            PowerSupply::Usb { .. } => 5.0,
            PowerSupply::Battery {
                chemistry,
                cells,
                soc,
                ..
            } => chemistry.ocv_per_cell(*soc) * (*cells as f64),
        }
    }

    /// The series output resistance to stamp for this supply's Thevenin leg.
    pub fn series_r(&self) -> f64 {
        match self {
            PowerSupply::Ideal { .. } | PowerSupply::Bench { .. } => STIFF_R_OHMS,
            PowerSupply::Wall { r_out_ohms, .. } => r_out_ohms.max(STIFF_R_OHMS),
            PowerSupply::Usb { spec } => spec.droop_ohms().max(STIFF_R_OHMS),
            PowerSupply::Battery {
                r_internal_ohms, ..
            } => r_internal_ohms.max(STIFF_R_OHMS),
        }
    }

    /// State-of-charge readout (0..1), 1.0 for non-depleting supplies.
    pub fn soc(&self) -> f64 {
        match self {
            PowerSupply::Battery { soc, .. } => *soc,
            _ => 1.0,
        }
    }

    /// Whether this supply's protection has latched open. False for supplies
    /// without protection (or unprotected batteries).
    pub fn protection_tripped(&self) -> bool {
        match self {
            PowerSupply::Battery {
                protection: Some(p),
                ..
            } => p.tripped,
            _ => false,
        }
    }

    /// Compute the `Vsource` target voltage for the *next* chunk, given the
    /// rail current `i_a` (A, positive = sourced into the net) measured over
    /// the chunk just solved, the simulation time `t` (s, for ripple phase),
    /// and the chunk length `dt` (s, for battery SoC integration). May mutate
    /// internal state (battery SoC drains here).
    ///
    /// The returned voltage is the ideal source value *behind* `series_r()`;
    /// the solver adds the I·R drop across the series resistor on top, so the
    /// terminal voltage the load sees is `v_internal - i*series_r`.
    pub fn update(&mut self, i_a: f64, last_cmd_v: f64, t: f64, dt: f64) -> f64 {
        let i = i_a.max(0.0);
        match self {
            PowerSupply::Ideal { volts } => *volts,

            // CV with constant-current foldback. Above the limit, command a
            // lower internal voltage so that I·R_load ≈ limit. We do not know
            // R_load directly, but holding the *commanded* voltage proportional
            // to (limit / i) regulates the current down toward the limit over a
            // few chunks (a discrete CC loop). Smoothed to avoid oscillation.
            PowerSupply::Bench {
                volts,
                current_limit_a,
            } => cc_regulate(*volts, *current_limit_a, last_cmd_v, i),

            // Nominal minus the ripple sine. The I·R_out drop is applied by the
            // solver through the series resistor, so we only add ripple here.
            PowerSupply::Wall {
                volts,
                ripple_vpp,
                ripple_hz,
                ..
            } => {
                let ripple = 0.5 * *ripple_vpp * (std::f64::consts::TAU * *ripple_hz * t).sin();
                *volts + ripple
            }

            // 5 V nominal; the connector/cable droop is the series resistor.
            // Past the spec limit, fold the commanded voltage back hard so the
            // current cannot run away (models the port's current-limit trip).
            PowerSupply::Usb { spec } => cc_regulate(5.0, spec.current_limit_a(), last_cmd_v, i),

            // Drain charge and recompute the open-circuit stack voltage. The
            // internal resistance drop is the series resistor (solver-applied).
            // An optional BMS protection cutoff can latch the output to ~0 V when
            // the pack current trips it (the inrush-vs-protection interaction).
            PowerSupply::Battery {
                chemistry,
                cells,
                capacity_mah,
                soc,
                protection,
                ..
            } => {
                // Integrate charge out: ΔAh = I·dt / 3600; ΔSoC = ΔAh / cap_Ah.
                let cap_ah = (*capacity_mah / 1000.0).max(1e-9);
                let d_soc = (i * dt / 3600.0) / cap_ah;
                *soc = (*soc - d_soc).clamp(0.0, 1.0);
                let ocv = chemistry.ocv_per_cell(*soc) * (*cells as f64);
                if let Some(prot) = protection {
                    // Advance the cutoff state machine on the measured current.
                    if prot.update(i, dt) {
                        // Open: command ~0 V so the rail collapses behind the
                        // series resistor (the protection MOSFET is off).
                        return 0.0;
                    }
                }
                ocv
            }
        }
    }

    /// Short human-readable kind label for diagnostics / protocol.
    pub fn kind_label(&self) -> &'static str {
        match self {
            PowerSupply::Ideal { .. } => "ideal",
            PowerSupply::Bench { .. } => "bench",
            PowerSupply::Wall { .. } => "wall",
            PowerSupply::Usb { .. } => "usb",
            PowerSupply::Battery { .. } => "battery",
        }
    }
}

/// Constant-voltage / constant-current regulation. The pair
/// (last commanded voltage, measured current) estimates the total load
/// resistance seen by the source, so the voltage that delivers exactly
/// `limit` is `limit * v_cmd / i`. CV applies whenever that exceeds the
/// setpoint. Anchoring to the *previous command* (not the setpoint) makes the
/// loop convergent for resistive loads in one chunk and free of the
/// bang-bang oscillation a setpoint-anchored law produces.
fn cc_regulate(v_set: f64, limit_a: f64, last_cmd_v: f64, i_a: f64) -> f64 {
    if limit_a <= 0.0 || i_a <= 1e-12 {
        return v_set;
    }
    let v_cc = limit_a * (last_cmd_v.max(1e-6) / i_a);
    v_set.min(v_cc).max(0.0)
}

/// A supply stamped onto a circuit: the controllable `Vsource`, its series
/// resistor, the net it drives, and the live model.
#[derive(Debug, Clone)]
pub struct SupplyLeg {
    /// Supply net name (e.g. "+5V").
    pub net_name: String,
    /// Net node the supply pushes onto.
    pub net: NodeId,
    /// The controllable internal `Vsource`.
    pub vsource: DeviceId,
    /// The series output resistor (output impedance).
    pub resistor: DeviceId,
    /// The behavioral model.
    pub supply: PowerSupply,
    /// Last measured rail current (A, sourced into the net).
    pub last_current_a: f64,
    /// Voltage commanded onto the internal source last chunk.
    last_cmd_v: f64,
}

impl SupplyLeg {
    /// Stamp a behavioral supply onto `net` and return its handle. Mirrors the
    /// `Vrail_*` ideal source the binder would otherwise add, but routes through
    /// a measurable series resistor so the scheduler can read rail current.
    pub fn stamp(circuit: &mut Circuit, net: NodeId, net_name: &str, supply: PowerSupply) -> Self {
        let drv_node = circuit.node(&format!("__supply_{net_name}"));
        let v0 = supply.nominal_volts();
        let vsource = circuit.add(Device::Vsource {
            name: format!("Vsupply_{net_name}"),
            p: drv_node,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(v0),
        });
        let resistor = circuit.add(Device::Resistor {
            name: format!("Rsupply_{net_name}"),
            a: drv_node,
            b: net,
            ohms: supply.series_r(),
            tc1: None,
        });
        SupplyLeg {
            net_name: net_name.to_string(),
            net,
            vsource,
            resistor,
            supply,
            last_current_a: 0.0,
            last_cmd_v: v0,
        }
    }

    /// Push a new behavioral model onto an already-stamped leg, retuning both
    /// the source voltage and the series resistance to match.
    pub fn reconfigure(&mut self, circuit: &mut Circuit, supply: PowerSupply) {
        self.supply = supply;
        if let Some(Device::Resistor { ohms, .. }) =
            circuit.devices.get_mut(self.resistor.0 as usize)
        {
            *ohms = self.supply.series_r();
        }
        let v0 = self.supply.nominal_volts();
        self.last_cmd_v = v0;
        self.set_internal_volts(circuit, v0);
    }

    /// Update the leg from the rail current measured this chunk: recompute the
    /// supply's internal voltage and write it onto the `Vsource`.
    pub fn update(&mut self, circuit: &mut Circuit, i_a: f64, t: f64, dt: f64) {
        self.last_current_a = i_a;
        let v = self.supply.update(i_a, self.last_cmd_v, t, dt);
        self.last_cmd_v = v;
        self.set_internal_volts(circuit, v);
    }

    fn set_internal_volts(&self, circuit: &mut Circuit, v: f64) {
        if let Some(Device::Vsource { kind, .. }) =
            circuit.devices.get_mut(self.vsource.0 as usize)
        {
            *kind = SourceKind::Dc(v);
        }
    }
}

/// Piecewise-linear interpolation of `(x, y)` knots (assumed x-ascending) at
/// `x`. Clamps to the endpoints outside the range.
fn interp(knots: &[(f64, f64)], x: f64) -> f64 {
    if knots.is_empty() {
        return 0.0;
    }
    if x <= knots[0].0 {
        return knots[0].1;
    }
    if x >= knots[knots.len() - 1].0 {
        return knots[knots.len() - 1].1;
    }
    for w in knots.windows(2) {
        let (x0, y0) = w[0];
        let (x1, y1) = w[1];
        if x >= x0 && x <= x1 {
            let f = if (x1 - x0).abs() < 1e-12 {
                0.0
            } else {
                (x - x0) / (x1 - x0)
            };
            return y0 + f * (y1 - y0);
        }
    }
    knots[knots.len() - 1].1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ocv_curves_are_monotonic_and_bounded() {
        for chem in [
            Chemistry::LiIon,
            Chemistry::Alkaline,
            Chemistry::NiMh,
            Chemistry::LiFePO4,
        ] {
            let full = chem.ocv_per_cell(1.0);
            let empty = chem.ocv_per_cell(0.0);
            assert!(full > empty, "{chem:?} full {full} should exceed empty {empty}");
            // Monotonic non-decreasing in SoC.
            let mut prev = empty;
            for k in 1..=10 {
                let v = chem.ocv_per_cell(k as f64 / 10.0);
                assert!(v + 1e-9 >= prev, "{chem:?} non-monotonic at soc={k}/10");
                prev = v;
            }
        }
    }

    #[test]
    fn battery_drains_with_load() {
        let mut s = PowerSupply::Battery {
            chemistry: Chemistry::LiIon,
            cells: 1,
            capacity_mah: 100.0, // 0.1 Ah
            soc: 1.0,
            r_internal_ohms: 0.1,
            protection: None,
        };
        // Draw 1 A for 36 s = 0.01 Ah = 10% of 0.1 Ah.
        let dt = 0.1;
        for _ in 0..360 {
            s.update(1.0, 4.2, 0.0, dt);
        }
        // 360 * 0.1 s = 36 s at 1 A → 0.01 Ah drained → SoC ≈ 0.90.
        assert!((s.soc() - 0.90).abs() < 0.02, "soc {} not ~0.90", s.soc());
    }

    #[test]
    fn protection_trips_on_sustained_overcurrent_and_holds_under_brief_spike() {
        // 1 A trip, 5 ms delay. A 2 ms spike must NOT trip; a sustained 10 ms
        // over-current MUST trip and collapse the output to 0 V.
        let mut prot = BatteryProtection::new(1.0, 5e-3);
        let dt = 1e-3;

        // 2 ms over-current: below the delay, no trip.
        for _ in 0..2 {
            assert!(!prot.update(2.0, dt), "tripped too early on a brief spike");
        }
        // Back below threshold: accumulator resets.
        prot.update(0.2, dt);
        assert!(!prot.tripped);

        // Sustained over-current: trips once the 5 ms delay elapses.
        let mut tripped = false;
        for _ in 0..10 {
            tripped = prot.update(2.0, dt);
        }
        assert!(tripped, "sustained over-current did not trip protection");

        // With protection latched, the battery commands 0 V regardless of OCV.
        let mut s = PowerSupply::Battery {
            chemistry: Chemistry::LiIon,
            cells: 1,
            capacity_mah: 1000.0,
            soc: 1.0,
            r_internal_ohms: 0.1,
            protection: Some(prot),
        };
        // Keep drawing over the trip current: output stays at 0 V (open).
        let v = s.update(2.0, 4.2, 0.0, dt);
        assert_eq!(v, 0.0, "tripped pack should command 0 V, got {v}");
        assert!(s.protection_tripped());

        // Remove the load (below reset) for longer than the delay: re-arms.
        for _ in 0..10 {
            s.update(0.0, 0.0, 0.0, dt);
        }
        assert!(!s.protection_tripped(), "protection should re-arm on load removal");
    }
}
