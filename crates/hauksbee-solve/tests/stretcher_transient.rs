//! Pulse-stretcher transient convergence regressions.
//!
//! This mirrors the Tarski neuron stretcher core around one `/V_out` node:
//! comparator output -> R_charge -> D_stretch -> V_out, the opposing stretcher
//! diode into an adaptation capacitor, and the large stretch capacitor/leak.

use hauksbee_ir::{Circuit, Device, DiodeModel, NodeId, PwlPoint, SourceKind};
use hauksbee_solve::{Integration, Partitioning, SolverOptions, StepControl, Transient};

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

fn build_stretcher_core() -> Circuit {
    let mut c = Circuit::new();
    let cmp_out = c.node("CMP_OUT");
    let charge = c.node("D_STRETCH_A");
    let vout = c.node("V_OUT");
    let adapt_k = c.node("D_STRETCH_K");
    let adapt_ref = c.node("ADAPT_REF");
    let mem = c.node("MEM");
    let thr = c.node("THR");

    c.add(Device::Vsource {
        name: "VMEM".into(),
        p: mem,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(1.0),
    });
    c.add(Device::Vsource {
        name: "VTHR".into(),
        p: thr,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(0.2),
    });
    c.add(Device::Comparator {
        name: "UCMP".into(),
        out: cmp_out,
        inp: mem,
        inn: thr,
        out_lo: 0.0,
        out_hi: 5.0,
        hysteresis: 0.003,
    });

    c.add(Device::Resistor {
        name: "RCHARGE".into(),
        a: cmp_out,
        b: charge,
        ohms: 1.0e3,
        tc1: None,
    });
    c.add(Device::Diode {
        name: "DCHARGE".into(),
        a: charge,
        k: vout,
        model: diode_1n4148(),
    });
    c.add(Device::Diode {
        name: "DADAPT".into(),
        a: vout,
        k: adapt_k,
        model: diode_1n4148(),
    });
    c.add(Device::Capacitor {
        name: "CADAPT".into(),
        a: adapt_k,
        b: adapt_ref,
        farads: 150e-12,
        ic: Some(0.0),
    });
    c.add(Device::Vsource {
        name: "VADAPT_REF".into(),
        p: adapt_ref,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(0.0),
    });

    c.add(Device::Capacitor {
        name: "CSTRETCH".into(),
        a: vout,
        b: cmp_out,
        farads: 5.8e-9,
        ic: None,
    });
    c.add(Device::Resistor {
        name: "RSTRETCH".into(),
        a: vout,
        b: NodeId::GROUND,
        ohms: 150e3,
        tc1: None,
    });
    c
}

#[test]
fn fired_comparator_charges_stretch_output_on_first_transient_step() {
    let mut opts = SolverOptions::fixed(1e-9);
    opts.integration = Integration::BackwardEuler;
    opts.partitioning = Partitioning::Off;
    opts.step = StepControl::Fixed { dt: 1e-9 };

    let circuit = build_stretcher_core();
    let wf = Transient::new(opts)
        .run(&circuit, 20e-9)
        .expect("stretcher transient should converge");
    let vout = wf.final_node(&circuit, "V_OUT").expect("V_OUT waveform");

    assert!(
        vout > 0.1,
        "V_OUT did not charge from fired comparator: {vout:.6e} V"
    );
}

/// Full one-neuron spike path transcribed from the Tarski InputSystem netlist
/// around `/Neuron_Layer1/Neuron9` (refs *2601/*2602) plus the SURGERY.md rework
/// (stretch cap 10pF -> 5.8nF). Unlike the test above, the comparator starts
/// BELOW threshold and the membrane is ramped THROUGH it during the transient,
/// so the step Newton must converge across the comparator-flip event and form a
/// real analog stretched pulse. The component values and connectivity are the
/// raw-netlist truth (stretch cap and leak referenced to GND).
fn build_neuron9_spike_path(membrane: Vec<PwlPoint>) -> Circuit {
    let mut c = Circuit::new();
    let vdd = c.node("ANALOG_VDD");
    let mem = c.node("N_OUT9"); // comparator +IN (membrane)
    let thr = c.node("CMP_IN_MINUS"); // comparator -IN (threshold)
    let cmp_out = c.node("CMP_OUT");
    let d_a = c.node("D_STRETCH2601_A");
    let vout = c.node("V_OUT");
    let adapt2 = c.node("D_STRETCH2602_K");
    let adapt2_pad = c.node("C_ADAPT2602_PAD2");

    c.add(Device::Vsource {
        name: "VDD".into(),
        p: vdd,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(5.0),
    });
    c.add(Device::Vsource {
        name: "VMEM".into(),
        p: mem,
        n: NodeId::GROUND,
        kind: SourceKind::Pwl(membrane),
    });

    // Threshold divider + adaptation on -IN.
    c.add(Device::Resistor { name: "R_top2601".into(), a: vdd, b: thr, ohms: 820e3, tc1: None });
    c.add(Device::Resistor { name: "R_bottom2601".into(), a: thr, b: NodeId::GROUND, ohms: 150e3, tc1: None });
    c.add(Device::Capacitor { name: "C_adapt2601".into(), a: thr, b: NodeId::GROUND, farads: 4.7e-9, ic: None });
    c.add(Device::Resistor { name: "R_inject2601".into(), a: thr, b: adapt2_pad, ohms: 47e3, tc1: None });

    // LMV7219 comparator (datasheet rails 0.05/4.95 V, 3 mV offset).
    c.add(Device::Comparator {
        name: "NEURON_COMPARATOR2601".into(),
        out: cmp_out, inp: mem, inn: thr,
        out_lo: 0.05, out_hi: 4.95, hysteresis: 0.003,
    });

    // Charge path OUT -> R_charge -> D_stretch -> V_out.
    c.add(Device::Resistor { name: "R_charge2601".into(), a: cmp_out, b: d_a, ohms: 1.0e3, tc1: None });
    c.add(Device::Diode { name: "D_stretch2601".into(), a: d_a, k: vout, model: diode_1n4148() });

    // V_out: stretch cap (surgered 5.8nF) + leak, both to GND; plus a hi-Z load
    // for the synapse-mirror analog-switch source pins.
    c.add(Device::Capacitor { name: "C__stretch2601".into(), a: vout, b: NodeId::GROUND, farads: 5.8e-9, ic: None });
    c.add(Device::Resistor { name: "R__stretch2601".into(), a: vout, b: NodeId::GROUND, ohms: 150e3, tc1: None });
    c.add(Device::Resistor { name: "R_mirror_load".into(), a: vout, b: NodeId::GROUND, ohms: 1.0e6, tc1: None });

    // Adaptation feedback V_out -> D_stretch2602 -> C_adapt2602 -> R_inject.
    c.add(Device::Diode { name: "D_stretch2602".into(), a: vout, k: adapt2, model: diode_1n4148() });
    c.add(Device::Capacitor { name: "C_adapt2602".into(), a: adapt2, b: adapt2_pad, farads: 150e-12, ic: None });
    c
}

#[test]
fn neuron_spike_forms_through_comparator_flip_event() {
    // Membrane below threshold (~0.77 V divider), ramped through it, held high,
    // then dropped back below to exercise both flip directions.
    let membrane = vec![
        PwlPoint { t: 0.0, v: 0.30 },
        PwlPoint { t: 100e-6, v: 0.30 },
        PwlPoint { t: 300e-6, v: 1.50 },
        PwlPoint { t: 900e-6, v: 1.50 },
        PwlPoint { t: 1000e-6, v: 0.30 },
        PwlPoint { t: 2000e-6, v: 0.30 },
    ];
    let circuit = build_neuron9_spike_path(membrane);

    let mut opts = SolverOptions::adaptive(1e-7, 1e-4);
    opts.integration = Integration::Gear2;
    opts.partitioning = Partitioning::Off;
    opts.step = StepControl::Adaptive { dt_initial: 1e-7, dt_min: 1e-12, dt_max: 1e-4 };

    let wf = Transient::new(opts)
        .run(&circuit, 2000e-6)
        .expect("spike-path transient must converge through the comparator flip");

    let vout = wf.node(&circuit, "V_OUT").expect("V_OUT waveform");
    let time = &wf.time;

    // Before the membrane crosses threshold (t < 150 us) V_out is the discharged
    // power-on state.
    let early = time.iter().zip(vout).find(|(t, _)| **t > 120e-6).map(|(_, v)| *v).unwrap();
    assert!(early.abs() < 0.05, "V_out should rest near 0 before the flip: {early:.4} V");

    // After the comparator fires, a real stretched pulse forms (rails near the
    // comparator high minus a diode drop).
    let peak = vout.iter().cloned().fold(0.0f64, f64::max);
    assert!(peak > 3.5, "no stretched spike formed: peak {peak:.4} V");

    // Once the membrane drops the comparator releases and V_out decays through
    // R__stretch||R_mirror * C__stretch (tau ~ 0.76 ms), i.e. it is FALLING but
    // not yet fully discharged by t = 2 ms.
    let v_release = time.iter().zip(vout).find(|(t, _)| **t > 1100e-6).map(|(_, v)| *v).unwrap();
    let v_final = *vout.last().unwrap();
    assert!(v_final < v_release, "V_out should be decaying after release: {v_release:.3} -> {v_final:.3}");
    assert!(v_final > 0.5, "decay too fast vs the R*C stretch time constant: {v_final:.3} V");
}
