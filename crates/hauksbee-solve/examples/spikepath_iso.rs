//! Isolated one-neuron spike path from the Tarski InputSystem board.
//!
//! This is the fast debug loop for the transient-convergence goal kernel: get a
//! single hidden-neuron spike path to converge THROUGH the comparator-flip
//! event so a real analog stretched pulse forms on `V_out`.
//!
//! Topology and values are transcribed from the InputSystem netlist around
//! `/Neuron_Layer1/Neuron9` (refs *2601/*2602) plus the SURGERY.md rework:
//!
//!   +IN  (membrane)  = N_OUT9  -> driven here by a controllable ramp source
//!   -IN  (threshold) :  R_top 820k -> ANALOG_VDD, R_bottom 150k -> GND,
//!                       C_adapt2601 4.7nF -> GND, R_inject 47k -> adapt2 node
//!   OUT  -> R_charge 1k -> D_stretch2601(A) ; D_stretch2601 K = V_out
//!   V_out:  C__stretch 5.8nF -> GND (surgered from 10pF),
//!           R__stretch 150k -> GND (leak/decay),
//!           D_stretch2602 A = V_out, K = adapt2 node,
//!           (the synapse-mirror analog-switch S pins it drives are high-Z: a
//!            large resistor to GND approximates that load)
//!   adapt2 node (D_stretch2602-K):  C_adapt2602 150pF -> adapt2_pad node
//!   adapt2_pad node (C_adapt2602-Pad2): R_inject 47k -> -IN
//!
//! The comparator is the LMV7219 (out_lo 0.05, out_hi 4.95, hysteresis from the
//! datasheet). At power-on the membrane sits below threshold and V_out is a
//! diode+cap-only node with no DC value (V_out = 0, diodes off) -- that is the
//! correct power-on state. The SPIKE is the transient event: drive the membrane
//! above threshold, the comparator flips HIGH, D_stretch2601 forward-biases and
//! charges C__stretch, and V_out rises into a stretched pulse that then decays
//! with tau ~ R__stretch * C__stretch.

use hauksbee_ir::{Circuit, Device, DiodeModel, NodeId, PwlPoint, SourceKind};
use hauksbee_solve::{Integration, Partitioning, SolverOptions, StepControl, Transient};

/// 1N4148 small-signal switching diode (the surgered D_stretch part).
fn diode_1n4148() -> DiodeModel {
    DiodeModel {
        is: 4.352e-9,
        n: 1.906,
        rs: 0.6458,
        cjo: 7.048e-13,
        vj: 0.869,
        m: 0.0306,
        tt: 3.48e-9,
        bv: 110.0,
        ..DiodeModel::default()
    }
}

/// Build the isolated spike path. `membrane`: PWL drive on +IN (the hidden
/// neuron's membrane voltage). Returns the circuit.
fn build_spike_path(membrane: Vec<PwlPoint>) -> Circuit {
    let mut c = Circuit::new();

    // Nets (named to match the board).
    let vdd = c.node("ANALOG_VDD");
    let mem = c.node("N_OUT9"); // comparator +IN, the membrane
    let thr = c.node("CMP_IN_MINUS"); // comparator -IN, the threshold node
    let cmp_out = c.node("CMP_OUT");
    let d_a = c.node("D_STRETCH2601_A"); // R_charge -> diode anode
    let vout = c.node("V_OUT");
    let adapt2 = c.node("D_STRETCH2602_K"); // adaptation-injection node
    let adapt2_pad = c.node("C_ADAPT2602_PAD2");

    // --- supply rail (ideal 5V, the board's regulated ANALOG_VDD) ---
    c.add(Device::Vsource {
        name: "VDD".into(),
        p: vdd,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(5.0),
    });

    // --- membrane drive on +IN ---
    c.add(Device::Vsource {
        name: "VMEM".into(),
        p: mem,
        n: NodeId::GROUND,
        kind: SourceKind::Pwl(membrane),
    });

    // --- threshold divider + adaptation on -IN ---
    c.add(Device::Resistor {
        name: "R_top2601".into(),
        a: vdd,
        b: thr,
        ohms: 820e3,
        tc1: None,
    });
    c.add(Device::Resistor {
        name: "R_bottom2601".into(),
        a: thr,
        b: NodeId::GROUND,
        ohms: 150e3,
        tc1: None,
    });
    c.add(Device::Capacitor {
        name: "C_adapt2601".into(),
        a: thr,
        b: NodeId::GROUND,
        farads: 4.7e-9,
        ic: None,
    });
    c.add(Device::Resistor {
        name: "R_inject2601".into(),
        a: thr,
        b: adapt2_pad,
        ohms: 47e3,
        tc1: None,
    });

    // --- comparator (LMV7219) ---
    c.add(Device::Comparator {
        name: "NEURON_COMPARATOR2601".into(),
        out: cmp_out,
        inp: mem,
        inn: thr,
        out_lo: 0.05,
        out_hi: 4.95,
        hysteresis: 0.003,
    });

    // --- charge path: OUT -> R_charge -> D_stretch2601 -> V_out ---
    c.add(Device::Resistor {
        name: "R_charge2601".into(),
        a: cmp_out,
        b: d_a,
        ohms: 1.0e3,
        tc1: None,
    });
    c.add(Device::Diode {
        name: "D_stretch2601".into(),
        a: d_a,
        k: vout,
        model: diode_1n4148(),
    });

    // --- V_out node: stretch cap (surgered 5.8nF) + leak + hi-Z load ---
    c.add(Device::Capacitor {
        name: "C__stretch2601".into(),
        a: vout,
        b: NodeId::GROUND,
        farads: 5.8e-9,
        ic: None,
    });
    c.add(Device::Resistor {
        name: "R__stretch2601".into(),
        a: vout,
        b: NodeId::GROUND,
        ohms: 150e3,
        tc1: None,
    });
    // The synapse-mirror analog-switch source pins V_out drives are high
    // impedance at rest; approximate that aggregate load by a large R to GND.
    c.add(Device::Resistor {
        name: "R_mirror_load".into(),
        a: vout,
        b: NodeId::GROUND,
        ohms: 1.0e6,
        tc1: None,
    });

    // --- adaptation feedback: V_out -> D_stretch2602 -> C_adapt2602 -> R_inject ---
    c.add(Device::Diode {
        name: "D_stretch2602".into(),
        a: vout,
        k: adapt2,
        model: diode_1n4148(),
    });
    c.add(Device::Capacitor {
        name: "C_adapt2602".into(),
        a: adapt2,
        b: adapt2_pad,
        farads: 150e-12,
        ic: None,
    });

    c
}

fn node_trace(circuit: &Circuit, wf: &hauksbee_solve::Waveforms, name: &str) -> Vec<f64> {
    wf.node(circuit, name)
        .map(|s| s.to_vec())
        .unwrap_or_default()
}

fn main() {
    // Membrane drive. Default: hold below threshold, ramp through it, hold high,
    // drop back below so we exercise both flip directions (hysteresis).
    // Threshold divider sets the base -IN ~ 5 * 150/(820+150) = 0.773 V.
    //
    // ISO_START_HIGH=1: start the membrane ABOVE threshold at t=0, so the DC
    // operating point itself must seed the comparator HIGH while V_out (a
    // cap-isolated, DC-rootless node) has no static value -- the exact ambiguous
    // power-on the full board hits.
    let start_high = std::env::var("ISO_START_HIGH")
        .map(|v| v == "1")
        .unwrap_or(false);
    let membrane = if start_high {
        vec![
            PwlPoint { t: 0.0, v: 1.50 },
            PwlPoint {
                t: 2000e-6,
                v: 1.50,
            },
        ]
    } else {
        vec![
            PwlPoint { t: 0.0, v: 0.30 },
            PwlPoint { t: 100e-6, v: 0.30 },
            PwlPoint { t: 300e-6, v: 1.50 }, // ramp across threshold (~0.77V)
            PwlPoint { t: 900e-6, v: 1.50 },
            PwlPoint {
                t: 1000e-6,
                v: 0.30,
            }, // drop back below
            PwlPoint {
                t: 2000e-6,
                v: 0.30,
            },
        ]
    };

    let circuit = build_spike_path(membrane);

    let mut opts = SolverOptions::adaptive(1e-7, 5e-6);
    // Integration + partitioning configurable so we can match the engine's exact
    // defaults (Trapezoidal + Auto) and isolate which one (if any) breaks the
    // per-step convergence on the board.
    opts.integration = match std::env::var("ISO_INTEG").as_deref() {
        Ok("trapz") => Integration::Trapezoidal,
        Ok("be") => Integration::BackwardEuler,
        _ => Integration::Gear2,
    };
    opts.partitioning = match std::env::var("ISO_PART").as_deref() {
        Ok("auto") => Partitioning::Auto,
        _ => Partitioning::Off,
    };
    opts.step = StepControl::Adaptive {
        dt_initial: 1e-7,
        dt_min: 1e-12,
        dt_max: 1e-4,
    };

    eprintln!("[iso] running isolated spike-path transient to 2 ms ...");
    let res = Transient::new(opts).run(&circuit, 2000e-6);

    match res {
        Ok(wf) => {
            let t = &wf.time;
            let vmem = node_trace(&circuit, &wf, "N_OUT9");
            let vthr = node_trace(&circuit, &wf, "CMP_IN_MINUS");
            let vcmp = node_trace(&circuit, &wf, "CMP_OUT");
            let vout = node_trace(&circuit, &wf, "V_OUT");

            let vout_peak = vout.iter().cloned().fold(0.0f64, f64::max);
            let n = t.len();
            eprintln!(
                "[iso] CONVERGED: {n} accepted steps to t={:.3e}s",
                t.last().copied().unwrap_or(0.0)
            );
            eprintln!("[iso] V_OUT peak = {vout_peak:.4} V");

            println!("# t(s)\tVmem\tVthr\tCMP_OUT\tV_OUT");
            // Print a downsampled trace so the waveform is legible.
            let stride = (n / 60).max(1);
            for i in (0..n).step_by(stride) {
                println!(
                    "{:.6e}\t{:.4}\t{:.4}\t{:.4}\t{:.4}",
                    t[i], vmem[i], vthr[i], vcmp[i], vout[i]
                );
            }
            if vout_peak > 0.1 {
                eprintln!("[iso] SPIKE FORMED (V_OUT peak {vout_peak:.3} V > 0.1 V)");
            } else {
                eprintln!("[iso] NO SPIKE: V_OUT stayed at {vout_peak:.3e} V");
            }
        }
        Err(e) => {
            eprintln!("[iso] TRANSIENT FAILED: {e}");
            std::process::exit(1);
        }
    }
}
