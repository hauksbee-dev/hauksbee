//! Dynamic load: a chip-activity current sink driven by a [`LoadProfile`].
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/peripherals.md.
//!
//! This is the transient consumer of the `hauksbee-models` load profiles. It
//! owns an `Isource` drawing from the part's supply node to ground and, each
//! chunk, sets the source value to the profile's current at the current sim
//! time (offset by the configured start). The existing `Isource` machinery
//! carries this through *both* solver paths: the monolithic engine evaluates the
//! source at `ctx.time`, and the partitioned engine routes it as a current-input
//! column into the affected linear island (verified in hauksbee-solve). So the
//! same dI/dt the chip imposes reaches the rail, and decoupling / supply
//! impedance is exercised honestly.
//!
//! The `Isource` sits on a private node behind a 0 V sense source, and a
//! behavioral sink `p = net`, `n = GROUND` draws its current out of `net`,
//! folded to zero as the rail falls to 0 V (see [`DynamicLoad::new`]).

use std::collections::HashMap;

use hauksbee_ir::{BDep, BOutput, Circuit, CompiledExpr, Device, DeviceId, NodeId};
use hauksbee_ir::{PwlPoint, SourceKind};
use hauksbee_models::LoadProfile;

use super::{Peripheral, TickCtx};

/// Profile corners one chunk may carry as a PWL drive (the GPIO replay's budget).
const MAX_CORNERS: usize = 10_000;

/// Rail voltage (V) from which the load draws its full profile current; it
/// folds to zero at 0 V. Well under any logic supply's brown-out, so a
/// powered rail is never touched.
const COMPLIANCE_V: f64 = 0.25;

/// A dynamic load presenting a [`LoadProfile`] current draw on one supply node.
pub struct DynamicLoad {
    id: String,
    sink: DeviceId,
    profile: LoadProfile,
    /// Time offset (s): the profile starts drawing its activity at this sim time.
    start_s: f64,
    /// Deterministic seed for profile jitter.
    seed: u64,
    /// Last commanded current (A), for state readout.
    last_i: f64,
    /// Peak current commanded so far (A).
    peak_i: f64,
}

impl DynamicLoad {
    /// Stamp a dynamic load drawing `profile` out of `net`, beginning at
    /// `start_s` seconds, with profile jitter `seed`.
    ///
    /// # Panics
    /// Never: the compliance fold is a fixed, valid expression.
    pub fn new(
        circuit: &mut Circuit,
        id: &str,
        net: NodeId,
        profile: LoadProfile,
        start_s: f64,
        seed: u64,
    ) -> Self {
        // The profile Isource (seeded at the pre-start level so the DC
        // operating point is consistent) runs through a private 0 V sense
        // source, and the net's draw is its current times a smoothstep fold
        // of the net's voltage: exactly the profile from COMPLIANCE_V up, zero
        // at 0 V and below, C1 between. A real load draws nothing unpowered;
        // an ideal sink bigger than a limited supply instead pulled the rail
        // below ground through the supply's own output resistance.
        let i0 = profile.current_at(-1.0, seed);
        let (at, gnd) = (circuit.node(&format!("__load_{id}")), NodeId::GROUND);
        let (name, dc) = (format!("Iload_{id}"), SourceKind::Dc(i0));
        let sink = circuit.add(Device::isource(name, at, gnd, dc));
        let (name, dc) = (format!("Vload_{id}_sense"), SourceKind::Dc(0.0));
        let sense = circuit.add(Device::vsource(name, gnd, at, dc));
        let x = format!("min(max(__d1/{COMPLIANCE_V:?},0.0),1.0)");
        let fold = CompiledExpr::compile(&format!("__d0*{x}^2*(3.0-2.0*{x})"));
        circuit.add(Device::Behavioral {
            name: format!("Bload_{id}"),
            p: net,
            n: gnd,
            output: BOutput::Current,
            expr: fold.expect("load fold compiles"),
            deps: vec![BDep::Branch(sense), BDep::Volt(net)],
        });
        Self {
            id: id.to_string(),
            sink,
            profile,
            start_s,
            seed,
            last_i: i0,
            peak_i: i0,
        }
    }

    /// The current the profile draws at sim time `t` (s).
    fn current_at(&self, t: f64) -> f64 {
        self.profile.current_at(t - self.start_s, self.seed)
    }

    fn set_kind(&self, circuit: &mut Circuit, value: SourceKind) {
        if let Some(Device::Isource { kind, .. }) = circuit.devices.get_mut(self.sink.0 as usize) {
            *kind = value;
        }
    }
}

impl Peripheral for DynamicLoad {
    fn id(&self) -> &str {
        &self.id
    }
    fn kind(&self) -> &'static str {
        "dynamic_load"
    }

    fn pre_solve(&mut self, ctx: &mut TickCtx) {
        // Drive the sink along the profile's own waveform through this chunk
        // (chunk-local time, as the march evaluates it), so every edge keeps
        // its rise time; the solver lands a step on each corner. Holding the
        // chunk-end sample instead turned a 25 ns edge into a step, which on a
        // rail with capacitor ESL is an L*di/dt impulse of kilovolts that the
        // trapezoidal rule then rings on. A train far finer than the chunk
        // keeps the zero-order hold.
        let a = ctx.t - self.start_s;
        let i = self.current_at(ctx.t + ctx.dt);
        let kind = match self.profile.pwl_over(a, a + ctx.dt, self.seed, MAX_CORNERS) {
            Some(pts) if pts.iter().any(|p| p.1.total_cmp(&i).is_ne()) => {
                self.peak_i = pts.iter().fold(self.peak_i, |m, p| m.max(p.1));
                SourceKind::Pwl(pts.into_iter().map(|(t, v)| PwlPoint { t, v }).collect())
            }
            _ => SourceKind::Dc(i),
        };
        self.last_i = i;
        self.peak_i = self.peak_i.max(i);
        self.set_kind(ctx.circuit, kind);
    }

    fn post_solve(&mut self, ctx: &mut TickCtx) {
        // Settle at the chunk-end level so everything between solves (the DC
        // seed, the stress monitor, a fallback rung's constant-forcing check)
        // sees the level the load rests at.
        self.set_kind(ctx.circuit, SourceKind::Dc(self.last_i));
    }

    fn set_value(&mut self, value: f64) {
        // Live override: shift the start time (a scrub) is not meaningful; treat
        // `value` as a manual current clamp the next chunk applies.
        if let Some(seg) = self.profile.segments.first_mut() {
            seg.level_a = value.max(0.0);
        }
    }

    fn state(&self) -> HashMap<String, f64> {
        let mut m = HashMap::new();
        m.insert("current_a".into(), self.last_i);
        m.insert("peak_a".into(), self.peak_i);
        m
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dynamic_load_drives_profile_current() {
        let mut c = Circuit::new();
        let rail = c.node("VDD");
        let profile = LoadProfile::by_id("esp32_boot_wifi").unwrap();
        let mut load = DynamicLoad::new(&mut c, "U5", rail, profile, 0.0, 0);

        // Step to a time inside a WiFi TX burst: current should be near 240 mA.
        let volts = vec![0.0; c.node_count()];
        // Burst train starts ~1 ms in (after the 1 ms baseline ramp); hold peak
        // around t = 1ms + 0.5ms.
        load.pre_solve(&mut TickCtx::new(&mut c, &volts, 0.0016, 1e-5));
        assert!(
            (load.last_i - 0.240).abs() < 1e-3,
            "expected ~240 mA TX burst, got {}",
            load.last_i
        );
        // The stamped Isource carries it; the fold draws it off the rail.
        let on_rail = |d: &Device| matches!(d, Device::Behavioral { p, .. } if *p == rail);
        assert!(c.devices.iter().any(on_rail));
        if let Device::Isource { kind, n, .. } = &c.devices[load.sink.0 as usize] {
            assert_eq!(*n, NodeId::GROUND);
            assert!((kind.eval(0.0) - 0.240).abs() < 1e-3);
        } else {
            panic!("sink is not an Isource");
        }
    }
}
