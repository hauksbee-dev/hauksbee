//! MNA unknown layout and per-device reactive state.
//!
//! Modified nodal analysis solves for every non-ground node voltage plus one
//! branch current per voltage source and per inductor (elements that can't be
//! expressed as a conductance). [`Layout`] assigns each unknown a row/column in
//! the system matrix once; the index is stable for the whole run so the sparse
//! pattern stays frozen.

use hauksbee_ir::{Circuit, Device, DeviceId};

/// Maps circuit nodes and extra branches to dense unknown indices.
///
/// Node `k` (k >= 1) maps to unknown `k - 1`. Voltage sources and inductors
/// each get an appended branch-current unknown. Ground is never an unknown.
#[derive(Debug, Clone)]
pub struct Layout {
    /// Number of non-ground nodes.
    pub n_nodes: usize,
    /// Total unknowns = nodes + extra branches.
    pub size: usize,
    /// For each device that owns a branch current, its unknown index.
    /// `None` for devices without a branch.
    branch_of: Vec<Option<usize>>,
}

impl Layout {
    /// Build the layout for a circuit.
    pub fn new(circuit: &Circuit) -> Layout {
        let n_nodes = circuit.max_node() as usize;
        let mut branch_of = vec![None; circuit.devices.len()];
        let mut next = n_nodes;
        for (id, dev) in circuit.iter() {
            if matches!(dev, Device::Vsource { .. } | Device::Inductor { .. }) {
                branch_of[id.0 as usize] = Some(next);
                next += 1;
            }
        }
        Layout {
            n_nodes,
            size: next,
            branch_of,
        }
    }

    /// Unknown index of a non-ground node. Ground returns `None`.
    #[inline]
    pub fn node(&self, node: hauksbee_ir::NodeId) -> Option<usize> {
        if node.is_ground() {
            None
        } else {
            Some(node.0 as usize - 1)
        }
    }

    /// Branch-current unknown index for a device, if it owns one.
    #[inline]
    pub fn branch(&self, id: DeviceId) -> Option<usize> {
        self.branch_of[id.0 as usize]
    }
}

/// Per-step reactive history a device needs for its companion model.
///
/// Companion (integration) models replace a capacitor/inductor with a
/// conductance in parallel/series with a current source whose value depends on
/// the previous step(s). This struct carries that history per timestep so the
/// stamping code stays stateless.
#[derive(Debug, Clone, Default)]
pub struct ReactiveState {
    /// Per-device stored voltage (caps) or current (inductors) at the accepted
    /// previous step, indexed by `DeviceId.0`.
    pub x1: Vec<f64>,
    /// Time-derivative at the previous step (for trapezoidal / Gear-2).
    pub dx1: Vec<f64>,
    /// One step further back (for Gear-2's two-point history).
    pub x2: Vec<f64>,
}

impl ReactiveState {
    /// History zeroed for `n` devices.
    pub fn new(n: usize) -> Self {
        ReactiveState {
            x1: vec![0.0; n],
            dx1: vec![0.0; n],
            x2: vec![0.0; n],
        }
    }
}
