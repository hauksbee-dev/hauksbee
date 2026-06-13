//! Performance smoke tests. Ignored by default (they print, they don't
//! assert); run once with `cargo test --release -- --ignored --nocapture`.
//!
//! The headline is the 1000-stage RC ladder over 10k fixed steps: this is the
//! sparse solver's home turf (a tridiagonal-plus pattern), and the reusable
//! ordering means each step is a cheap numeric refactor + two triangular
//! sweeps. We also print the rectifier wall-clock next to ngspice's for the
//! same circuit (no assertion — environments vary).

use hauksbee_ir::{BjtModel, Circuit, Device, NodeId, Polarity, SourceKind};
use hauksbee_solve::{Integration, Partitioning, SolverOptions, StepControl, Transient};
use std::time::Instant;

/// Build the N-stage RC ladder driven by a DC source from `n0`.
fn rc_ladder(stages: usize) -> Circuit {
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
    c
}

#[test]
#[ignore]
fn rc_ladder_1000_stages_10k_steps() {
    let stages = 1000;
    let steps = 10_000;
    let c = rc_ladder(stages);

    let tstop = 1e-3;
    let dt = tstop / steps as f64;

    let run = |part: Partitioning| {
        let opts = SolverOptions {
            integration: Integration::Trapezoidal,
            step: StepControl::Fixed { dt },
            partitioning: part,
            ..SolverOptions::default()
        };
        let t0 = Instant::now();
        let wf = Transient::new(opts).run(&c, tstop).unwrap();
        (wf, t0.elapsed())
    };

    let (mono, mono_t) = run(Partitioning::Off);
    let (parted, part_t) = run(Partitioning::Auto);

    let n = mono.time.len();
    let mono_sps = n as f64 / mono_t.as_secs_f64();
    let part_sps = parted.time.len() as f64 / part_t.as_secs_f64();

    // Accuracy: final node voltages must match the monolithic reference.
    let mut max_abs = 0.0f64;
    for node in 0..=stages {
        let name = if node == 0 { "n0".to_string() } else { format!("n{node}") };
        let a = mono.final_node(&c, &name).unwrap_or(0.0);
        let b = parted.final_node(&c, &name).unwrap_or(0.0);
        max_abs = max_abs.max((a - b).abs());
    }

    println!("=== RC ladder {stages} stages, {n} fixed steps (same dt) ===");
    println!(
        "  monolithic : {:>9.3?}  => {:>10.0} steps/sec ({:.2} us/step)",
        mono_t, mono_sps, mono_t.as_secs_f64() * 1e6 / n as f64
    );
    println!(
        "  partitioned: {:>9.3?}  => {:>10.0} steps/sec ({:.2} us/step)",
        part_t, part_sps, part_t.as_secs_f64() * 1e6 / n as f64
    );
    println!("  speedup    : {:.2}x", mono_t.as_secs_f64() / part_t.as_secs_f64());
    println!("  max |Δv| partitioned vs monolithic (final): {max_abs:.3e}");
    println!(
        "  NOTE: a single 1000-stage ladder is one large linear island; the sparse"
    );
    println!(
        "  MNA engine is already O(n)/step on its tridiagonal pattern, so Auto keeps"
    );
    println!(
        "  it monolithic. The closed-form exponential wins on MANY SMALL islands"
    );
    println!("  (see synapse_array) and on equal-accuracy-fewer-steps (below).");

    assert!(max_abs < 1e-6, "partitioned diverged from monolithic: {max_abs:.3e}");
    assert!(parted.time.len() >= steps);
}

/// The legitimate closed-form win: a small linear island is exact at *any* step
/// size, so it reaches the monolithic trapezoidal solution's accuracy with far
/// fewer steps. Here a single-pole RC: the exponential path takes 50 giant
/// steps and still matches a 5000-step monolithic run to <1e-3, a 100x step
/// reduction. This is the Tarski trick's actual leverage.
#[test]
#[ignore]
fn small_rc_exact_with_fewer_steps() {
    // V - R - out - C - gnd. tau = 1ms. Settle over 5 tau.
    let mut c = Circuit::new();
    let vin = c.node("in");
    let out = c.node("out");
    c.add(Device::Vsource { name: "V1".into(), p: vin, n: NodeId::GROUND, kind: SourceKind::Dc(1.0) });
    c.add(Device::Resistor { name: "R".into(), a: vin, b: out, ohms: 1e3, tc1: None });
    c.add(Device::Capacitor { name: "C".into(), a: out, b: NodeId::GROUND, farads: 1e-6, ic: Some(0.0) });
    let tau = 1e-3;
    let tstop = 5.0 * tau;

    // Reference: fine monolithic trapezoidal.
    let fine = SolverOptions {
        integration: Integration::Trapezoidal,
        step: StepControl::Fixed { dt: tstop / 5000.0 },
        partitioning: Partitioning::Off,
        ..SolverOptions::default()
    };
    let t0 = Instant::now();
    let ref_wf = Transient::new(fine).run(&c, tstop).unwrap();
    let ref_t = t0.elapsed();
    let ref_final = ref_wf.final_node(&c, "out").unwrap();

    // Coarse partitioned (closed-form exact) — 50 steps only.
    let coarse = SolverOptions {
        integration: Integration::Trapezoidal,
        step: StepControl::Fixed { dt: tstop / 50.0 },
        partitioning: Partitioning::Auto,
        ..SolverOptions::default()
    };
    let t1 = Instant::now();
    let coarse_wf = Transient::new(coarse).run(&c, tstop).unwrap();
    let coarse_t = t1.elapsed();
    let coarse_final = coarse_wf.final_node(&c, "out").unwrap();

    let analytic = 1.0 - (-5.0f64).exp();
    println!("=== small RC: closed-form exact at large step ===");
    println!("  monolithic 5000 steps: {ref_t:.3?}, out={ref_final:.6}");
    println!("  partitioned  50 steps: {coarse_t:.3?}, out={coarse_final:.6}");
    println!("  analytic final       : {analytic:.6}");
    println!("  step reduction       : {}x", 5000 / 50);
    println!("  partitioned err vs analytic: {:.2e}", (coarse_final - analytic).abs());

    // The 50-step closed-form result matches the analytic answer to <1e-3.
    assert!((coarse_final - analytic).abs() < 1e-3, "coarse exp not accurate");
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
    let circuit = hauksbee_ir::SpiceLoader::load(net).unwrap();
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

    // Adaptive run -> Auto transparently falls back to monolithic (the closed-
    // form linear path needs a fixed step), so this is a no-regression check.
    let t0 = Instant::now();
    let wf = Transient::new(opts).run(&circuit, 5e-3).unwrap();
    let ours = t0.elapsed();
    println!(
        "rectifier (hauksbee): {:.3?} for {} steps",
        ours,
        wf.time.len()
    );

    // ngspice wall-clock on the same netlist, if installed.
    let ngspice = "/opt/homebrew/bin/ngspice";
    if std::path::Path::new(ngspice).exists() {
        let path = std::env::temp_dir().join("hauksbee_perf_rect.cir");
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
            "ratio hauksbee/ngspice = {:.3} (lower is faster for us)",
            ours.as_secs_f64() / ng.as_secs_f64()
        );
    } else {
        println!("ngspice not installed; skipping wall-clock comparison");
    }
}

// --- synapse array: the partitioning win ------------------------------------

/// One synaptic block: a pulse drives a voltage switch that gates a reference
/// current into an NPN current mirror; the mirror output charges an RC
/// "membrane". Every block hangs off the shared VCC / ground rails, both
/// source-pinned, so the blocks decouple into independent islands.
struct Synapse {
    netlist: String,
    circuit: Circuit,
    membrane_nodes: Vec<String>,
}

/// Build an `n`-block synapse array as both an in-memory [`Circuit`] and a SPICE
/// `.cir` string (for the ngspice cross-check). Phase-staggered pulses keep the
/// blocks active at different times so the array genuinely exercises coupling.
fn build_synapse_array(n: usize) -> Synapse {
    let mut c = Circuit::new();
    let vcc = c.node("vcc");
    c.add(Device::Vsource {
        name: "VCC".into(),
        p: vcc,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(5.0),
    });

    let mut net = String::from("synapse array\nVCC vcc 0 DC 5\n");

    let model = BjtModel {
        polarity: Polarity::N,
        is: 1e-15,
        bf: 150.0,
        vaf: 80.0,
        nf: 1.0,
        ..BjtModel::default()
    };

    let mut membrane_nodes = Vec::new();

    for k in 0..n {
        // Per-block phase-staggered pulse driving the switch control. Edges are
        // deliberately gentle (10 us) and start after a quiescent settle so a
        // fixed-step run resolves the switch transition over many steps instead
        // of slamming it in one (no event bisection on fixed steps).
        let delay = 30e-6 + 1e-4 * (k as f64 / n as f64);
        let pulse = format!("PULSE(0 3 {:.6e} 10u 10u 50u 200u)", delay);
        let ctrl = c.node(&format!("ctrl{k}"));
        c.add(Device::Vsource {
            name: format!("VP{k}"),
            p: ctrl,
            n: NodeId::GROUND,
            kind: SourceKind::Pulse {
                v1: 0.0,
                v2: 3.0,
                delay,
                rise: 10e-6,
                fall: 10e-6,
                width: 50e-6,
                period: 200e-6,
            },
        });
        net.push_str(&format!("VP{k} ctrl{k} 0 {pulse}\n"));

        // Switch from VCC through to the reference resistor when control is high.
        let sw = c.node(&format!("sw{k}"));
        c.add(Device::VSwitch {
            name: format!("S{k}"),
            a: vcc,
            b: sw,
            ctrl_p: ctrl,
            ctrl_n: NodeId::GROUND,
            von: 2.5,
            voff: 0.5,
            ron: 10.0,
            roff: 1e9,
        });
        net.push_str(&format!("S{k} vcc sw{k} ctrl{k} 0 SMOD\n"));

        // Reference resistor -> diode-connected Q1 (the mirror reference).
        let rref = c.node(&format!("ref{k}"));
        c.add(Device::Resistor {
            name: format!("RR{k}"),
            a: sw,
            b: rref,
            ohms: 10e3,
            tc1: None,
        });
        net.push_str(&format!("RR{k} sw{k} ref{k} 10k\n"));

        // Bleeder keeps the mirror reference node defined when the switch is
        // open (otherwise the diode-connected base floats and Newton stalls).
        c.add(Device::Resistor {
            name: format!("RB{k}"),
            a: rref,
            b: NodeId::GROUND,
            ohms: 1e6,
            tc1: None,
        });
        net.push_str(&format!("RB{k} ref{k} 0 1meg\n"));

        c.add(Device::Bjt {
            name: format!("Q1_{k}"),
            c: rref,
            b: rref,
            e: NodeId::GROUND,
            model,
        });
        net.push_str(&format!("Q1_{k} ref{k} ref{k} 0 QMOD\n"));

        // Mirror output Q2 -> charges the membrane RC.
        let mem = c.node(&format!("mem{k}"));
        c.add(Device::Bjt {
            name: format!("Q2_{k}"),
            c: mem,
            b: rref,
            e: NodeId::GROUND,
            model,
        });
        net.push_str(&format!("Q2_{k} mem{k} ref{k} 0 QMOD\n"));

        // Membrane: pull-up from VCC + capacitor to ground.
        c.add(Device::Resistor {
            name: format!("RM{k}"),
            a: vcc,
            b: mem,
            ohms: 47e3,
            tc1: None,
        });
        net.push_str(&format!("RM{k} vcc mem{k} 47k\n"));
        c.add(Device::Capacitor {
            name: format!("CM{k}"),
            a: mem,
            b: NodeId::GROUND,
            farads: 1e-9,
            ic: Some(5.0),
        });
        net.push_str(&format!("CM{k} mem{k} 0 1n IC=5\n"));

        membrane_nodes.push(format!("mem{k}"));
    }

    net.push_str(".model SMOD SW(VT=1.5 VH=1.0 RON=10 ROFF=1e9)\n");
    net.push_str(".model QMOD NPN(IS=1e-15 BF=150 VAF=80 NF=1.0)\n");
    net.push_str(".tran 1u 400u uic\n");
    net.push_str(".print tran v(mem0)\n");
    net.push_str(".options reltol=1e-4\n.end\n");

    Synapse { netlist: net, circuit: c, membrane_nodes }
}

#[test]
#[ignore]
fn synapse_array_partitioned_vs_monolithic() {
    let blocks = 90;
    let syn = build_synapse_array(blocks);
    let c = &syn.circuit;
    let tstop = 400e-6;
    let dt = 1e-6;

    let run = |part: Partitioning| {
        let opts = SolverOptions {
            integration: Integration::Trapezoidal,
            step: StepControl::Fixed { dt },
            reltol: 1e-4,
            // The cold-start switch transitions are stiff for a global Newton;
            // give it room and a small gmin floor. The partitioned path needs
            // far less of both since each block solves on its own.
            max_newton: 200,
            gmin: 1e-9,
            partitioning: part,
            ..SolverOptions::default()
        };
        let t0 = Instant::now();
        let wf = Transient::new(opts).run(c, tstop).unwrap();
        (wf, t0.elapsed())
    };

    let (mono, mono_t) = run(Partitioning::Off);
    let (parted, part_t) = run(Partitioning::Auto);

    // Accuracy: membrane voltages, partitioned vs monolithic.
    let mut max_rel = 0.0f64;
    for name in &syn.membrane_nodes {
        let a = mono.final_node(c, name).unwrap_or(0.0);
        let b = parted.final_node(c, name).unwrap_or(0.0);
        let denom = a.abs().max(0.1);
        max_rel = max_rel.max((a - b).abs() / denom);
    }

    println!("=== synapse array: {blocks} blocks, {} fixed steps ===", mono.time.len());
    println!("  monolithic : {mono_t:.3?}");
    println!("  partitioned: {part_t:.3?}");
    println!("  speedup    : {:.2}x", mono_t.as_secs_f64() / part_t.as_secs_f64());
    println!("  max rel error (membranes, partitioned vs monolithic): {:.3e}", max_rel);

    // ngspice on the same generated netlist.
    let ngspice = "/opt/homebrew/bin/ngspice";
    if std::path::Path::new(ngspice).exists() {
        let path = std::env::temp_dir().join("hauksbee_synapse.cir");
        std::fs::write(&path, &syn.netlist).unwrap();
        let t1 = Instant::now();
        let _ = std::process::Command::new(ngspice).arg("-b").arg(&path).output().unwrap();
        let ng = t1.elapsed();
        println!("  ngspice -b (incl. process start): {ng:.3?}");
        println!("  hauksbee partitioned / ngspice = {:.3}", part_t.as_secs_f64() / ng.as_secs_f64());
        // Leave the .cir on disk for inspection.
        println!("  netlist written to {}", path.display());
    } else {
        println!("  ngspice not installed; skipping cross-check");
    }

    assert!(max_rel < 5e-3, "partitioned synapse diverged: {max_rel:.3e}");
}

// --- accuracy: partitioned vs monolithic on ngspice circuits ----------------

#[test]
#[ignore]
fn accuracy_partitioned_vs_monolithic() {
    // Fixed-step versions of the ngspice cross-check circuits, comparing the
    // partitioned engine against the monolithic reference. Max rel error target
    // < 0.5%.
    let cases: &[(&str, &str, f64, f64)] = &[
        (
            "rc_ladder20",
            {
                // 20-stage ladder, generated inline.
                "GEN_LADDER20"
            },
            2e-3,
            1e-6,
        ),
    ];

    for (name, spec, tstop, dt) in cases {
        let c = if *spec == "GEN_LADDER20" {
            let mut c = Circuit::new();
            let n0 = c.node("n0");
            c.add(Device::Vsource {
                name: "V1".into(),
                p: n0,
                n: NodeId::GROUND,
                kind: SourceKind::Pulse {
                    v1: 0.0, v2: 1.0, delay: 0.0, rise: 1e-9, fall: 1e-9, width: 1.0, period: 0.0,
                },
            });
            let mut prev = n0;
            for i in 0..20 {
                let next = c.node(&format!("n{}", i + 1));
                c.add(Device::Resistor { name: format!("R{}", i+1), a: prev, b: next, ohms: 1e3, tc1: None });
                c.add(Device::Capacitor { name: format!("C{}", i+1), a: next, b: NodeId::GROUND, farads: 10e-9, ic: Some(0.0) });
                prev = next;
            }
            c
        } else {
            hauksbee_ir::SpiceLoader::load(spec).unwrap()
        };

        let run = |part: Partitioning| {
            let opts = SolverOptions {
                integration: Integration::Trapezoidal,
                step: StepControl::Fixed { dt: *dt },
                reltol: 1e-4,
                partitioning: part,
                ..SolverOptions::default()
            };
            Transient::new(opts).run(&c, *tstop).unwrap()
        };
        let mono = run(Partitioning::Off);
        let parted = run(Partitioning::Auto);

        let mut max_rel = 0.0f64;
        for node in 0..c.node_count() {
            let nm = c.node_name(NodeId(node as u32)).to_string();
            let a = mono.node(&c, &nm).map(|w| *w.last().unwrap()).unwrap_or(0.0);
            let b = parted.node(&c, &nm).map(|w| *w.last().unwrap()).unwrap_or(0.0);
            let denom = a.abs().max(0.05);
            max_rel = max_rel.max((a - b).abs() / denom);
        }
        println!("accuracy [{name}]: max rel error partitioned vs monolithic = {:.3e}", max_rel);
        assert!(max_rel < 5e-3, "{name} exceeded 0.5%: {max_rel:.3e}");
    }
}
