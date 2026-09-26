//! Configurable power supplies.
//!
//! A power supply replaces the binder's ideal rail on a supply net with a
//! *behavioral source*: an [`PinDriver`](crate::drivers)-style Thevenin leg
//! whose target voltage is recomputed between solver chunks from the rail
//! current measured in the previous chunk. This mirrors the GPIO-driver
//! pattern, set a `Vsource` value, let MNA resolve the rest, but the update
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
//! ## A current limit folds inside the solve
//!
//! A bench or USB limit is NOT a per-chunk rule: one end-of-chunk current
//! sample into bulk capacitance is inrush, so a fold driven by it bangs the
//! source between setpoint and 0 V every chunk, moves with the chunk size and
//! opens each chunk with a volt step onto milliohms. Instead the leg carries a
//! current-controlled drop in series with its source ([`SupplyLeg::stamp`]),
//! so Newton resolves the CV/CC operating point at every timestep.
//!
//! Discharge / V(SoC) curves below are piecewise-linear fits to published
//! per-cell discharge curves; each is cited inline.

use hauksbee_ir::{BDep, BOutput, Circuit, CompiledExpr, Device, DeviceId, NodeId, SourceKind};

/// Series output resistance used for the "ideal" and current-source-limited supplies.
///
/// Small enough to be electrically negligible (a few mΩ) but
/// non-zero so the `Vsource` branch current is well defined and so the rail
/// never becomes a hard short in MNA.
pub const STIFF_R_OHMS: f64 = 1e-3;

/// Battery / cell electrochemistry. Per-cell open-circuit voltage is a
/// piecewise-linear function of state-of-charge (`SoC`, 0..1), see
/// [`Chemistry::ocv_per_cell`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chemistry {
    /// Lithium-ion / `LiPo` (graphite ‖ NMC/LCO), nominal 3.7 V/cell.
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
    /// state-of-charge (`SoC` in `[0, 1]`). Piecewise-linear interpolation over
    /// `(soc, volts)` knots taken from published discharge curves.
    ///
    /// Sources (typical room-temperature, low-rate discharge):
    /// - Li-ion: Panasonic NCR18650B datasheet discharge curve, 4.2 V full,
    ///   ~3.7 V nominal plateau, ~3.0 V knee, 2.5 V empty.
    /// - Alkaline: Energizer E91 AA application data, 1.6 V fresh sloping
    ///   steadily to ~0.9 V cutoff (alkalines have no plateau).
    /// - `NiMH`: Panasonic eneloop HR-3UTGB, 1.45 V full, flat ~1.25 V plateau,
    ///   1.0 V cutoff.
    /// - `LiFePO4`: A123 / generic LFP, 3.6 V surface-charge, extremely flat
    ///   ~3.3–3.2 V plateau, 2.5 V cutoff.
    #[must_use]
    pub fn ocv_per_cell(self, soc: f64) -> f64 {
        let s = soc.clamp(0.0, 1.0);
        let knots: &[(f64, f64)] = match self {
            Self::LiIon => &[
                (0.00, 3.00),
                (0.05, 3.30),
                (0.15, 3.50),
                (0.30, 3.65),
                (0.60, 3.80),
                (0.85, 4.00),
                (1.00, 4.20),
            ],
            Self::Alkaline => &[
                (0.00, 0.90),
                (0.10, 1.05),
                (0.30, 1.20),
                (0.60, 1.35),
                (0.85, 1.50),
                (1.00, 1.60),
            ],
            Self::NiMh => &[
                (0.00, 1.00),
                (0.10, 1.15),
                (0.25, 1.22),
                (0.70, 1.25),
                (0.90, 1.30),
                (1.00, 1.45),
            ],
            Self::LiFePO4 => &[
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
    /// voltage to hold the current at the limit (smooth CV→CC transition, see
    /// [`PowerSupply::cc_fold`]). Below the limit it is the CV Thevenin leg,
    /// which sinks a back-driven current like it sources one: only SOURCED
    /// current counts against the limit.
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
    /// `LiPo` cells (and the DW01/FS312-class protection ICs on bare cells) trip a
    /// MOSFET open when the pack current stays above a threshold for a short
    /// delay. This is exactly the dynamic that DC misses and that the Inkplate
    /// `WiFi` cold-boot inrush hits: the pack is fine at steady state but the TX
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
    /// Sticky flag: set true the moment the cutoff latches, and stays true until
    /// explicitly cleared by a reader. This survives a trip-then-rearm that both
    /// happen inside a single coarse frame, so a CI sampler that only reads
    /// `tripped` once per frame cannot alias the event away.
    #[serde(default)]
    pub ever_tripped: bool,
    /// Internal: accumulated time the over/under-current condition has held (s).
    #[serde(default)]
    pub accum_s: f64,
}

impl BatteryProtection {
    /// New protection with a trip threshold and delay; reset threshold = trip.
    #[must_use]
    pub const fn new(trip_a: f64, delay_s: f64) -> Self {
        Self {
            trip_a,
            delay_s,
            reset_a: trip_a,
            tripped: false,
            ever_tripped: false,
            accum_s: 0.0,
        }
    }

    /// Advance the protection state machine by `dt` seconds given the measured
    /// pack current `i_a`. Returns `true` if the cutoff is open (no output).
    fn update(&mut self, i_a: f64, dt: f64) -> bool {
        // Armed: time spent above the trip threshold latches it open. Tripped:
        // time spent below the reset threshold re-arms it. Anything else
        // drops the accumulator, so brief spikes under the delay do nothing.
        let flipping = if self.tripped {
            i_a < self.reset_a
        } else {
            i_a > self.trip_a
        };
        self.accum_s = if flipping { self.accum_s + dt } else { 0.0 };
        if flipping && self.accum_s >= self.delay_s {
            self.tripped = !self.tripped;
            self.ever_tripped |= self.tripped;
            self.accum_s = 0.0;
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
    #[must_use]
    pub const fn current_limit_a(self) -> f64 {
        match self {
            Self::V5_0_5A => 0.5,
            Self::V5_1_5A => 1.5,
            Self::V5_3A => 3.0,
        }
    }

    /// Effective cable + connector droop resistance (ohms). A real USB cable
    /// drops ~0.25 V at the rated current; we back that out into an ohmic droop.
    #[must_use]
    pub fn droop_ohms(self) -> f64 {
        // ~0.25 V at full rated current.
        0.25 / self.current_limit_a()
    }
}

impl Default for PowerSupply {
    fn default() -> Self {
        Self::Ideal { volts: 5.0 }
    }
}

impl PowerSupply {
    /// Nominal open-circuit voltage (no load), for seeding the first chunk and
    /// for diagnostics.
    #[must_use]
    pub fn nominal_volts(&self) -> f64 {
        match self {
            Self::Ideal { volts } | Self::Bench { volts, .. } | Self::Wall { volts, .. } => *volts,
            Self::Usb { .. } => 5.0,
            Self::Battery {
                chemistry,
                cells,
                soc,
                ..
            } => chemistry.ocv_per_cell(*soc) * f64::from(*cells),
        }
    }

    /// The series output resistance to stamp for this supply's Thevenin leg.
    #[must_use]
    pub fn series_r(&self) -> f64 {
        match self {
            Self::Ideal { .. } | Self::Bench { .. } => STIFF_R_OHMS,
            Self::Wall { r_out_ohms, .. } => r_out_ohms.max(STIFF_R_OHMS),
            Self::Usb { spec } => spec.droop_ohms().max(STIFF_R_OHMS),
            Self::Battery {
                r_internal_ohms, ..
            } => r_internal_ohms.max(STIFF_R_OHMS),
        }
    }

    /// State-of-charge readout (0..1), 1.0 for non-depleting supplies.
    #[must_use]
    pub const fn soc(&self) -> f64 {
        match self {
            Self::Battery { soc, .. } => *soc,
            _ => 1.0,
        }
    }

    /// Whether this supply's protection has latched open *right now*. False for
    /// supplies without protection (or unprotected batteries).
    #[must_use]
    pub const fn protection_tripped(&self) -> bool {
        matches!(self, Self::Battery { protection: Some(p), .. } if p.tripped)
    }

    /// Whether this supply's protection has tripped at *any* point so far
    /// (sticky). This is what a coarse-frame CI sampler should read, so a
    /// trip-then-rearm inside one frame is not aliased away.
    #[must_use]
    pub const fn protection_ever_tripped(&self) -> bool {
        matches!(self, Self::Battery { protection: Some(p), .. } if p.ever_tripped)
    }

    /// Compute the `Vsource` target voltage for the *next* chunk, given the
    /// rail current `i_a` (A, positive = sourced into the net) measured over
    /// the chunk just solved, the simulation time `t` (s, for ripple phase),
    /// and the chunk length `dt` (s, for battery `SoC` integration). May mutate
    /// internal state (battery `SoC` drains here).
    ///
    /// The returned voltage is the ideal source value *behind* `series_r()`;
    /// the solver adds the I·R drop across the series resistor on top, so the
    /// terminal voltage the load sees is `v_internal - i*series_r`. A current
    /// limit is not applied here but in the solve ([`Self::cc_fold`]).
    pub fn update(&mut self, i_a: f64, t: f64, dt: f64) -> f64 {
        let i = i_a.max(0.0);
        match self {
            Self::Ideal { volts } | Self::Bench { volts, .. } => *volts,

            // Nominal minus the ripple sine. The I·R_out drop is applied by the
            // solver through the series resistor, so we only add ripple here.
            Self::Wall {
                volts,
                ripple_vpp,
                ripple_hz,
                ..
            } => {
                let ripple = 0.5 * *ripple_vpp * (std::f64::consts::TAU * *ripple_hz * t).sin();
                *volts + ripple
            }

            // 5 V nominal; the connector/cable droop is the series resistor.
            Self::Usb { .. } => 5.0,

            // Drain charge and recompute the open-circuit stack voltage. The
            // internal resistance drop is the series resistor (solver-applied).
            // An optional BMS protection cutoff can latch the output to ~0 V when
            // the pack current trips it (the inrush-vs-protection interaction).
            Self::Battery {
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
                let ocv = chemistry.ocv_per_cell(*soc) * f64::from(*cells);
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
    #[must_use]
    pub const fn kind_label(&self) -> &'static str {
        match self {
            Self::Ideal { .. } => "ideal",
            Self::Bench { .. } => "bench",
            Self::Wall { .. } => "wall",
            Self::Usb { .. } => "usb",
            Self::Battery { .. } => "battery",
        }
    }

    /// The constant-current fold as solve-time expressions, or `None`
    /// without a limit: `[drop, clamp]`.
    ///
    /// `drop` reads `__d0`, the current SOURCED into the net (in the rail's
    /// own sense, so a negative rail mirrors), and is the voltage the source
    /// gives up: exactly zero up to the limit `L`, so the CV leg (back-driven
    /// current included) is untouched, then `R*q(I - L)` with `q` rounded
    /// quadratically over a knee of `L/50` and linear past it, `R =
    /// 1e4*|V|/L`, so folding the whole rail takes 0.25% over the limit. The
    /// solver's finite-difference probe steps `__d0` UP, into the fold, so
    /// its slope never undershoots near the onset (a probe that reached back
    /// onto the flat side would throw Newton past the root and cycle). It is
    /// NOT capped: a flat cap would hand Newton a tangent that never sees the
    /// steep CC band between two flat regions. Instead `clamp` reads the
    /// source node `__d0` and stops it crossing 0 V (1 mOhm, the output's
    /// reverse diode): an overload that does not respond to voltage (an ideal
    /// sink bigger than the port) floors the rail near 0 V instead of holding
    /// it below ground, and the excess is the clamp's, not the supply's; a CC
    /// source folds toward zero, it does not sink.
    #[must_use]
    pub fn cc_fold(&self) -> Option<[String; 2]> {
        let (volts, limit) = match self {
            Self::Bench {
                volts,
                current_limit_a,
            } => (*volts, *current_limit_a),
            Self::Usb { spec } => (5.0, spec.current_limit_a()),
            _ => return None,
        };
        let (s, knee, r) = (volts.signum(), limit / 50.0, 1e4 * volts.abs() / limit);
        let over = format!("(__d0-{limit:?})");
        let q = format!("min(max({over},0.0),{knee:?})^2/{:?}", 2.0 * knee);
        let drop = format!("{:?}*({q}+max({over}-{knee:?},0.0))", s * r);
        let clamp = format!("{s:?}*max({:?}*__d0,0.0)/{STIFF_R_OHMS:?}", -s);
        (limit > 0.0).then_some([drop, clamp])
    }
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
    /// Last solved rail current (A, sourced into the net; negative while the
    /// net back-drives the supply).
    pub last_current_a: f64,
    /// The constant-current fold's sense source, drop and clamp
    /// ([`PowerSupply::cc_fold`]), once a limited model has been stamped.
    cc: Option<[DeviceId; 3]>,
}

impl SupplyLeg {
    /// Stamp a behavioral supply onto `net` and return its handle. Mirrors the
    /// `Vrail_*` ideal source the binder would otherwise add, but routes through
    /// a measurable series resistor so the scheduler can read rail current.
    pub fn stamp(circuit: &mut Circuit, net: NodeId, net_name: &str, supply: PowerSupply) -> Self {
        let drv_node = circuit.node(&format!("__supply_{net_name}"));
        let v0 = supply.nominal_volts();
        let vsource = circuit.add(Device::vsource(
            format!("Vsupply_{net_name}"),
            drv_node,
            NodeId::GROUND,
            SourceKind::Dc(v0),
        ));
        let resistor = circuit.add(Device::resistor(
            format!("Rsupply_{net_name}"),
            drv_node,
            net,
            supply.series_r(),
        ));
        let mut leg = Self {
            net_name: net_name.to_string(),
            net,
            vsource,
            resistor,
            supply,
            last_current_a: 0.0,
            cc: None,
        };
        leg.fold(circuit);
        leg
    }

    /// Push a new behavioral model onto an already-stamped leg, retuning the
    /// source voltage, the series resistance and the current fold to match.
    /// May add devices (a first current limit): relayout after it.
    pub fn reconfigure(&mut self, circuit: &mut Circuit, supply: PowerSupply) {
        self.supply = supply;
        if let Some(Device::Resistor { ohms, .. }) =
            circuit.devices.get_mut(self.resistor.0 as usize)
        {
            *ohms = self.supply.series_r();
        }
        self.set_internal_volts(circuit, self.supply.nominal_volts());
        self.fold(circuit);
    }

    /// Update the leg from the rail current solved this chunk: recompute the
    /// supply's internal voltage and write it onto the `Vsource`.
    pub fn update(&mut self, circuit: &mut Circuit, i_a: f64, t: f64, dt: f64) {
        self.last_current_a = i_a;
        let v = self.supply.update(i_a, t, dt);
        self.set_internal_volts(circuit, v);
    }

    /// Stamp or retune the constant-current fold ([`PowerSupply::cc_fold`]):
    /// the source moves onto a private `_set` node, the drop sits between it
    /// and `_cc`, and a 0 V sense source carries the sourced current from
    /// `_cc` onto the drive node, oriented so its branch reads positive while
    /// the rail sources. The clamp sits on the drive node. Stamped for the
    /// first limited model only, so an unlimited leg keeps its source
    /// grounded on the drive node (a pinned node the partitioner cuts at); a
    /// later model rewrites the three in place (all zero when unlimited).
    fn fold(&mut self, circuit: &mut Circuit) {
        let fold = self.supply.cc_fold();
        if fold.is_none() && self.cc.is_none() {
            return;
        }
        let exprs = fold.unwrap_or_else(|| ["0.0".into(), "0.0".into()]);
        let [drop, clamp] = exprs.map(|e| CompiledExpr::compile(&e).expect("supply fold compiles"));
        let [set, cc, drv] = ["_set", "_cc", ""]
            .map(|tag| circuit.node(&format!("__supply_{}{tag}", self.net_name)));
        // Oriented so the sense branch reads positive while the rail sources.
        let [p, n] = if self.supply.nominal_volts() < 0.0 {
            [drv, cc]
        } else {
            [cc, drv]
        };
        let ids = self.cc.unwrap_or_else(|| {
            let next = circuit.devices.len();
            [0, 1, 2].map(|k| DeviceId(u32::try_from(next + k).expect("device count fits u32")))
        });
        let b = |tag, [p, n]: [NodeId; 2], output, expr, dep| Device::Behavioral {
            name: format!("Bsupply_{}_{tag}", self.net_name),
            p,
            n,
            output,
            expr,
            deps: vec![dep],
        };
        let (sense, gnd) = (format!("Vsupply_{}_sense", self.net_name), NodeId::GROUND);
        let (by_sense, by_drv) = (BDep::Branch(ids[0]), BDep::Volt(drv));
        let devices = [
            Device::vsource(sense, p, n, SourceKind::Dc(0.0)),
            b("cc", [set, cc], BOutput::Voltage, drop, by_sense),
            b("clamp", [gnd, drv], BOutput::Current, clamp, by_drv),
        ];
        if self.cc.is_some() {
            for (id, dev) in ids.into_iter().zip(devices) {
                circuit.devices[id.0 as usize] = dev;
            }
            return;
        }
        if let Some(Device::Vsource { p, .. }) = circuit.devices.get_mut(self.vsource.0 as usize) {
            *p = set;
        }
        self.cc = Some(devices.map(|dev| circuit.add(dev)));
    }

    /// Open the leg's sources: each becomes an `ohms` resistor across its own
    /// terminals, not a deletion, so no `DeviceId` shifts under a caller
    /// holding one. Returns whether anything was opened.
    pub fn open(&self, circuit: &mut Circuit, ohms: f64) -> bool {
        let mut opened = false;
        for id in std::iter::once(self.vsource).chain(self.cc.into_iter().flatten()) {
            let dev = circuit.devices.get_mut(id.0 as usize);
            if let Some(dev @ (Device::Vsource { .. } | Device::Behavioral { .. })) = dev {
                let (name, ends) = (dev.name().to_string(), dev.nodes());
                *dev = Device::resistor(name, ends[0], ends[1], ohms);
                opened = true;
            }
        }
        opened
    }

    fn set_internal_volts(&self, circuit: &mut Circuit, v: f64) {
        if let Some(Device::Vsource { kind, .. }) = circuit.devices.get_mut(self.vsource.0 as usize)
        {
            *kind = SourceKind::Dc(v);
        }
    }
}

/// Piecewise-linear interpolation of `(x, y)` knots (assumed x-ascending) at
/// `x`. Clamps to the endpoints outside the range.
fn interp(knots: &[(f64, f64)], x: f64) -> f64 {
    let (Some(&(x_first, y_first)), Some(&(x_last, y_last))) = (knots.first(), knots.last()) else {
        return 0.0;
    };
    if x <= x_first {
        return y_first;
    }
    let segment = knots
        .windows(2)
        .find(|w| x <= w[1].0)
        .filter(|_| x < x_last);
    segment.map_or(y_last, |w| {
        let ((x0, y0), (x1, y1)) = (w[0], w[1]);
        (y1 - y0).mul_add((x - x0) / (x1 - x0), y0)
    })
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
            assert!(
                full > empty,
                "{chem:?} full {full} should exceed empty {empty}"
            );
            // Monotonic non-decreasing in SoC.
            let mut prev = empty;
            for k in 1..=10 {
                let v = chem.ocv_per_cell(f64::from(k) / 10.0);
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
            s.update(1.0, 0.0, dt);
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
        let v = s.update(2.0, 0.0, dt);
        assert!(v.abs() < 1e-12, "tripped pack commands 0 V, got {v}");
        assert!(s.protection_tripped());

        // Remove the load (below reset) for longer than the delay: re-arms.
        for _ in 0..10 {
            s.update(0.0, 0.0, dt);
        }
        assert!(
            !s.protection_tripped(),
            "protection should re-arm on load removal"
        );
    }

    /// The CC fold's laws, per limited model. The drop at a SOURCED current
    /// `i`: exactly none up to the limit, back-driven current included;
    /// monotone past it; the whole rail folded 0.25% over. The clamp: nothing
    /// while the source node stays on its rail's side of 0 V, then 1 mOhm
    /// back to it, so an unsatisfiable sink floors the rail instead of
    /// holding it below ground. A negative rail folds mirrored.
    #[test]
    fn cc_fold_is_exact_to_the_limit_and_clamps_at_zero() {
        let usb = PowerSupply::Usb {
            spec: UsbSpec::V5_0_5A,
        };
        let bench = |volts| PowerSupply::Bench {
            volts,
            current_limit_a: 3.0,
        };
        for (supply, v, lim) in [
            (usb, 5.0_f64, 0.5),
            (bench(15.0), 15.0, 3.0),
            (bench(-12.0), -12.0, 3.0),
        ] {
            let [drop, clamp] = supply
                .cc_fold()
                .expect("a limit folds")
                .map(|e| CompiledExpr::compile(&e).unwrap());
            let drop = |i: f64| drop.eval(&[i], 0.0).unwrap();
            let under = [-2.0 * lim, -1e-3, 0.0, 0.5 * lim, lim].map(drop);
            assert!(
                under.iter().all(|&d| d == 0.0),
                "{supply:?}: {under:?} under the limit"
            );
            let over: Vec<f64> = (0..=60)
                .map(|k| drop(lim * f64::from(k).mul_add(5e-5, 1.0)))
                .collect();
            let monotone = over.windows(2).all(|w| (w[1] - w[0]) * v > 0.0);
            let (sag, full) = (over[20] / v, over[50] / v);
            assert!(
                monotone && sag > 0.0 && sag < 1.0 && full >= 1.0,
                "{supply:?}: monotone {monotone}, 0.1% over sags {sag}, 0.25% over {full} of the rail"
            );
            let clamp = |node_v: f64| clamp.eval(&[node_v], 0.0).unwrap();
            let (idle, below) = ([clamp(v), clamp(0.0)], clamp(-1e-3 * v.signum()));
            assert!(
                idle.iter().all(|&c| c == 0.0) && (below - v.signum()).abs() < 1e-9,
                "{supply:?}: clamp {idle:?} on its side of 0 V, {below} A 1 mV past it"
            );
        }
        assert!(PowerSupply::Ideal { volts: 5.0 }.cc_fold().is_none());
    }
}
