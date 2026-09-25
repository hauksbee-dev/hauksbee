//! Stamp scaffolding and waveform comparisons shared by the crate's unit
//! tests, so each test states only the circuit and the number it expects.
//! Test-only: `lib.rs` declares this module under `#[cfg(test)]`, and
//! nothing in the shipped library depends on it.
//!
//! The device-literal builders (`res`, `cap`, `vdc`, ...) that used to live
//! here moved to `hauksbee-testkit::circuits`, which unlike this
//! `#[cfg(test)]`-private module is reachable from this crate's own
//! integration tests (and every other crate's), so there is exactly one
//! copy of each builder instead of one private and one duplicated in
//! `tests/support`. Re-exported here so existing `test_fixtures::res(..)`
//! call sites in this crate's unit tests keep working unchanged.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::LazyLock;

use hauksbee_ir::{Circuit, DeviceId, NodeId};

use crate::options::{Integration, Partitioning, SolverOptions, StepControl};
use crate::stamp::{IntegCoeffs, StampCtx};
use crate::system::{Layout, ReactiveState};
use crate::transient::{Transient, Waveforms};

pub use hauksbee_testkit::circuits::{
    bjt, cap, cap_ic, comparator, diode, divider, idc, ind, pnp, pnp_blocks, res, shunt_array, sw,
    vdc, vdc_between, vpulse, vswitch, GND,
};

pub fn fixed_opts(dt: f64) -> SolverOptions {
    SolverOptions {
        step: StepControl::Fixed { dt },
        integration: Integration::Trapezoidal,
        ..SolverOptions::default()
    }
}

/// The fused monolithic transient of `c` under `opts` (partitioning off).
pub fn monolith(c: &Circuit, opts: &SolverOptions, tstop: f64) -> Waveforms {
    let mut mono_opts = *opts;
    mono_opts.partitioning = Partitioning::Off;
    Transient::new(mono_opts).run(c, tstop).expect("monolith")
}

pub(crate) use crate::orchestrate::lerp_at;

/// Max |sample - reference| over every node (ground excluded) at every sample
/// time of `time`, interpolating the reference.
// `node` indexes the solved and the reference waveforms alike and names the node.
#[allow(clippy::needless_range_loop)]
pub fn max_error(
    c: &Circuit,
    time: &[f64],
    nodes: &[Vec<f64>],
    reference: &Waveforms,
) -> (f64, NodeId) {
    let mut worst = (0.0f64, NodeId(0));
    for node in 1..c.node_count() {
        for (k, &t) in time.iter().enumerate() {
            let err = (nodes[node][k]
                - lerp_at(&reference.time, &reference.node_voltages[node], t))
            .abs();
            if err > worst.0 {
                worst = (err, NodeId(node as u32));
            }
        }
    }
    worst
}

/// Two-sided capture-grid compare: every sample must agree with the reference
/// within `tol(reference value)`, or the reference must attain the sample's
/// value somewhere within `±dt` (an edge placed up to one grid interval away).
// `node` indexes the solved and the reference waveforms alike and names the node.
#[allow(clippy::needless_range_loop)]
pub fn assert_matches_within_grid(
    c: &Circuit,
    time: &[f64],
    nodes: &[Vec<f64>],
    reference: &Waveforms,
    dt: f64,
    tol: &dyn Fn(f64) -> f64,
    what: &str,
) {
    for node in 1..c.node_count() {
        let m = &reference.node_voltages[node];
        for (k, &t) in time.iter().enumerate() {
            let sv = nodes[node][k];
            let mv = lerp_at(&reference.time, m, t);
            let tol = tol(mv);
            if (sv - mv).abs() <= tol {
                continue;
            }
            let edge = (0..=8).any(|j| {
                let tt = t - dt + (j as f64) * (dt / 4.0);
                (lerp_at(&reference.time, m, tt) - sv).abs() <= tol
            });
            assert!(
                edge,
                "{what}: diverged at {} t={t:.3e}: {sv:.9} vs {mv:.9} (tol {tol:.3e})",
                c.node_name(NodeId(node as u32))
            );
        }
    }
}

pub fn swing(v: &[f64]) -> f64 {
    v.iter().cloned().fold(f64::MIN, f64::max) - v.iter().cloned().fold(f64::MAX, f64::min)
}

/// Up-crossings of `level`: one per spike.
pub fn up_crossings(v: &[f64], level: f64) -> usize {
    v.windows(2)
        .filter(|w| w[0] < level && w[1] >= level)
        .count()
}

static NO_SPDT: LazyLock<HashMap<DeviceId, DeviceId>> = LazyLock::new(HashMap::new);

/// A transient-shaped stamp context with every optional knob off: gmin 0,
/// no regularizer, no frozen decisions. Callers adjust the public fields.
pub fn stamp_ctx<'a>(
    circuit: &'a Circuit,
    layout: &'a Layout,
    opts: &'a SolverOptions,
    x: &'a [f64],
    state: &'a ReactiveState,
    coeffs: IntegCoeffs,
) -> StampCtx<'a> {
    StampCtx {
        circuit,
        layout,
        opts,
        x,
        x_prev: x,
        time: 0.0,
        coeffs,
        state,
        dc: false,
        use_ic: false,
        gmin: 0.0,
        src_scale: 1.0,
        branch_reg: 0.0,
        cmp_freeze: None,
        switch_freeze: None,
        switch_latch: None,
        spdt_sibling: &NO_SPDT,
        junction_eval: None,
    }
}

pub fn trapz(dt: f64) -> IntegCoeffs {
    IntegCoeffs::for_step(Integration::Trapezoidal, dt, dt, false)
}
