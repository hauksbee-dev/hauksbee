//! Solve-side gates for `.ic` (transient initial conditions under `uic`) and
//! `.nodeset` (DC Newton start-vector seed), SPICE-compat plan §4.1 / step 10.
//!
//! These exercise the seeding hooks the loader threads onto the [`Circuit`]:
//! `.ic` node voltages seed the `FromZero` (uic) power-on start and the reactive
//! state; `.nodeset` seeds the DC cold-start vector as a convergence GUESS that
//! is never pinned.

use hauksbee_ir::SpiceLoader;
use hauksbee_solve::{run_op, run_tran, DcInit, Integration, Probe, SolverOptions, StepControl};

/// Build transient options from a deck's directives, the same shape the CLI and
/// ngspice harness use (adaptive step; `uic` -> power-on `FromZero`).
fn tran_opts(td: hauksbee_ir::TranDirective, uic: bool) -> SolverOptions {
    let mut opts = SolverOptions::default();
    let dt_max = td.tmax.unwrap_or(td.tstep).max(1e-15);
    opts.integration = Integration::Trapezoidal;
    opts.step = StepControl::Adaptive {
        dt_initial: (td.tstep / 100.0).max(1e-15),
        dt_min: 1e-15,
        dt_max,
    };
    if uic {
        opts.dc_init = DcInit::FromZero;
    }
    opts
}

#[test]
fn ic_uic_seeds_rc_initial_voltage_and_decays() {
    // A capacitor pre-charged via `.ic V(out)=5`, discharging through R with
    // tau = R*C = 1k * 1u = 1 ms. Under uic there is no DC solve: out starts at
    // 5 V and decays. The observable is the exponential tail.
    let net = "rc ic decay\n\
               C1 out 0 1u\n\
               R1 out 0 1k\n\
               .ic V(out)=5\n\
               .tran 10u 3m uic\n\
               .end\n";
    let (circuit, directives) = SpiceLoader::load_with_directives(net).unwrap();
    assert!(directives.use_initial_conditions, "deck carries uic");
    let opts = tran_opts(directives.tran.unwrap(), directives.use_initial_conditions);
    let out = run_tran(
        &circuit,
        &opts,
        directives.tran.unwrap().tstop,
        &[Probe::NodeVoltage("out".into())],
    )
    .expect("transient runs");
    let v = out.column("V(out)").unwrap();
    let t = out.time.clone().unwrap();

    // Starts at the initial condition, not zero.
    assert!(
        (v[0] - 5.0).abs() < 0.05,
        "t=0 initial condition ~5V, got {}",
        v[0]
    );
    // Monotonically decays toward zero.
    assert!(
        *v.last().unwrap() < 1.0,
        "decays well below 5V by 3ms, got {}",
        v.last().unwrap()
    );
    // At one time constant (~1 ms) the RC tail is ~5*e^-1 = 1.84 V.
    let i_tau = t.iter().position(|&x| x >= 1e-3).unwrap();
    let expected = 5.0 * (-1.0f64).exp();
    assert!(
        (v[i_tau] - expected).abs() < 0.4,
        "at t≈tau expect ~{expected:.2}V, got {:.3}V",
        v[i_tau]
    );
}

#[test]
fn nodeset_selects_bistable_state_but_does_not_pin() {
    // Cross-coupled NMOS inverters with resistor loads: a bistable latch. Its DC
    // has two stable roots (qa high / qb low, or the mirror). `.nodeset` seeds
    // the DC start vector, so it SELECTS which root Newton finds, but nothing
    // is pinned, so the settled voltages differ from the seed.
    let latch = |ns: &str| -> (f64, f64) {
        let net = format!(
            "latch\n\
             Vdd vdd 0 5\n\
             R1 vdd qa 10k\n\
             R2 vdd qb 10k\n\
             M1 qa qb 0 0 NM\n\
             M2 qb qa 0 0 NM\n\
             .model NM NMOS(VTO=1 KP=5e-3)\n\
             {ns}\n\
             .op\n.end\n"
        );
        let c = SpiceLoader::load(&net).unwrap();
        let opts = SolverOptions::default();
        let out = run_op(
            &c,
            &opts,
            &[
                Probe::NodeVoltage("qa".into()),
                Probe::NodeVoltage("qb".into()),
            ],
        )
        .expect("op converges");
        (
            out.column("V(qa)").unwrap()[0],
            out.column("V(qb)").unwrap()[0],
        )
    };

    // Seed qa toward the high state (but at 3 V, a value that is NOT the
    // solution: the settled qa is ~5 V, since M1 turns off and R1 pulls qa to
    // the rail). The seed picks the "qa wins" basin; convergence then walks qa
    // AWAY from the 3 V seed, proving the seed is a guess, not a pin.
    let (qa1, qb1) = latch(".nodeset V(qa)=3 V(qb)=0");
    assert!(
        qa1 > qb1 + 1.0,
        "seed qa-high selects qa>qb: qa={qa1:.3} qb={qb1:.3}"
    );
    assert!(
        (qa1 - 3.0).abs() > 0.5,
        "NOT pinned: qa converged away from the 3V seed to {qa1:.4}"
    );

    // Mirror seed -> the mirror state. Same equations; the seed alone flips the
    // root Newton lands on.
    let (qa2, qb2) = latch(".nodeset V(qa)=0 V(qb)=3");
    assert!(
        qb2 > qa2 + 1.0,
        "seed qb-high selects qb>qa: qa={qa2:.3} qb={qb2:.3}"
    );
}
