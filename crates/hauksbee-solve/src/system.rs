//! MNA unknown layout and per-device reactive state.
//!
//! Modified nodal analysis solves for every non-ground node voltage plus one
//! branch current per voltage source and per inductor (elements that can't be
//! expressed as a conductance). [`Layout`] assigns each unknown a row/column in
//! the system matrix once; the index is stable for the whole run so the sparse
//! pattern stays frozen.

use hauksbee_ir::{BOutput, Circuit, Device, DeviceId};

/// Maps circuit nodes and extra branches to dense unknown indices.
///
/// Node `k` (k >= 1) maps to unknown `k - 1`. Voltage sources and inductors
/// each get an appended branch-current unknown. Ground is never an unknown.
///
/// Device-private INTERNAL nodes (dev-plan 04 §3.2): a BJT whose model carries
/// a positive series resistance (`rb`/`re`/`rc`) owns one internal unknown per
/// nonzero terminal resistance — the intrinsic base/emitter/collector the
/// Gummel-Poon core moves onto, with the ohmic resistor stamped between the
/// external node and the internal one. These unknowns live INSIDE the node
/// block (`< n_nodes`), never in the branch block, because they are KCL rows:
/// they take the gmin shunt, participate in the node-block residual and the
/// `vntol` convergence test, and must never receive the staged-DC
/// `branch_reg` term. They have NO [`hauksbee_ir::NodeId`]: the netlist,
/// `Device::nodes`, `conduction_nodes`, `map_nodes` and the partition/tear
/// layers never see them, so a partitioner keeps a BJT and its internal nodes
/// in one island BY CONSTRUCTION — each sub-circuit's own `Layout::new`
/// re-allocates them locally. Allocation is keyed on the MODEL FIELDS ONLY
/// (a zero-valued resistance allocates nothing, so the common default-model
/// case pays nothing and stays bit-identical); the `series_resistance`
/// effects toggle is honored at STAMP time — when it is off, the core stamps
/// on the external nodes and any allocated internal unknown is pinned to 0 by
/// a unit diagonal, an isolated row that couples to nothing.
#[derive(Debug, Clone)]
pub struct Layout {
    /// Number of node-block unknowns: non-ground netlist nodes plus
    /// device-private internal nodes.
    pub n_nodes: usize,
    /// Total unknowns = nodes (incl. internal) + extra branches.
    pub size: usize,
    /// For each device that owns a branch current, its unknown index.
    /// `None` for devices without a branch.
    branch_of: Vec<Option<usize>>,
    /// For each BJT with series resistance, its internal-node unknowns in
    /// terminal order `[c_int, b_int, e_int]` (`None` per terminal whose
    /// series resistance is zero). Empty when the circuit has no such BJT,
    /// so existing circuits pay one `is_empty` check and nothing else.
    bjt_internal_of: Vec<[Option<usize>; 3]>,
}

impl Layout {
    /// Build the layout for a circuit.
    pub fn new(circuit: &Circuit) -> Layout {
        let n_real = circuit.max_node() as usize;
        let mut branch_of = vec![None; circuit.devices.len()];
        // Internal nodes first (extending the node block), then branches.
        let mut bjt_internal_of: Vec<[Option<usize>; 3]> = Vec::new();
        let mut next = n_real;
        for (_, dev) in circuit.iter() {
            if let Device::Bjt { model, .. } = dev {
                if model.rc > 0.0 || model.rb > 0.0 || model.re > 0.0 {
                    if bjt_internal_of.is_empty() {
                        bjt_internal_of = vec![[None; 3]; circuit.devices.len()];
                    }
                }
            }
        }
        if !bjt_internal_of.is_empty() {
            for (id, dev) in circuit.iter() {
                if let Device::Bjt { model, .. } = dev {
                    let mut alloc = |r: f64| {
                        if r > 0.0 {
                            let i = next;
                            next += 1;
                            Some(i)
                        } else {
                            None
                        }
                    };
                    bjt_internal_of[id.0 as usize] =
                        [alloc(model.rc), alloc(model.rb), alloc(model.re)];
                }
            }
        }
        let n_nodes = next;
        for (id, dev) in circuit.iter() {
            // A VCVS fixes its output-port voltage like an ideal Vsource, so it
            // carries the same branch-current unknown (the VCCS does not: its
            // stamp is a pure transconductance with no extra unknown). The CCVS
            // is the H-card analogue of the VCVS: it too fixes its output-port
            // voltage, so it owns a branch; the CCCS, like the VCCS/Isource,
            // adds no unknown — it only WRITES into its control source's branch
            // column, which `Layout::branch(ctrl_src)` resolves after this
            // freeze (that is the `branch_index_of` accessor the F/H stamps and
            // `reserve_pattern` consume).
            // A V-output B-source (`Bxxx p n V={expr}`) fixes its output-port
            // voltage like a VCVS/CCVS, so it owns the same branch-current
            // unknown; the I-output form injects current like an Isource and
            // adds nothing.
            if matches!(
                dev,
                Device::Vsource { .. }
                    | Device::Inductor { .. }
                    | Device::Vcvs { .. }
                    | Device::Ccvs { .. }
                    | Device::Behavioral {
                        output: BOutput::Voltage,
                        ..
                    }
            ) {
                branch_of[id.0 as usize] = Some(next);
                next += 1;
            }
        }
        Layout {
            n_nodes,
            size: next,
            branch_of,
            bjt_internal_of,
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

    /// Internal-node unknowns of a series-resistance BJT, in terminal order
    /// `[c_int, b_int, e_int]`. `None` when the device allocated none (the
    /// common case: default models, or not a BJT).
    #[inline]
    pub fn bjt_internal(&self, id: DeviceId) -> Option<&[Option<usize>; 3]> {
        if self.bjt_internal_of.is_empty() {
            return None;
        }
        let ints = &self.bjt_internal_of[id.0 as usize];
        if ints.iter().all(|i| i.is_none()) {
            None
        } else {
            Some(ints)
        }
    }
}

/// Per-step reactive history a device needs for its companion model.
///
/// Companion (integration) models replace a capacitor/inductor with a
/// conductance in parallel/series with a current source whose value depends on
/// the previous step(s). This struct carries that history per timestep so the
/// stamping code stays stateless.
///
/// Slot packing for multi-state devices (dev-plan 04 §3.2): the primary bank
/// (`x1`/`dx1`/`x2`) holds ONE state per device — capacitor voltage, inductor
/// current, a charge-storing diode's junction charge, or a BJT's
/// BASE-EMITTER charge. The secondary bank (`x1b`/`dx1b`/`x2b`) exists for
/// the one device that stores TWO independent charges: the BJT's BASE-
/// COLLECTOR junction. Both banks are indexed by `DeviceId.0` and roll
/// forward together; every single-state device simply leaves its secondary
/// slot at zero.
#[derive(Debug, Clone, Default)]
pub struct ReactiveState {
    /// Per-device stored voltage (caps) or current (inductors) at the accepted
    /// previous step, indexed by `DeviceId.0`.
    pub x1: Vec<f64>,
    /// Time-derivative at the previous step (for trapezoidal / Gear-2).
    pub dx1: Vec<f64>,
    /// One step further back (for Gear-2's two-point history).
    pub x2: Vec<f64>,
    /// Secondary bank (BJT base-collector charge), same indexing as `x1`.
    pub x1b: Vec<f64>,
    /// Secondary-bank derivative, same indexing as `dx1`.
    pub dx1b: Vec<f64>,
    /// Secondary-bank two-step history, same indexing as `x2`.
    pub x2b: Vec<f64>,
}

impl ReactiveState {
    /// History zeroed for `n` devices.
    pub fn new(n: usize) -> Self {
        ReactiveState {
            x1: vec![0.0; n],
            dx1: vec![0.0; n],
            x2: vec![0.0; n],
            x1b: vec![0.0; n],
            dx1b: vec![0.0; n],
            x2b: vec![0.0; n],
        }
    }
}
