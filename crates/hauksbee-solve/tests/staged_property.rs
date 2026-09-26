//! Property gate: staged execution equals the monolith on random topologies.
//!
//! The targeted fixtures in `orchestrate::staged` pin known shapes (the
//! comparator chain, the Thevenin absorption, the shunt-fed array). This gate
//! attacks the space between them: pseudo-random feedforward boards (seeded,
//! fully deterministic) with random island counts, chain depths, coupling
//! fan-outs, source waveforms, and thresholds. Every board whose
//! decomposition certificate is sound must solve to the same waveforms the
//! monolithic reference produces.
//!
//! ## The comparison bar
//!
//! Away from switching events the two runs must agree to solver tolerance.
//! AT a switching event, the certificate's own claim is the capture grid: a
//! replayed edge is linearly interpolated across one step, so the downstream
//! switch may fire up to one grid interval away from the monolith's firing
//! time, and a pointwise compare at a grid point straddled by the edge would
//! see a full-swing difference that the certificate never promised to
//! prevent. The honest check is therefore two-sided: a point passes if the
//! values agree, OR if the other run attains that value within one capture
//! interval (the edge happened, within the claimed time tolerance). Points
//! failing BOTH are real divergences and fail the gate.
//!
//! Randomness note: a tiny inline xorshift keeps the suite dependency-free
//! and every seed reproducible in a failure message.

use hauksbee_ir::{Circuit, Device, NodeId, SourceKind};
use hauksbee_solve::decompose::rails::TearMotive;
use hauksbee_solve::decompose::verify::Decomposition;
use hauksbee_solve::orchestrate::run_staged;

use crate::shared::{add_resistor, lerp_at};
use hauksbee_solve::{Integration, Partitioning, SolverOptions, StepControl, Transient, Waveforms};
use hauksbee_testkit::circuits::{cap, comparator, vswitch};

/// Deterministic xorshift64*; good enough to scatter topologies, and every
/// draw is reproducible from the seed printed on failure.
struct Rng(u64);
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)] // random bits: truncation is intended
impl Rng {
    const fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    const fn pick(&mut self, lo: usize, hi: usize) -> usize {
        lo + (self.next() as usize) % (hi - lo + 1)
    }
    fn f(&mut self, lo: f64, hi: f64) -> f64 {
        (self.next() as f64 / u64::MAX as f64).mul_add(hi - lo, lo)
    }
    const fn coin(&mut self) -> bool {
        self.next() & 1 == 1
    }
}

/// One random feedforward board: `n` source-fed RC islands, then random
/// couplings that always point from a lower island index to a higher one
/// (feedforward by construction; cyclic shapes are the fused-group tests'
/// job, not this gate's). Couplings alternate comparators (which conduct
/// their own out node into the downstream island) and switch controls
/// (pure sense, no new conduction).
fn random_board(seed: u64) -> Circuit {
    let mut rng = Rng(seed | 1);
    let mut c = Circuit::new();
    let n_islands = rng.pick(2, 5);

    // Build islands; remember each one's tail node (the sensed candidate)
    // and a node the island conducts that couplings can bridge into.
    let mut tails: Vec<NodeId> = Vec::new();
    let mut hubs: Vec<NodeId> = Vec::new();
    for i in 0..n_islands {
        let vin = c.node(&format!("i{i}_vin"));
        let kind = if rng.coin() {
            SourceKind::Dc(rng.f(1.0, 5.0))
        } else {
            SourceKind::Pulse {
                v1: 0.0,
                v2: rng.f(2.0, 5.0),
                delay: rng.f(0.5e-6, 3e-6),
                rise: rng.f(0.2e-6, 1e-6),
                fall: rng.f(0.2e-6, 1e-6),
                width: rng.f(3e-6, 8e-6),
                period: 0.0,
            }
        };
        c.add(Device::vsource(format!("V{i}"), vin, NodeId::GROUND, kind));
        let depth = rng.pick(1, 3);
        let mut prev = vin;
        let mut tail = vin;
        for k in 0..depth {
            let node = c.node(&format!("i{i}_n{k}"));
            add_resistor(&mut c, &format!("R{i}_{k}"), prev, node, rng.f(0.5e3, 5e3));
            cap(
                &mut c,
                &format!("C{i}_{k}"),
                node,
                NodeId::GROUND,
                rng.f(0.2e-9, 2e-9),
            );
            prev = node;
            tail = node;
        }
        tails.push(tail);
        hubs.push(tail);
    }

    // Random feedforward couplings: 1..=2 per downstream island.
    for (down, &hub) in hubs.iter().enumerate().skip(1) {
        for edge in 0..rng.pick(1, 2) {
            let up = rng.pick(0, down - 1);
            if rng.coin() {
                // Comparator living in `down`, watching `up`'s tail.
                let out = c.node(&format!("cmp_{up}_{down}_{edge}"));
                add_resistor(&mut c, &format!("RB{up}_{down}_{edge}"), out, hub, 1e3);
                comparator(
                    &mut c,
                    &format!("K{up}_{down}_{edge}"),
                    out,
                    tails[up],
                    NodeId::GROUND,
                    2e-3,
                );
            } else {
                // Switch in `down` gated by `up`'s tail.
                let o = c.node(&format!("sw_{up}_{down}_{edge}"));
                vswitch(
                    &mut c,
                    &format!("S{up}_{down}_{edge}"),
                    hub,
                    o,
                    (tails[up], NodeId::GROUND),
                    (rng.f(1.0, 2.5), rng.f(0.3, 0.9)),
                    (10.0, 1e9),
                );
                add_resistor(
                    &mut c,
                    &format!("RS{up}_{down}_{edge}"),
                    o,
                    NodeId::GROUND,
                    10e3,
                );
            }
        }
    }
    c
}

/// Two-sided compare per the module doc: value agreement, or the same value
/// attained by the reference within one capture interval around the point.
fn diverges(
    staged: &Waveforms,
    mono: &Waveforms,
    node: usize,
    dt: f64,
    tol: f64,
) -> Option<(f64, f64, f64)> {
    for (k, &t) in staged.time.iter().enumerate() {
        let sv = staged.node_voltages[node][k];
        let mv = lerp_at(&mono.time, &mono.node_voltages[node], t);
        if (sv - mv).abs() <= tol {
            continue;
        }
        // Edge window: does the reference reach sv within +/- dt of t?
        let hit = (0..=8).any(|j| {
            let tt = f64::from(j).mul_add(dt / 4.0, t - dt);
            (lerp_at(&mono.time, &mono.node_voltages[node], tt) - sv).abs() <= tol
        });
        if !hit {
            return Some((t, sv, mv));
        }
    }
    None
}

#[test]
fn random_feedforward_boards_match_the_monolith() {
    let dt = 100e-9;
    let tstop = 12e-6;
    let tol = 1e-6;
    let opts = SolverOptions {
        step: StepControl::Fixed { dt },
        integration: Integration::Trapezoidal,
        ..SolverOptions::default()
    };
    let mut mono_opts = opts;
    mono_opts.partitioning = Partitioning::Off;

    let mut sound_boards = 0;
    let mut staged_boards = 0;
    for seed in 1..=20u64 {
        let c = random_board(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let d = Decomposition::analyze(&c, TearMotive::Profit);
        // Random boards must not be able to produce unsound certificates:
        // every sense edge here is conducted upstream by construction.
        assert!(
            d.certificate.sound(),
            "seed {seed}: generator produced an unsound certificate\n{}",
            d.certificate.summary(&c)
        );
        sound_boards += 1;
        if d.dag.groups.len() > 1 {
            staged_boards += 1;
        }

        let staged = run_staged(&c, &d, &opts, tstop)
            .unwrap_or_else(|e| panic!("seed {seed}: staged run failed: {e}"));
        let mono = Transient::new(mono_opts)
            .run(&c, tstop)
            .unwrap_or_else(|e| panic!("seed {seed}: monolith failed: {e}"));

        for (node, id) in (1..c.node_count()).zip(1u32..) {
            if let Some((t, sv, mv)) = diverges(&staged.waveforms, &mono, node, dt, tol) {
                panic!(
                    "seed {seed}: node {} diverged at t={t:.3e}: staged {sv:.9} vs mono {mv:.9}",
                    c.node_name(NodeId(id)),
                );
            }
        }
    }
    assert_eq!(sound_boards, 20);
    assert!(
        staged_boards >= 15,
        "generator degenerated: only {staged_boards}/20 boards actually decomposed"
    );
}
