//! Sub-circuit extraction: the one node-remapping copy every executor that
//! solves part of a board on its own matrix builds on (the partitioned
//! engine's islands, the staged executor's solve groups, the stiff executor's
//! capture clusters).
//!
//! Local node ids are interned in first-sight order as devices are copied, so
//! the extracted circuit's construction order, which the solver SEES (LU
//! pivots, Newton paths, the last bits of accepted values), is a pure function
//! of the device order the caller copies in. Node names are preserved so
//! diagnostics read like the board, not like `n17`; an id the parent never
//! named (an as-built cut's reserved-range node) is named after its number so
//! two such nodes never fold into one.

use std::collections::HashMap;

use hauksbee_ir::{Circuit, Device, DeviceId, NodeId};

/// A circuit under construction from pieces of a parent, plus both directions
/// of its node map.
pub(crate) struct SubCircuit {
    pub(crate) circuit: Circuit,
    /// Parent node id -> local node id, non-ground nodes only.
    g2l: HashMap<u32, u32>,
    /// Local node id -> parent node id, dense (index 0 is ground).
    l2g: Vec<NodeId>,
}

impl SubCircuit {
    /// An empty sub-circuit inheriting the parent's temperature.
    pub(crate) fn new(parent: &Circuit) -> Self {
        let mut circuit = Circuit::new();
        circuit.temp_c = parent.temp_c;
        SubCircuit {
            circuit,
            g2l: HashMap::new(),
            l2g: vec![NodeId::GROUND],
        }
    }

    /// Local id of parent node `gn`, interned on first sight.
    pub(crate) fn map(&mut self, parent: &Circuit, gn: NodeId) -> NodeId {
        if gn.is_ground() {
            return NodeId::GROUND;
        }
        if let Some(&ln) = self.g2l.get(&gn.0) {
            return NodeId(ln);
        }
        let ln = if (gn.0 as usize) < parent.node_count() {
            self.circuit.node(parent.node_name(gn))
        } else {
            self.circuit.node(&format!("?{}", gn.0))
        };
        self.g2l.insert(gn.0, ln.0);
        debug_assert_eq!(ln.0 as usize, self.l2g.len(), "local ids are dense");
        self.l2g.push(gn);
        ln
    }

    /// Copy parent device `id` in with its nodes remapped; its local id.
    pub(crate) fn copy(&mut self, parent: &Circuit, id: DeviceId) -> DeviceId {
        let mut d = parent.devices[id.0 as usize].clone();
        d.map_nodes(&mut |gn| self.map(parent, gn));
        self.circuit.add(d)
    }

    /// Add a device already expressed in local ids.
    pub(crate) fn add(&mut self, device: Device) -> DeviceId {
        self.circuit.add(device)
    }

    /// Local id of an already-mapped parent node.
    pub(crate) fn local(&self, gn: NodeId) -> Option<NodeId> {
        if gn.is_ground() {
            return Some(NodeId::GROUND);
        }
        self.g2l.get(&gn.0).map(|&ln| NodeId(ln))
    }

    /// Parent node of every local node, by local id (ground at index 0).
    pub(crate) fn l2g(&self) -> &[NodeId] {
        &self.l2g
    }

    /// The circuit and its parent-to-local node map.
    pub(crate) fn into_parts(self) -> (Circuit, HashMap<u32, u32>) {
        (self.circuit, self.g2l)
    }
}
