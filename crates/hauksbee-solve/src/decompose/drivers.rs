//! The driver pass: which upstream groups are absorbed instead of torn.
//!
//! The failure this pass exists to prevent: a tear that correctly excludes
//! switch-control nets from conduction reachability thereby also excludes the
//! little Thevenin drivers (`Vdrv` behind `Rdrv`) that *hold* those nets at
//! their latched values. The sense nets float, every magnitude switch sits at
//! its band centre, and no synapse current reaches any membrane: a dead board,
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
    use crate::test_fixtures::{cap, comparator, diode, res, sw, vdc, GND};
    use hauksbee_ir::{Circuit, NodeId};

    fn thevenin_driver(c: &mut Circuit) -> NodeId {
        let (vdrv, sel) = (c.node("vdrv"), c.node("sel"));
        vdc(c, "Vdrv", vdrv, 5.0);
        res(c, "Rdrv", vdrv, sel, 1e3);
        sel
    }

    /// A source feeding a load through a switch whose select is `sel`.
    fn switch_consumer(c: &mut Circuit, tag: &str, sel: NodeId) {
        let s = c.node(&format!("{tag}_src"));
        let o = c.node(&format!("{tag}_out"));
        vdc(c, &format!("V{tag}"), s, 3.3);
        sw(c, &format!("SW{tag}"), s, o, sel, (2.0, 1.0), 1.0);
        res(c, &format!("RL{tag}"), o, GND, 10e3);
    }

    fn assignments(c: &Circuit) -> (StageDag, Vec<DriverAssignment>, ConductionGraph) {
        let g = ConductionGraph::analyze(c);
        let dag = StageDag::build(c, &g);
        let asn = driver_assignments(c, &g, &dag, &DriverPolicy::default());
        (dag, asn, g)
    }

    /// A Thevenin select driver is absorbed into its consumer's group; one
    /// driver holding two consumers' selects is replicated into both.
    #[test]
    fn thevenin_select_driver_is_absorbed_into_every_consumer() {
        let mut c = Circuit::new();
        let sel = thevenin_driver(&mut c);
        switch_consumer(&mut c, "x", sel);
        let (dag, asn, g) = assignments(&c);
        assert_eq!(dag.groups.len(), 2, "{dag:?}");
        assert_eq!(asn.len(), 1, "{asn:?}");
        assert_eq!(asn[0].consumers.len(), 1);
        let dev_count: usize = dag.groups[asn[0].driver_group]
            .iter()
            .map(|&i| g.islands[i].len())
            .sum();
        assert_eq!(
            dev_count, 2,
            "the driver is exactly the two Thevenin devices"
        );

        switch_consumer(&mut c, "y", sel);
        let (_, asn, _) = assignments(&c);
        assert_eq!(asn.len(), 1, "{asn:?}");
        assert_eq!(asn[0].consumers.len(), 2, "replicate into both: {asn:?}");
    }

    /// A large linear upstream stays a staged tear; a nonlinear upstream and a
    /// self-sensing upstream are never absorbed.
    #[test]
    fn large_nonlinear_or_self_sensing_upstreams_are_not_absorbed() {
        let mut c = Circuit::new();
        let vin = c.node("vin");
        vdc(&mut c, "VIN", vin, 5.0);
        let mut last = vin;
        for k in 0..10 {
            let n = c.node(&format!("l{k}"));
            res(&mut c, &format!("R{k}"), last, n, 1e3);
            cap(&mut c, &format!("C{k}"), n, GND, 1e-9);
            last = n;
        }
        let cmp_out = c.node("cmp_out");
        res(&mut c, "RCMP", cmp_out, GND, 1e3);
        comparator(&mut c, "CMP", cmp_out, last, GND, 1e-3);
        let (dag, asn, _) = assignments(&c);
        assert_eq!(dag.groups.len(), 2);
        assert!(
            asn.is_empty(),
            "21 devices exceeds the driver budget: {asn:?}"
        );
        assert_eq!(dag.free_tears.len(), 1);

        let mut c = Circuit::new();
        let vin = c.node("vin");
        vdc(&mut c, "V1", vin, 3.0);
        let drv = c.node("drv");
        res(&mut c, "R1", vin, drv, 1e3);
        diode(&mut c, "D1", drv, GND, Default::default());
        switch_consumer(&mut c, "x", drv);
        assert!(
            assignments(&c).1.is_empty(),
            "a nonlinear upstream may not be absorbed"
        );

        let mut c = Circuit::new();
        let vin = c.node("vin");
        vdc(&mut c, "V1", vin, 3.0);
        let osc = c.node("osc");
        res(&mut c, "R1", vin, osc, 1e3);
        let dump = c.node("dump");
        sw(&mut c, "S_reset", osc, dump, osc, (2.0, 1.0), 10.0);
        res(&mut c, "Rdump", dump, GND, 1e3);
        switch_consumer(&mut c, "x", osc);
        assert!(
            assignments(&c).1.is_empty(),
            "a self-sensing upstream may not be absorbed"
        );
    }
}
