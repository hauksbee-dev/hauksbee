//! The driver pass: which upstream groups are absorbed instead of torn.
//!
//! The bug that forced this pass into existence (`docs/learn/tarski-saga.md`
//! §1c, STEP 1): the first exact tear correctly excluded switch-control nets
//! from conduction reachability, and thereby also excluded the little
//! Thevenin drivers (`Vdrv` behind `Rdrv`) that *held* those nets at their
//! latched values. The sense nets floated, every magnitude switch sat at its
//! band centre, and no synapse current reached any membrane: a dead board,
//! from dropping two-device islands.
//!
//! ## Why absorption is exact, and when
//!
//! By construction every inter-island coupling is a sense edge (conduction
//! fuses islands; only sensing crosses them). A sense edge carries zero
//! current, so an upstream island whose *only* outbound couplings are sense
//! edges sources no current at all into the rest of the circuit; its internal
//! state is fully determined by its own sources. Two consequences:
//!
//! * Absorbing it (copying its devices into the consumer's sub-circuit) is
//!   electrically exact and removes a capture/replay boundary along with the
//!   capture-grid tolerance the certificate would otherwise carry.
//! * Replicating it into *several* consumers is equally exact: since no
//!   current leaves it through any boundary, the copies cannot disagree.
//!   (This is precisely why a `Vdrv`/`Rdrv` pair can hold twenty switch
//!   selects: the select pins draw nothing, so the driver is a constant.)
//!
//! Absorption is a policy choice, not a correctness requirement: any upstream
//! group could instead be staged (solved first, waveform replayed). Small
//! linear drivers are absorbed because the copy costs almost nothing and the
//! staged alternative costs a capture grid; a large linear upstream (a filter
//! chain) stays a staged tear where the matrix-exponential fast path earns
//! its keep. The threshold is a policy field with its reasoning attached, not
//! a buried constant.
//!
//! Nonlinear upstream groups are never absorbed by this pass: their waveforms
//! are what the staged executor exists to capture, and copying a nonlinear
//! block into k consumers multiplies exactly the Newton work tearing was
//! supposed to remove.
//!
//! Long-form how-and-why (motivation, theory, rejected alternatives, the
//! buried bodies): docs/how-and-why/hauksbee-solve/decompose.md

use hauksbee_ir::Circuit;

use super::conduction::ConductionGraph;
use super::feedforward::StageDag;

/// Policy for the driver pass.
#[derive(Debug, Clone, Copy)]
pub struct DriverPolicy {
    /// Absorb a linear, sense-only-outbound group when it has at most this
    /// many devices. Default 8: comfortably covers the Thevenin pairs and
    /// small divider/reference stacks the pass exists for, while keeping a
    /// real RC filter chain (tens of reactive devices, where the exact
    /// matrix-exponential stage is the better home) on the staged path.
    pub max_driver_devices: usize,
}

impl Default for DriverPolicy {
    fn default() -> Self {
        DriverPolicy {
            max_driver_devices: 8,
        }
    }
}

/// One absorbable driver group and everyone who consumes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriverAssignment {
    /// Index into [`StageDag::groups`] of the driver.
    pub driver_group: usize,
    /// Groups (same indexing) that sense at least one of its nodes; the
    /// executor copies the driver's devices into each consumer's sub-circuit.
    pub consumers: Vec<usize>,
}

/// Identify the driver groups of a stage DAG.
///
/// A group qualifies when (a) every device in it is linear, (b) it senses
/// nothing itself (no inbound tears: a driver is a leaf of the dependency
/// order), and (c) it is small per `policy`. The returned assignments do not
/// modify `dag`; the staged executor applies them when it builds sub-circuits,
/// and every tear whose upstream is an absorbed driver is dropped from the
/// replay set (its tolerance never enters the certificate).
pub fn driver_assignments(
    circuit: &Circuit,
    graph: &ConductionGraph,
    dag: &StageDag,
    policy: &DriverPolicy,
) -> Vec<DriverAssignment> {
    let mut inbound = vec![false; dag.groups.len()];
    for t in &dag.free_tears {
        inbound[t.downstream] = true;
    }

    let mut out = Vec::new();
    for (gi, islands) in dag.groups.iter().enumerate() {
        if inbound[gi] {
            continue; // senses something itself: not a constant-state driver
        }
        if dag.self_sensing[gi] {
            continue; // oscillators are not constants, whatever their size
        }
        let devices: usize = islands.iter().map(|&i| graph.islands[i].len()).sum();
        if devices == 0 || devices > policy.max_driver_devices {
            continue;
        }
        let all_linear = islands.iter().all(|&i| {
            graph.islands[i]
                .iter()
                .all(|id| circuit.devices[id.0 as usize].is_linear())
        });
        if !all_linear {
            continue;
        }
        let mut consumers: Vec<usize> = dag
            .free_tears
            .iter()
            .filter(|t| t.upstream == gi)
            .map(|t| t.downstream)
            .collect();
        consumers.sort_unstable();
        consumers.dedup();
        if consumers.is_empty() {
            continue; // drives nothing: nothing to absorb it into
        }
        out.push(DriverAssignment {
            driver_group: gi,
            consumers,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decompose::conduction::ConductionGraph;
    use crate::decompose::feedforward::StageDag;
    use hauksbee_ir::{Circuit, Device, NodeId, SourceKind};

    /// The STEP-1 dead-membrane regression, in miniature: a VSwitch whose
    /// select is held by an exogenous Thevenin driver. The driver group must
    /// be assigned to the switch's group, so the select never floats.
    #[test]
    fn thevenin_select_driver_is_absorbed() {
        let mut c = Circuit::new();
        // The consumer island: a source feeding a load through the switch.
        let src = c.node("src");
        let out = c.node("out");
        c.add(Device::Vsource {
            name: "VS".into(),
            p: src,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(5.0),
        });
        // The driver island: Vdrv behind Rdrv holding the select.
        let vdrv = c.node("vdrv");
        let sel = c.node("sel");
        c.add(Device::Vsource {
            name: "Vdrv".into(),
            p: vdrv,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(5.0),
        });
        c.add(Device::Resistor {
            name: "Rdrv".into(),
            a: vdrv,
            b: sel,
            ohms: 1e3,
            tc1: None,
        });
        c.add(Device::VSwitch {
            name: "SW".into(),
            a: src,
            b: out,
            ctrl_p: sel,
            ctrl_n: NodeId::GROUND,
            von: 2.0,
            voff: 1.0,
            ron: 1.0,
            roff: 1e9,
        });
        c.add(Device::Resistor {
            name: "RL".into(),
            a: out,
            b: NodeId::GROUND,
            ohms: 10e3,
            tc1: None,
        });

        let g = ConductionGraph::analyze(&c);
        let dag = StageDag::build(&c, &g);
        assert_eq!(dag.groups.len(), 2, "{dag:?}");
        let assignments = driver_assignments(&c, &g, &dag, &DriverPolicy::default());
        assert_eq!(assignments.len(), 1, "{assignments:?}");
        assert_eq!(assignments[0].consumers.len(), 1);
        // The driver is the group containing exactly the two Thevenin devices.
        let d = assignments[0].driver_group;
        let dev_count: usize = dag.groups[d]
            .iter()
            .map(|&i| g.islands[i].len())
            .sum();
        assert_eq!(dev_count, 2);
    }

    /// One driver holding the selects of two independent consumers must be
    /// replicated into both (exact: zero current leaves it either way).
    #[test]
    fn shared_driver_lists_every_consumer() {
        let mut c = Circuit::new();
        let vdrv = c.node("vdrv");
        let sel = c.node("sel");
        c.add(Device::Vsource {
            name: "Vdrv".into(),
            p: vdrv,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(5.0),
        });
        c.add(Device::Resistor {
            name: "Rdrv".into(),
            a: vdrv,
            b: sel,
            ohms: 1e3,
            tc1: None,
        });
        for tag in ["x", "y"] {
            let s = c.node(&format!("{tag}_src"));
            let o = c.node(&format!("{tag}_out"));
            c.add(Device::Vsource {
                name: format!("V{tag}"),
                p: s,
                n: NodeId::GROUND,
                kind: SourceKind::Dc(3.3),
            });
            c.add(Device::VSwitch {
                name: format!("SW{tag}"),
                a: s,
                b: o,
                ctrl_p: sel,
                ctrl_n: NodeId::GROUND,
                von: 2.0,
                voff: 1.0,
                ron: 1.0,
                roff: 1e9,
            });
            c.add(Device::Resistor {
                name: format!("RL{tag}"),
                a: o,
                b: NodeId::GROUND,
                ohms: 10e3,
                tc1: None,
            });
        }

        let g = ConductionGraph::analyze(&c);
        let dag = StageDag::build(&c, &g);
        let assignments = driver_assignments(&c, &g, &dag, &DriverPolicy::default());
        assert_eq!(assignments.len(), 1, "{assignments:?}");
        assert_eq!(
            assignments[0].consumers.len(),
            2,
            "replicate into both: {assignments:?}"
        );
    }

    /// A big linear upstream (a long RC ladder driving a comparator input)
    /// is NOT a driver: it stays a staged tear where the matrix-exponential
    /// path solves it once and the waveform is replayed.
    #[test]
    fn large_linear_upstream_stays_staged() {
        let mut c = Circuit::new();
        let vin = c.node("vin");
        c.add(Device::Vsource {
            name: "VIN".into(),
            p: vin,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(5.0),
        });
        let mut prev = vin;
        let mut last = vin;
        for k in 0..10 {
            let n = c.node(&format!("l{k}"));
            c.add(Device::Resistor {
                name: format!("R{k}"),
                a: prev,
                b: n,
                ohms: 1e3,
                tc1: None,
            });
            c.add(Device::Capacitor {
                name: format!("C{k}"),
                a: n,
                b: NodeId::GROUND,
                farads: 1e-9,
                ic: None,
            });
            prev = n;
            last = n;
        }
        // Downstream comparator island sensing the ladder output.
        let cmp_out = c.node("cmp_out");
        c.add(Device::Resistor {
            name: "RCMP".into(),
            a: cmp_out,
            b: NodeId::GROUND,
            ohms: 1e3,
            tc1: None,
        });
        c.add(Device::Comparator {
            name: "CMP".into(),
            out: cmp_out,
            inp: last,
            inn: NodeId::GROUND,
            out_lo: 0.0,
            out_hi: 5.0,
            hysteresis: 1e-3,
        });

        let g = ConductionGraph::analyze(&c);
        let dag = StageDag::build(&c, &g);
        assert_eq!(dag.groups.len(), 2);
        let assignments = driver_assignments(&c, &g, &dag, &DriverPolicy::default());
        assert!(
            assignments.is_empty(),
            "21 devices exceeds the driver budget: {assignments:?}"
        );
        assert_eq!(dag.free_tears.len(), 1, "the staged tear remains");
    }

    /// A small NONLINEAR upstream must never be absorbed: replication is only
    /// exact for groups whose state is a linear function of their own sources
    /// (the doc's "nonlinear upstreams are never absorbed", now enforced).
    #[test]
    fn nonlinear_driver_is_not_absorbed() {
        let mut c = Circuit::new();
        let vin = c.node("vin");
        c.add(Device::Vsource {
            name: "V1".into(),
            p: vin,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(3.0),
        });
        let drv = c.node("drv");
        c.add(Device::Resistor {
            name: "R1".into(),
            a: vin,
            b: drv,
            ohms: 1e3,
            tc1: None,
        });
        // The nonlinearity: a diode clamp inside the would-be driver group.
        c.add(Device::Diode {
            name: "D1".into(),
            a: drv,
            k: NodeId::GROUND,
            model: hauksbee_ir::DiodeModel::default(),
        });
        // Downstream island senses drv through a switch select.
        let a = c.node("a");
        let b = c.node("b");
        c.add(Device::Vsource {
            name: "V2".into(),
            p: a,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(1.0),
        });
        c.add(Device::VSwitch {
            name: "S1".into(),
            a,
            b,
            ctrl_p: drv,
            ctrl_n: NodeId::GROUND,
            von: 2.0,
            voff: 1.0,
            ron: 10.0,
            roff: 1e9,
        });
        c.add(Device::Resistor {
            name: "RL".into(),
            a: b,
            b: NodeId::GROUND,
            ohms: 1e3,
            tc1: None,
        });
        let g = ConductionGraph::analyze(&c);
        let dag = StageDag::build(&c, &g);
        let asn = driver_assignments(&c, &g, &dag, &DriverPolicy::default());
        assert!(
            asn.is_empty(),
            "a nonlinear upstream may not be absorbed: {asn:?}"
        );
    }

    /// A SELF-SENSING upstream (a relaxation oscillator resetting itself)
    /// must never be absorbed: its output is not a function of its inputs
    /// alone, so replicas could diverge (lore #5).
    #[test]
    fn self_sensing_driver_is_not_absorbed() {
        let mut c = Circuit::new();
        let vin = c.node("vin");
        c.add(Device::Vsource {
            name: "V1".into(),
            p: vin,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(3.0),
        });
        let osc = c.node("osc");
        c.add(Device::Resistor {
            name: "R1".into(),
            a: vin,
            b: osc,
            ohms: 1e3,
            tc1: None,
        });
        // The self-sense: a switch in the SAME island whose control reads the
        // island's own output node.
        let dump = c.node("dump");
        c.add(Device::VSwitch {
            name: "S_reset".into(),
            a: osc,
            b: dump,
            ctrl_p: osc,
            ctrl_n: NodeId::GROUND,
            von: 2.0,
            voff: 1.0,
            ron: 10.0,
            roff: 1e9,
        });
        c.add(Device::Resistor {
            name: "Rdump".into(),
            a: dump,
            b: NodeId::GROUND,
            ohms: 1e3,
            tc1: None,
        });
        // Downstream consumer senses osc.
        let a = c.node("a");
        let b = c.node("b");
        c.add(Device::Vsource {
            name: "V2".into(),
            p: a,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(1.0),
        });
        c.add(Device::VSwitch {
            name: "S1".into(),
            a,
            b,
            ctrl_p: osc,
            ctrl_n: NodeId::GROUND,
            von: 2.0,
            voff: 1.0,
            ron: 10.0,
            roff: 1e9,
        });
        c.add(Device::Resistor {
            name: "RL".into(),
            a: b,
            b: NodeId::GROUND,
            ohms: 1e3,
            tc1: None,
        });
        let g = ConductionGraph::analyze(&c);
        let dag = StageDag::build(&c, &g);
        let asn = driver_assignments(&c, &g, &dag, &DriverPolicy::default());
        assert!(
            asn.is_empty(),
            "a self-sensing upstream may not be absorbed: {asn:?}"
        );
    }
}
