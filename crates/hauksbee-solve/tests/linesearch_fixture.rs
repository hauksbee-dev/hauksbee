//! Line-search-armed determinism fixture: the bitwise regression witness for
//! optimization work inside `newton_solve`'s Armijo/staged machinery.
//!
//! The flagship general path marches its joint capture with
//! HAUKSBEE_TRANSIENT_DYN set (which arms the staged regularizers, the
//! event-freeze retry AND the global Armijo line search) plus the SPDT
//! break-before-make and control-gm device knobs. Any "this optimization is
//! bit-identical" claim about that machinery needs a fast fixture that
//! exercises the same code paths and reduces the whole waveform to one
//! comparable line. This board is an integrate-and-fire relaxation
//! oscillator: a membrane cap charged by a current source fires a hysteretic
//! comparator whose output flips an SPDT pair (binder-style `_s0`/`_s1` legs
//! sharing the common node, so the BBM winner-take-all path runs), shorting
//! the membrane through a fast discharge leg. No consistent DC exists (the
//! usual self-resetting shape), so it powers on FromZero like the capture
//! recipe and spikes repeatedly; comparator flips guarantee the event/backtrack
//! paths see real work.
//!
//! The hash printed here is compared ACROSS COMMITS (run with --nocapture):
//! identical hash = bit-identical accepted grid. Within one run the march is
//! executed twice and the hashes asserted equal, which pins determinism (no
//! hidden state, no map-iteration-order leakage into the numerics).
//!
//! Env-var hazard: HAUKSBEE_* are process-global, so this test lives in its
//! own test binary (this file) and must stay the only test in it.

use hauksbee_ir::{Circuit, Device, NodeId, SourceKind};
use hauksbee_solve::{
    DcInit, DeviceEffects, EventRetryTuning, RobustnessLadder, SolverOptions, StepControl,
    Strategy, Transient,
    Waveforms,
};

/// Integrate-and-fire relaxation oscillator with a binder-style SPDT pair.
fn spiking_board() -> Circuit {
    let mut c = Circuit::new();
    let m = c.node("m"); // membrane
    let th = c.node("th"); // comparator threshold
    let spk = c.node("spk"); // comparator output
    let spkb = c.node("spkb"); // inverted comparator output (idle-leg select)
    let com = c.node("com"); // SPDT common node
    let rail = c.node("rail"); // idle-leg bias

    c.add(Device::Vsource {
        name: "VTH".into(),
        p: th,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(2.5),
    });
    c.add(Device::Vsource {
        name: "VRAIL".into(),
        p: rail,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(1.0),
    });
    // Charge current into the membrane.
    c.add(Device::Isource {
        name: "IIN".into(),
        p: NodeId::GROUND,
        n: m,
        kind: SourceKind::Dc(5e-5),
    });
    c.add(Device::Capacitor {
        name: "CM".into(),
        a: m,
        b: NodeId::GROUND,
        farads: 1e-9,
        ic: None,
    });
    // Leak sized so the open-gate equilibrium (5 V) sits ABOVE threshold:
    // firing is guaranteed, so there is no DC fixed point and the board is
    // the honest FromZero shape.
    c.add(Device::Resistor {
        name: "RL".into(),
        a: m,
        b: NodeId::GROUND,
        ohms: 1e5,
        tc1: None,
    });
    c.add(Device::Comparator {
        name: "K1".into(),
        out: spk,
        inp: m,
        inn: th,
        out_lo: 0.0,
        out_hi: 5.0,
        hysteresis: 0.2,
    });
    // Complementary select for the idle leg. The two throws must be driven by
    // OPPOSITE controls (as real SPDT select wiring is): with a shared control
    // the BBM winner-take-all margins tie exactly (the sibling margin is
    // span-absolute), the discharge leg gets halved forever, and the membrane
    // servos at threshold instead of firing; measured on the first cut of this
    // fixture.
    c.add(Device::Comparator {
        name: "K2".into(),
        out: spkb,
        inp: th,
        inn: m,
        out_lo: 0.0,
        out_hi: 5.0,
        hysteresis: 0.2,
    });
    // SPDT: names differ only in the `_s0`/`_s1` suffix and share the common
    // node `com`, exactly the shape `SpdtPairs::analyze` pairs up, so the
    // break-before-make winner-take-all code runs on every stamp.
    c.add(Device::VSwitch {
        name: "GATE_s1".into(), // discharge leg: on while the spike is high
        a: com,
        b: m,
        ctrl_p: spk,
        ctrl_n: NodeId::GROUND,
        von: 3.0,
        voff: 2.0,
        ron: 10.0,
        roff: 1e9,
    });
    c.add(Device::VSwitch {
        name: "GATE_s0".into(), // idle leg: on while the spike is low
        a: com,
        b: rail,
        ctrl_p: spkb,
        ctrl_n: NodeId::GROUND,
        von: 3.0,
        voff: 2.0,
        ron: 10.0,
        roff: 1e9,
    });
    // Discharge path: membrane -> s1 -> com -> RD -> ground, tau ~ 60 ns
    // (the fast post-flip transient the event/LTE machinery must track).
    c.add(Device::Resistor {
        name: "RD".into(),
        a: com,
        b: NodeId::GROUND,
        ohms: 50.0,
        tc1: None,
    });
    c
}

/// FNV-1a over the raw bits of every sample: time, then each node voltage.
/// Same construction as the census waveform hash, computed independently here
/// so the fixture does not depend on the census env var.
fn wf_hash(wf: &Waveforms) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut fold = |v: f64| {
        for b in v.to_bits().to_le_bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    for k in 0..wf.time.len() {
        fold(wf.time[k]);
        for col in &wf.node_voltages {
            fold(col[k]);
        }
    }
    h
}

fn march() -> Waveforms {
    let c = spiking_board();
    let opts = SolverOptions {
        // The capture recipe's adaptive envelope at the fixture's time scale.
        step: StepControl::Adaptive {
            dt_initial: 1e-6,
            dt_min: 1e-12,
            // Overridable for grid-refinement studies (the burst-count
            // convergence note at the assertion below); the default matches
            // the capture recipe's dt_max = 2*dt at this sampling scale.
            dt_max: std::env::var("HAUKSBEE_FIXTURE_DT_MAX")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(2e-6),
        },
        dc_init: DcInit::FromZero,
        // The flagship spike-path bundle, typed since the env migration.
        ladder: RobustnessLadder::none().with(Strategy::TransientDyn),
        event_retry: EventRetryTuning {
            smooth_comparator_first: true,
            ..Default::default()
        },
        effects: DeviceEffects {
            // The flagship substrate drops the switch control tangent (the
            // torn-column high-Z boundary problem); typed since the env
            // migration.
            switch_ctrl_gm: false,
            ..Default::default()
        },
        ..Default::default()
    };
    Transient::new(opts)
        .run(&c, 300e-6)
        .expect("armed fixture march must carry the oscillator")
}

#[test]
fn armed_march_is_deterministic_and_spikes() {
    // The flagship general path's numerical substrate (tarski_general_e2e.rs
    // sets these as its own configuration); TRANSIENT_DYN is what arms the
    // line search this fixture exists to witness.

    let wf = march();
    let c = spiking_board();
    // Debug estate for fixture surgery: a coarse (t, m, spk) trace.
    if std::env::var("HAUKSBEE_FIXTURE_TRACE").is_ok() {
        let m = wf.node(&c, "m").unwrap();
        let s = wf.node(&c, "spk").unwrap();
        let n = wf.time.len();
        eprintln!("samples={n} t_end={:.3e}", wf.time.last().unwrap());
        for k in (0..n).step_by((n / 40).max(1)) {
            eprintln!("t={:.3e} m={:.4} spk={:.4}", wf.time[k], m[k], s[k]);
        }
    }
    let spk = wf.node(&c, "spk").expect("spk waveform");
    let spikes = spk.windows(2).filter(|w| w[0] <= 2.5 && w[1] > 2.5).count();
    // Burst-count band, NOT an exact count: this is a self-resetting loop, so
    // its coarse-grid spike count is integrator-sensitive (the grid study's
    // lesson). Measured refinement study (HAUKSBEE_FIXTURE_DT_MAX): the count
    // CONVERGES to 12 at dt_max 1e-7 under both the raw-second-difference LTE
    // and the divided-difference LTE; at the default coarse dt_max the two
    // estimators counted 13 and 15. The band accepts that whole small-burst
    // class and still catches the real failure modes (a dead oscillator, the
    // BBM servo latch-up, or chatter manufacturing tens of spikes).
    assert!(
        (8..=18).contains(&spikes),
        "spike count {spikes} left the fixture's converged small-burst band \
         (refinement study: converges to 12, coarse grids count 12-15)"
    );
    assert!(
        wf.time.len() > 200,
        "suspiciously few accepted steps ({}): the adaptive march did not do \
         real work",
        wf.time.len()
    );

    let h1 = wf_hash(&wf);
    let h2 = wf_hash(&march());
    assert_eq!(
        h1, h2,
        "two identical marches hashed differently: nondeterminism in the armed path"
    );
    // The cross-commit witness line (run with --nocapture and compare).
    println!(
        "linesearch fixture: wf fnv1a=0x{h1:016x} samples={} spikes={spikes}",
        wf.time.len()
    );
}
