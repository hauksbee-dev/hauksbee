//! Dynamic load: a chip-activity current sink driven by a [`LoadProfile`].
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/peripherals.md.
//!
//! This is the transient consumer of the `hauksbee-models` load profiles. It
//! owns an `Isource` stamped from the part's supply node to ground and, each
//! chunk, sets the source value to the profile's current at the current sim
//! time (offset by the configured start). The existing `Isource` machinery
//! carries this through *both* solver paths: the monolithic engine evaluates the
//! source at `ctx.time`, and the partitioned engine routes it as a current-input
//! column into the affected linear island (verified in hauksbee-solve). So the
//! same dI/dt the chip imposes reaches the rail, and decoupling / supply
//! impedance is exercised honestly.
//!
//! The sink draws *out of* the supply node (positive load current), so it is
//! stamped `p = net`, `n = GROUND`: an `Isource` pushes current `p -> n`
//! internally, i.e. it pulls current out of `net` into ground, which is a load.

use std::collections::HashMap;

use hauksbee_ir::{Circuit, Device, DeviceId, NodeId, PwlPoint, SourceKind};
use hauksbee_models::LoadProfile;

use super::{Peripheral, TickCtx};

/// Profile corners one chunk may carry as a PWL drive (the GPIO replay's budget).
const MAX_CORNERS: usize = 10_000;

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
    pub fn new(
        circuit: &mut Circuit,
        id: &str,
        net: NodeId,
        profile: LoadProfile,
        start_s: f64,
        seed: u64,
    ) -> Self {
        // Isource current flows p -> n internally; with p = net, n = GROUND it
        // pulls current out of the net (a sink / load). Seed it at the profile's
        // pre-start level so the DC operating point is consistent.
        let i0 = profile.current_at(-1.0, seed);
        let sink = circuit.add(Device::isource(
            format!("Iload_{id}"),
            net,
            NodeId::GROUND,
            SourceKind::Dc(i0),
        ));
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
            Some(pts) if pts.iter().any(|p| p.1 != i) => {
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
        // The stamped Isource carries it.
        if let Device::Isource { kind, p, n, .. } = &c.devices[load.sink.0 as usize] {
            assert_eq!(*p, rail);
            assert_eq!(*n, NodeId::GROUND);
            assert!((kind.eval(0.0) - 0.240).abs() < 1e-3);
        } else {
            panic!("sink is not an Isource");
        }
    }
}
