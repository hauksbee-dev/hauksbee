//! Feedforward discovery: which sense couplings are provably one-directional.
//!
//! Input: the [`ConductionGraph`](super::conduction::ConductionGraph)'s
//! islands and sense edges. Output: the *stage DAG*, the partial order that
//! licenses solving upstream islands first and replaying their boundary
//! waveforms into downstream islands as sources: the free tear, the weakest
//! and most widely applicable of the tear kinds.
//!
//! ## The proof obligation
//!
//! A free tear across a sense edge A→B is exact only if nothing flows back:
//! no conduction path B→A (impossible by construction, distinct conduction
//! components share no conduction path at all) and no sense edge B→A (a
//! comparator in A watching a node in B would make the coupling
//! bidirectional). This is the Tarski STEP-0 gate as an algorithm: the
//! exhaustive net sweep that proves the hidden and output comparator families
//! disjoint. Here the sweep is
//! a strongly-connected-components pass over the island digraph: islands in
//! the same SCC are genuinely coupled and stay fused into one solve;
//! condensation edges between distinct components are one-directional *by
//! definition of condensation*, and each is a certified free tear.
//!
//! ## Oscillators contain themselves
//!
//! A relaxation oscillator whose stretched output gates its own reset (lore
//! #5) appears here as an island sensing one of its own nodes: a self-loop.
//! Self-loops never merge components and never cross a tear boundary (the
//! edge is intra-island), so the oscillator is automatically kept whole; the
//! flag is still recorded because the *orchestrator* must know the island has
//! internal feedback (its waveform is not a function of its inputs alone, so
//! caching or reordering optimizations that assume input-determinism are off
//! the table for it).
//!
//! Long-form how-and-why (motivation, theory, rejected alternatives, the
//! buried bodies): docs/how-and-why/hauksbee-solve/decompose.md

use hauksbee_ir::{Circuit, NodeId};

use super::conduction::ConductionGraph;

/// A certified one-directional sense coupling between two solve groups: the
/// upstream group conducts `node`; devices in the downstream group only read
/// it. Solving upstream first and replaying `node`'s waveform downstream is
/// electrically exact (up to the capture grid; the certificate carries that
/// tolerance).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FreeTearEdge {
    /// The sensed (torn) node.
    pub node: NodeId,
    /// Index into [`StageDag::groups`] of the group that conducts the node.
    pub upstream: usize,
    /// Index of the group whose devices sense it.
    pub downstream: usize,
}

/// The feedforward decomposition of a circuit's conduction islands.
#[derive(Debug, Clone)]
pub struct StageDag {
    /// Solve groups: each is one or more conduction islands (island indices
    /// into the source `ConductionGraph`) fused because their sense couplings
    /// are cyclic. A group of size 1 with no self-loop is pure feedforward.
    pub groups: Vec<Vec<usize>>,
    /// Groups in dependency order: every group in `stages[k]` depends only on
    /// groups in stages `< k`. Groups within one stage are mutually
    /// independent (parallel-solvable).
    pub stages: Vec<Vec<usize>>,
    /// The certified free tears (see [`FreeTearEdge`]). Every inter-group
    /// sense coupling appears here exactly once per (node, downstream) pair.
    pub free_tears: Vec<FreeTearEdge>,
    /// Groups containing an island that senses one of its own conduction
    /// nodes (relaxation oscillators and other self-referential blocks).
    /// Their output waveforms are not functions of their inputs alone.
    pub self_sensing: Vec<bool>,
}

impl StageDag {
    /// Build the stage DAG from a conduction analysis.
    ///
    /// `graph` must have been computed from `circuit` (the circuit is needed
    /// only to look up which island conducts a sensed node; the graph already
    /// carries that in `node_island`).
    pub fn build(_circuit: &Circuit, graph: &ConductionGraph) -> StageDag {
        let n = graph.islands.len();

        // Island digraph: edge u -> v when island v senses a node island u
        // conducts. Self-loops recorded separately (they do not affect SCCs
        // or staging, but the orchestrator needs the flag).
        let mut edges: Vec<Vec<usize>> = vec![Vec::new(); n];
        let mut island_self_sense = vec![false; n];
        // (upstream island, node, downstream island), deduped later via sort.
        let mut sense_pairs: Vec<(usize, NodeId, usize)> = Vec::new();

        // island_of_device by direct index: devices were pushed in island
        // order, so build a lookup once instead of the O(islands) scan the
        // public helper does.
        let mut device_island = std::collections::HashMap::new();
        for (idx, isl) in graph.islands.iter().enumerate() {
            for id in isl {
                device_island.insert(*id, idx);
            }
        }

        for e in &graph.sense_edges {
            let Some(&down) = device_island.get(&e.device) else {
                // A device with no conduction terminal senses something: it
                // has no island to pull the signal into; nothing to stage.
                continue;
            };
            let Some(up) = graph.node_island.get(e.node.0 as usize).copied().flatten() else {
                // Sensed node nobody conducts: an exogenous boundary input,
                // not an inter-island coupling.
                continue;
            };
            if up == down {
                island_self_sense[down] = true;
            } else {
                edges[up].push(down);
                sense_pairs.push((up, e.node, down));
            }
        }

        // Tarjan SCC, iterative (boards reach thousands of islands; the
        // recursive formulation would be a stack overflow waiting for a
        // production netlist).
        let sccs = tarjan_sccs(n, &edges);
        let mut island_group = vec![usize::MAX; n];
        for (gi, group) in sccs.iter().enumerate() {
            for &isl in group {
                island_group[isl] = gi;
            }
        }

        // Condensation edges + free tears (deduped per node/downstream).
        let g = sccs.len();
        let mut gedges: Vec<Vec<usize>> = vec![Vec::new(); g];
        let mut free_tears: Vec<FreeTearEdge> = Vec::new();
        sense_pairs.sort_unstable_by_key(|(u, n, d)| (*u, n.0, *d));
        sense_pairs.dedup();
        for (u, node, d) in sense_pairs {
            let (gu, gd) = (island_group[u], island_group[d]);
            if gu != gd {
                gedges[gu].push(gd);
                let tear = FreeTearEdge {
                    node,
                    upstream: gu,
                    downstream: gd,
                };
                if !free_tears.contains(&tear) {
                    free_tears.push(tear);
                }
            }
            // gu == gd: a sense edge inside a fused (cyclic) group; nothing
            // to tear, the group solves as one.
        }

        // Stages by longest-path depth over the condensation, so independent
        // groups share a stage. Tarjan emits components in reverse
        // topological order, which gives us a topological order for free.
        let mut depth = vec![0usize; g];
        for gi in (0..g).rev() {
            // reverse of Tarjan emission order = topological order
            for &to in &gedges[gi] {
                if depth[to] < depth[gi] + 1 {
                    depth[to] = depth[gi] + 1;
                }
            }
        }
        let max_depth = depth.iter().copied().max().unwrap_or(0);
        let mut stages: Vec<Vec<usize>> = vec![Vec::new(); max_depth + 1];
        for (gi, d) in depth.iter().enumerate() {
            stages[*d].push(gi);
        }

        let self_sensing = sccs
            .iter()
            .map(|group| group.iter().any(|&i| island_self_sense[i]))
            .collect();

        StageDag {
            groups: sccs,
            stages,
            free_tears,
            self_sensing,
        }
    }
}

/// Iterative Tarjan strongly-connected components. Emits components in
/// reverse topological order of the condensation (standard Tarjan property,
/// relied on by the staging pass above).
fn tarjan_sccs(n: usize, edges: &[Vec<usize>]) -> Vec<Vec<usize>> {
    const UNVISITED: usize = usize::MAX;
    let mut index = vec![UNVISITED; n];
    let mut lowlink = vec![0usize; n];
    let mut on_stack = vec![false; n];
    let mut stack: Vec<usize> = Vec::new();
    let mut next_index = 0usize;
    let mut sccs: Vec<Vec<usize>> = Vec::new();

    // Explicit DFS frames: (node, next child position).
    let mut frames: Vec<(usize, usize)> = Vec::new();

    for start in 0..n {
        if index[start] != UNVISITED {
            continue;
        }
        frames.push((start, 0));
        index[start] = next_index;
        lowlink[start] = next_index;
        next_index += 1;
        stack.push(start);
        on_stack[start] = true;

        while let Some(&mut (v, ref mut child)) = frames.last_mut() {
            if *child < edges[v].len() {
                let w = edges[v][*child];
                *child += 1;
                if index[w] == UNVISITED {
                    index[w] = next_index;
                    lowlink[w] = next_index;
                    next_index += 1;
                    stack.push(w);
                    on_stack[w] = true;
                    frames.push((w, 0));
                } else if on_stack[w] {
                    lowlink[v] = lowlink[v].min(index[w]);
                }
            } else {
                frames.pop();
                if let Some(&mut (parent, _)) = frames.last_mut() {
                    lowlink[parent] = lowlink[parent].min(lowlink[v]);
                }
                if lowlink[v] == index[v] {
                    let mut comp = Vec::new();
                    while let Some(w) = stack.pop() {
                        on_stack[w] = false;
                        comp.push(w);
                        if w == v {
                            break;
                        }
                    }
                    sccs.push(comp);
                }
            }
        }
    }
    sccs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decompose::conduction::ConductionGraph;
    use hauksbee_ir::{Circuit, Device, SourceKind};

    fn rc_stage(c: &mut Circuit, tag: &str) -> (NodeId, NodeId) {
        let inp = c.node(&format!("{tag}_in"));
        let out = c.node(&format!("{tag}_out"));
        c.add(Device::Vsource {
            name: format!("V{tag}"),
            p: inp,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(5.0),
        });
        c.add(Device::Resistor {
            name: format!("R{tag}"),
            a: inp,
            b: out,
            ohms: 1e3,
            tc1: None,
        });
        c.add(Device::Capacitor {
            name: format!("C{tag}"),
            a: out,
            b: NodeId::GROUND,
            farads: 1e-9,
            ic: None,
        });
        (inp, out)
    }

    /// The canonical free-tear fixture: two RC stages coupled by a comparator
    /// select. Expect two groups, stage A before stage B, one free tear on the
    /// sensed node.
    #[test]
    fn comparator_coupling_yields_two_stages_and_a_free_tear() {
        let mut c = Circuit::new();
        let (_, a_out) = rc_stage(&mut c, "a");
        let (_, b_out) = rc_stage(&mut c, "b");
        // Comparator lives in B (conducts b_out's island via its out node on a
        // fresh node bridged into B), senses A.
        let cmp_out = c.node("cmp_out");
        c.add(Device::Resistor {
            name: "Rbridge_b".into(),
            a: cmp_out,
            b: b_out,
            ohms: 1e3,
            tc1: None,
        });
        c.add(Device::Comparator {
            name: "CMP".into(),
            out: cmp_out,
            inp: a_out,
            inn: NodeId::GROUND,
            out_lo: 0.0,
            out_hi: 5.0,
            hysteresis: 1e-3,
        });

        let g = ConductionGraph::analyze(&c);
        let dag = StageDag::build(&c, &g);
        assert_eq!(dag.groups.len(), 2, "{dag:?}");
        assert_eq!(dag.stages.len(), 2, "A strictly before B: {dag:?}");
        assert_eq!(dag.free_tears.len(), 1, "{:?}", dag.free_tears);
        assert_eq!(dag.free_tears[0].node, a_out);
        assert!(dag.self_sensing.iter().all(|s| !s));
    }

    /// Fixture (b): add a *sense* feedback (a comparator in A watching B).
    /// The coupling is now cyclic: one fused group, no free tears, one stage.
    #[test]
    fn sense_feedback_refuses_the_tear() {
        let mut c = Circuit::new();
        let (_, a_out) = rc_stage(&mut c, "a");
        let (_, b_out) = rc_stage(&mut c, "b");
        let cmp_ab = c.node("cmp_ab");
        c.add(Device::Resistor {
            name: "RB1".into(),
            a: cmp_ab,
            b: b_out,
            ohms: 1e3,
            tc1: None,
        });
        c.add(Device::Comparator {
            name: "CMP_AB".into(),
            out: cmp_ab,
            inp: a_out,
            inn: NodeId::GROUND,
            out_lo: 0.0,
            out_hi: 5.0,
            hysteresis: 1e-3,
        });
        // The reverse watcher: lives in A, senses B.
        let cmp_ba = c.node("cmp_ba");
        c.add(Device::Resistor {
            name: "RA1".into(),
            a: cmp_ba,
            b: a_out,
            ohms: 1e3,
            tc1: None,
        });
        c.add(Device::Comparator {
            name: "CMP_BA".into(),
            out: cmp_ba,
            inp: b_out,
            inn: NodeId::GROUND,
            out_lo: 0.0,
            out_hi: 5.0,
            hysteresis: 1e-3,
        });

        let g = ConductionGraph::analyze(&c);
        let dag = StageDag::build(&c, &g);
        assert_eq!(dag.groups.len(), 1, "cyclic sense coupling fuses: {dag:?}");
        assert!(dag.free_tears.is_empty(), "{:?}", dag.free_tears);
        assert_eq!(dag.stages.len(), 1);
    }

    /// Fixture (c): the relaxation-oscillator shape, an island whose
    /// comparator senses the island's own output. One group, flagged
    /// self-sensing, no tear offered on the self-edge.
    #[test]
    fn self_resetting_oscillator_is_contained_and_flagged() {
        let mut c = Circuit::new();
        let (_, out) = rc_stage(&mut c, "osc");
        let reset = c.node("reset");
        c.add(Device::Resistor {
            name: "Rreset".into(),
            a: reset,
            b: out,
            ohms: 10e3,
            tc1: None,
        });
        c.add(Device::Comparator {
            name: "CMPOSC".into(),
            out: reset,
            inp: out,
            inn: NodeId::GROUND,
            out_lo: 0.0,
            out_hi: 5.0,
            hysteresis: 1e-3,
        });

        let g = ConductionGraph::analyze(&c);
        let dag = StageDag::build(&c, &g);
        assert_eq!(dag.groups.len(), 1, "{dag:?}");
        assert!(dag.free_tears.is_empty());
        assert_eq!(dag.self_sensing, vec![true]);
    }

    /// A three-stage chain (A drives B drives C) plus an independent island D
    /// sensing A: staging must put A alone in stage 0, B and D together in
    /// stage 1 (both depend only on A), C in stage 2.
    #[test]
    fn stages_group_independent_islands() {
        let mut c = Circuit::new();
        let (_, a_out) = rc_stage(&mut c, "a");
        let (_, b_out) = rc_stage(&mut c, "b");
        let (_, c_out) = rc_stage(&mut c, "c");
        let (_, d_out) = rc_stage(&mut c, "d");
        for (name, inp, island_out) in [
            ("AB", a_out, b_out),
            ("BC", b_out, c_out),
            ("AD", a_out, d_out),
        ] {
            let o = c.node(&format!("cmp_{name}"));
            c.add(Device::Resistor {
                name: format!("R{name}"),
                a: o,
                b: island_out,
                ohms: 1e3,
                tc1: None,
            });
            c.add(Device::Comparator {
                name: format!("CMP{name}"),
                out: o,
                inp,
                inn: NodeId::GROUND,
                out_lo: 0.0,
                out_hi: 5.0,
                hysteresis: 1e-3,
            });
        }

        let g = ConductionGraph::analyze(&c);
        let dag = StageDag::build(&c, &g);
        assert_eq!(dag.groups.len(), 4);
        assert_eq!(dag.stages.len(), 3, "{dag:?}");
        assert_eq!(dag.stages[0].len(), 1);
        assert_eq!(dag.stages[1].len(), 2, "B and D are independent: {dag:?}");
        assert_eq!(dag.stages[2].len(), 1);
        assert_eq!(dag.free_tears.len(), 3);
    }
}
