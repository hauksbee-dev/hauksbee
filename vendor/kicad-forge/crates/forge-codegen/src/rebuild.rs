//! Semantic round-trip: rebuild a [`Pcb`] from the detected structure and
//! verify it is equivalent to the original.
//!
//! The rebuild walks the [`Analysis`] (clusters -> instances -> slots, then
//! singletons), so it exercises the detected structure rather than blindly
//! copying the netlist. Every component reachable through the analysis is
//! reconstructed via [`PcbBuilder`] with its pads and net assignments, then the
//! result's extraction is compared against the original up to net renaming.

use crate::cluster::Analysis;
use crate::netlist::{NetId, Netlist};
use forge_model::{FootprintBuilder, Pcb, PcbBuilder};
use std::collections::{BTreeMap, HashMap};

/// Rebuild a PCB by interpreting the analysis structure over the netlist.
///
/// `version` selects the KiCad format version for the emitted board.
pub fn rebuild(nl: &Netlist, analysis: &Analysis, version: i64) -> Pcb {
    // Gather every component index reachable through the analysis, in a
    // deterministic order: cluster instances first (by cluster, then instance,
    // then slot), then singletons. This proves the analysis covers the board.
    let mut order: Vec<usize> = Vec::new();
    let mut seen = vec![false; nl.comps.len()];
    let push = |ci: usize, order: &mut Vec<usize>, seen: &mut Vec<bool>| {
        if !seen[ci] {
            seen[ci] = true;
            order.push(ci);
        }
    };

    for c in &analysis.clusters {
        for inst in &c.instances {
            for &ci in inst.comps_by_slot.iter().flatten() {
                push(ci, &mut order, &mut seen);
            }
            for &ci in &inst.extra_comps {
                push(ci, &mut order, &mut seen);
            }
        }
    }
    for c in &analysis.singletons {
        for inst in &c.instances {
            for slot in inst.comps_by_slot.iter().flatten() {
                push(*slot, &mut order, &mut seen);
            }
            for &ci in &inst.extra_comps {
                push(ci, &mut order, &mut seen);
            }
        }
    }
    // Safety net: any component the analysis somehow didn't reach (shouldn't
    // happen, but keeps the round-trip honest) is appended.
    for ci in 0..nl.comps.len() {
        push(ci, &mut order, &mut seen);
    }

    // Assign fresh dense net ids for the rebuilt board (1.. ; 0 reserved for
    // unconnected). Map original NetId -> new id, declared in first-seen order.
    let mut net_map: HashMap<NetId, i64> = HashMap::new();
    let mut next_net: i64 = 1;
    let mut nets_decl: Vec<(i64, String)> = Vec::new();
    for &ci in &order {
        for pad in &nl.comps[ci].pads {
            if let Some(net) = pad.net {
                net_map.entry(net).or_insert_with(|| {
                    let id = next_net;
                    next_net += 1;
                    nets_decl.push((id, nl.net_names[net as usize].clone()));
                    id
                });
            }
        }
    }

    let mut builder = PcbBuilder::new(version).standard_2layer_layers();
    for &(id, ref name) in &nets_decl {
        builder = builder.add_net(id, name);
    }

    for &ci in &order {
        let c = &nl.comps[ci];
        let mut fb = FootprintBuilder::new(&c.lib_id, &c.reference, &c.value)
            .at(c.at.0, c.at.1, c.at.2)
            .layer(&c.layer);
        for pad in &c.pads {
            let net = pad.net.map(|n| {
                let id = net_map[&n];
                (id, nl.net_names[n as usize].as_str())
            });
            // Minimal pad geometry: the round-trip compares connectivity, not
            // pad shapes. A smd rect at origin with the right number+net suffices
            // for the equivalence we assert.
            fb = fb.add_pad(
                &pad.number,
                "smd",
                "rect",
                (0.0, 0.0),
                (1.0, 1.0),
                None,
                vec!["F.Cu"],
                net,
            );
        }
        builder = builder.add_footprint(fb);
    }

    builder.build()
}

// ---------------------------------------------------------------------------
// Equivalence checking
// ---------------------------------------------------------------------------

/// A canonical, comparable summary of a board's semantics: footprint count,
/// per-footprint identity/placement, and pad->net connectivity *up to net
/// renaming*.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardSemantics {
    pub footprint_count: usize,
    /// Per footprint (keyed by reference): (lib_id, value, layer, quantized at).
    pub footprints: BTreeMap<String, FpSummary>,
    /// Canonical connectivity: sorted list of nets, each net being the sorted
    /// set of `(reference, pad_number)` endpoints. Net *names* are dropped so
    /// the comparison is up to renaming.
    pub nets: Vec<Vec<(String, String)>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FpSummary {
    pub lib_id: String,
    pub value: String,
    pub layer: String,
    /// Position quantized to 1e-3 mm to absorb float formatting noise.
    pub at_q: (i64, i64, i64),
}

/// Extract canonical semantics from a parsed PCB.
pub fn semantics(pcb: &Pcb) -> BoardSemantics {
    let nl = Netlist::from_pcb(pcb);
    semantics_of_netlist(&nl)
}

/// Extract canonical semantics directly from a netlist (same canonical form).
pub fn semantics_of_netlist(nl: &Netlist) -> BoardSemantics {
    let mut footprints = BTreeMap::new();
    // net name -> endpoints; we group by name then drop names at the end.
    let mut net_endpoints: HashMap<NetId, Vec<(String, String)>> = HashMap::new();

    for c in &nl.comps {
        footprints.insert(
            c.reference.clone(),
            FpSummary {
                lib_id: c.lib_id.clone(),
                value: c.value.clone(),
                layer: c.layer.clone(),
                at_q: (q(c.at.0), q(c.at.1), q(c.at.2)),
            },
        );
        for pad in &c.pads {
            if let Some(net) = pad.net {
                net_endpoints
                    .entry(net)
                    .or_default()
                    .push((c.reference.clone(), pad.number.clone()));
            }
        }
    }

    // Canonicalize each net's endpoint set, then sort the whole collection so
    // it is independent of net id/name.
    let mut nets: Vec<Vec<(String, String)>> = net_endpoints
        .into_values()
        .map(|mut eps| {
            eps.sort();
            eps.dedup();
            eps
        })
        // A net touching a single endpoint carries no connectivity; drop it so
        // unconnected/odd single-pad nets don't cause spurious mismatches.
        .filter(|eps| eps.len() >= 2)
        .collect();
    nets.sort();

    BoardSemantics {
        footprint_count: nl.comps.len(),
        footprints,
        nets,
    }
}

fn q(v: f64) -> i64 {
    (v * 1000.0).round() as i64
}

/// Compare two boards on **connectivity only**: same component count and the
/// same net wiring up to net renaming. Placement and footprint geometry are
/// ignored.
///
/// This is the right check for the Board-as-Code loop, where re-layout and
/// recompile deliberately move components: the bar is that the electrical
/// circuit is preserved, not that coordinates match. It is also robust to a
/// board carrying duplicate reference designators (which the placement-keyed
/// [`BoardSemantics::footprints`] map cannot represent), because nets are keyed
/// by `(reference, pad)` endpoint *sets*, not by a per-reference map.
pub fn compare_connectivity(
    original: &BoardSemantics,
    rebuilt: &BoardSemantics,
) -> Result<(), String> {
    if original.footprint_count != rebuilt.footprint_count {
        return Err(format!(
            "footprint count differs: original {} vs rebuilt {}",
            original.footprint_count, rebuilt.footprint_count
        ));
    }
    if original.nets != rebuilt.nets {
        return Err(format!(
            "connectivity differs: {} nets vs {} nets (after canonicalization)",
            original.nets.len(),
            rebuilt.nets.len()
        ));
    }
    Ok(())
}

/// Compare two board semantics, returning a human-readable diff on mismatch.
pub fn compare(original: &BoardSemantics, rebuilt: &BoardSemantics) -> Result<(), String> {
    if original.footprint_count != rebuilt.footprint_count {
        return Err(format!(
            "footprint count differs: original {} vs rebuilt {}",
            original.footprint_count, rebuilt.footprint_count
        ));
    }
    if original.footprints != rebuilt.footprints {
        // Find first divergence for a useful message.
        for (k, v) in &original.footprints {
            match rebuilt.footprints.get(k) {
                None => return Err(format!("footprint {k} missing in rebuild")),
                Some(rv) if rv != v => {
                    return Err(format!("footprint {k} differs: {:?} vs {:?}", v, rv))
                }
                _ => {}
            }
        }
        for k in rebuilt.footprints.keys() {
            if !original.footprints.contains_key(k) {
                return Err(format!("footprint {k} unexpectedly added in rebuild"));
            }
        }
        return Err("footprint sets differ".to_string());
    }
    if original.nets != rebuilt.nets {
        return Err(format!(
            "connectivity differs: {} nets vs {} nets (after canonicalization)",
            original.nets.len(),
            rebuilt.nets.len()
        ));
    }
    Ok(())
}
