//! A lightweight, owned netlist extracted from a [`forge_model::Pcb`].
//!
//! The repeat-detection algorithm works on this flat representation rather than
//! re-reading the CST on every access: footprints become [`Comp`]s, pads carry
//! their net assignment, and nets are interned to small integer ids. Everything
//! downstream (partitioning, fingerprinting, rebuild) operates on this.

use forge_model::Pcb;
use std::collections::HashMap;

/// A pad on a component, with its (interned) net.
#[derive(Debug, Clone)]
pub struct CompPad {
    /// Pad number/name as it appears in the footprint (e.g. "1", "G", "2").
    pub number: String,
    /// Interned net id; `None` for an unconnected pad (no net or net "").
    pub net: Option<NetId>,
}

/// A footprint reduced to the fields the analysis cares about.
#[derive(Debug, Clone)]
pub struct Comp {
    /// Index into [`Netlist::comps`]; equals position.
    pub idx: usize,
    pub reference: String,
    pub lib_id: String,
    pub value: String,
    pub layer: String,
    /// Footprint placement `(x, y, rotation_degrees)`.
    pub at: (f64, f64, f64),
    pub pads: Vec<CompPad>,
}

/// Interned net identifier (dense, starting at 0).
pub type NetId = u32;

/// The whole board as a flat netlist.
#[derive(Debug, Clone)]
pub struct Netlist {
    pub comps: Vec<Comp>,
    /// net id -> net name (for the global-net heuristic and reporting).
    pub net_names: Vec<String>,
}

impl Netlist {
    /// Build a netlist from a parsed PCB.
    ///
    /// Net interning is keyed on the net *name* when available (so v10 string
    /// nets and v5-v9 numeric nets unify), falling back to the numeric id with a
    /// synthetic name when a pad has a net id but no name. Pads with net 0 / no
    /// net / empty name are treated as unconnected.
    pub fn from_pcb(pcb: &Pcb) -> Netlist {
        let mut net_intern: HashMap<String, NetId> = HashMap::new();
        let mut net_names: Vec<String> = Vec::new();

        let mut intern = |name: &str, names: &mut Vec<String>| -> NetId {
            if let Some(&id) = net_intern.get(name) {
                return id;
            }
            let id = names.len() as NetId;
            names.push(name.to_string());
            net_intern.insert(name.to_string(), id);
            id
        };

        let mut comps = Vec::new();
        for (idx, fp) in pcb.footprints().iter().enumerate() {
            let mut pads = Vec::new();
            for pad in fp.pads() {
                let net = match pad.net() {
                    // Net 0 with empty name, or no name, means unconnected.
                    Some((id, name)) => {
                        if name.is_empty() || (id == 0 && name.is_empty()) {
                            None
                        } else {
                            Some(intern(&name, &mut net_names))
                        }
                    }
                    None => None,
                };
                pads.push(CompPad {
                    number: pad.number(),
                    net,
                });
            }
            comps.push(Comp {
                idx,
                reference: fp.reference().unwrap_or_default(),
                lib_id: fp.lib_id(),
                value: fp.value().unwrap_or_default(),
                layer: fp.layer(),
                at: fp.at(),
                pads,
            });
        }

        Netlist { comps, net_names }
    }

    /// Number of distinct components touching each net.
    ///
    /// Used by the global-net heuristic: a net wired to a large fraction of all
    /// components is a power/ground rail and should not glue blocks together.
    pub fn net_fanout(&self) -> Vec<usize> {
        let mut seen: Vec<Option<usize>> = vec![None; self.net_names.len()];
        let mut count = vec![0usize; self.net_names.len()];
        for comp in &self.comps {
            for pad in &comp.pads {
                if let Some(net) = pad.net {
                    let n = net as usize;
                    if seen[n] != Some(comp.idx) {
                        seen[n] = Some(comp.idx);
                        count[n] += 1;
                    }
                }
            }
        }
        count
    }
}
