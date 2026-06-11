//! Performance smoke tests. Ignored by default (they print, they don't
//! assert); run once with `cargo test --release -- --ignored --nocapture`.
//!
//! The headline is the 1000-stage RC ladder over 10k fixed steps: this is the
//! sparse solver's home turf (a tridiagonal-plus pattern), and the reusable
//! ordering means each step is a cheap numeric refactor + two triangular
//! sweeps. We also print the rectifier wall-clock next to ngspice's for the
//! same circuit (no assertion — environments vary).

use galvani_ir::{Circuit, Device, NodeId, SourceKind};
use galvani_solve::{Integration, SolverOptions, StepControl, Transient};
use std::time::Instant;

#[test]
#[ignore]
fn rc_ladder_1000_stages_10k_steps() {
    let stages = 1000;
    let steps = 10_000;

    let mut c = Circuit::new();
    let n0 = c.node("n0");
    c.add(Device::Vsource {
        name: "V1".into(),
        p: n0,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(1.0),
    });
    let mut prev = n0;
    for i in 0..stages {
        let next = c.node(&format!("n{}", i + 1));
        c.add(Device::Resistor {
            name: format!("R{}", i + 1),
            a: prev,
            b: next,
            ohms: 1e3,
            tc1: None,
        });
        c.add(Device::Capacitor {
            name: format!("C{}", i + 1),
            a: next,
            b: NodeId::GROUND,
            farads: 1e-9,
            ic: Some(0.0),
        });
        prev = next;
    }

    let tstop = 1e-3;
    let dt = tstop / steps as f64;
    let opts = SolverOptions {
        integration: Integration::Trapezoidal,
        step: StepControl::Fixed { dt },
        ..SolverOptions::default()
    };

    let t0 = Instant::now();
    let wf = Transient::new(opts).run(&c, tstop).unwrap();
    let elapsed = t0.elapsed();

    let actual_steps = wf.time.len();
    let sps = actual_steps as f64 / elapsed.as_secs_f64();
    println!(
        "RC ladder {stages} stages, {actual_steps} steps in {:.3?} => {:.0} steps/sec ({:.1} us/step, {} unknowns)",
        elapsed,
        sps,
        elapsed.as_secs_f64() * 1e6 / actual_steps as f64,
        stages + 1,
    );
    assert!(actual_steps >= steps, "expected ~{steps} steps");
}

#[test]
#[ignore]
fn rectifier_wallclock_vs_ngspice() {
    // Same half-wave rectifier as the cross-check, timed end to end.
    let net = "\
halfwave
V1 in 0 SIN(0 5 1k 0 0 0)
D1 in out DMOD
R1 out 0 10k
C1 out 0 10u
.model DMOD D(IS=1e-14 N=1.0 RS=0.1)
.tran 1u 5m uic
.print tran v(out)
.options reltol=1e-4
.end
";
    let circuit = galvani_ir::SpiceLoader::load(net).unwrap();
    let opts = SolverOptions {
        integration: Integration::Trapezoidal,
        step: StepControl::Adaptive {
            dt_initial: 1e-7,
            dt_min: 1e-12,
            dt_max: 5e-6,
        },
        reltol: 1e-4,
        ..SolverOptions::default()
    };

    let t0 = Instant::now();
    let wf = Transient::new(opts).run(&circuit, 5e-3).unwrap();
    let ours = t0.elapsed();
    println!(
        "rectifier (galvani): {:.3?} for {} steps",
        ours,
        wf.time.len()
    );

    // ngspice wall-clock on the same netlist, if installed.
    let ngspice = "/opt/homebrew/bin/ngspice";
    if std::path::Path::new(ngspice).exists() {
        let path = std::env::temp_dir().join("galvani_perf_rect.cir");
        std::fs::write(&path, net).unwrap();
        let t1 = Instant::now();
        let _ = std::process::Command::new(ngspice)
            .arg("-b")
            .arg(&path)
            .output()
            .unwrap();
        let ng = t1.elapsed();
        let _ = std::fs::remove_file(&path);
        println!("rectifier (ngspice -b, incl. process start): {ng:.3?}");
        println!(
            "ratio galvani/ngspice = {:.3} (lower is faster for us)",
            ours.as_secs_f64() / ng.as_secs_f64()
        );
    } else {
        println!("ngspice not installed; skipping wall-clock comparison");
    }
}
