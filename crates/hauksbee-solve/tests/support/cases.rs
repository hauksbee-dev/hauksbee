//! The two marketed benchmark circuits, built once and shared.
//!
//! In a `tests/` subdirectory so cargo does not compile it as its own test
//! target: it is a fixture, not a check. Shared so the gate and any probe time
//! and compare literally the same circuits.

#![allow(dead_code)]

use hauksbee_ir::{BjtModel, Circuit, Device, NodeId, Polarity, SourceKind};
use hauksbee_solve::{Integration, Partitioning, SolverOptions, StepControl};

// --- the rectifier -----------------------------------------------------------

pub const RECTIFIER_DECK: &str = "\
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

pub fn rectifier_opts() -> SolverOptions {
    SolverOptions {
        integration: Integration::Trapezoidal,
        step: StepControl::Adaptive {
            dt_initial: 1e-7,
            dt_min: 1e-12,
            dt_max: 5e-6,
        },
        reltol: 1e-4,
        ..SolverOptions::default()
    }
}

// --- the synapse array -------------------------------------------------------

/// An `n`-block synapse array as both a [`Circuit`] and the equivalent `.cir`,
/// built in one pass so the two representations cannot drift apart.
///
/// Each block: a phase-staggered pulse drives a voltage switch that gates a
/// reference current into an NPN mirror, whose output charges an RC membrane.
/// Every block hangs off the shared, source-pinned VCC rail, which is what makes
/// the array decompose into independent islands.
pub struct Synapse {
    pub netlist: String,
    pub circuit: Circuit,
}

pub fn build_synapse_array(n: usize) -> Synapse {
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

    for k in 0..n {
        // Gentle 10 µs edges after a quiescent settle, so a fixed-step run
        // resolves the switch transition over many steps rather than slamming it
        // into one (there is no event bisection on fixed steps).
        let delay = 30e-6 + 1e-4 * (k as f64 / n as f64);
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
        net.push_str(&format!(
            "VP{k} ctrl{k} 0 PULSE(0 3 {delay:.6e} 10u 10u 50u 200u)\n"
        ));

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

        let rref = c.node(&format!("ref{k}"));
        c.add(Device::Resistor {
            name: format!("RR{k}"),
            a: sw,
            b: rref,
            ohms: 10e3,
            tc1: None,
        });
        net.push_str(&format!("RR{k} sw{k} ref{k} 10k\n"));

        // Bleeder: keeps the mirror reference defined while the switch is open,
        // so the diode-connected base does not float and stall Newton.
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

        let mem = c.node(&format!("mem{k}"));
        c.add(Device::Bjt {
            name: format!("Q2_{k}"),
            c: mem,
            b: rref,
            e: NodeId::GROUND,
            model,
        });
        net.push_str(&format!("Q2_{k} mem{k} ref{k} 0 QMOD\n"));

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
    }

    net.push_str(".model SMOD SW(VT=1.5 VH=1.0 RON=10 ROFF=1e9)\n");
    net.push_str(".model QMOD NPN(IS=1e-15 BF=150 VAF=80 NF=1.0)\n");
    net.push_str(".tran 1u 400u uic\n");
    net.push_str(".print tran v(mem0)\n");
    net.push_str(".options reltol=1e-4\n.end\n");

    Synapse {
        netlist: net,
        circuit: c,
    }
}

pub fn synapse_opts() -> SolverOptions {
    SolverOptions {
        integration: Integration::Trapezoidal,
        step: StepControl::Fixed { dt: 1e-6 },
        reltol: 1e-4,
        // Cold-start switch transitions are stiff for a global Newton; the
        // partitioned path needs far less of both, since each block solves alone.
        max_newton: 200,
        gmin: 1e-9,
        partitioning: Partitioning::Auto,
        ..SolverOptions::default()
    }
}
