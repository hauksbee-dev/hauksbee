//! MNA unknown layout and per-device reactive state.
//!
//! Modified nodal analysis solves for every non-ground node voltage plus one
//! branch current per voltage source and per inductor (elements that can't be
//! expressed as a conductance). [`Layout`] assigns each unknown a row/column in
//! the system matrix once; the index is stable for the whole run so the sparse
//! pattern stays frozen.
//!

use std::collections::HashMap;

use hauksbee_ir::{BOutput, Circuit, Device, DeviceId, NodeId};

/// Translation between externally stable `NodeId`s and the dense unknown block.
///
/// Parsed netlists use dense node ids, and that overwhelmingly common case keeps
/// the original subtraction-only lookup. As-built cuts deliberately allocate
/// ids from a high reserved range; those ids must be compacted or one cut would
/// manufacture hundreds of thousands of nonexistent MNA rows.
#[derive(Debug, Clone)]
enum NodeMap {
    Dense,
    Sparse {
        unknown_of: HashMap<NodeId, usize>,
        node_of: Vec<NodeId>,
    },
}

/// Maps circuit nodes and extra branches to dense unknown indices.
///
/// Dense netlist node `k` (k >= 1) maps to unknown `k - 1`. Sparse out-of-band
/// ids used by as-built cuts are compacted after the named nodes. Voltage
/// sources and inductors each get an appended branch-current unknown. Ground
/// is never an unknown.
///
/// Device-private INTERNAL nodes (dev-plan 04 §3.2): a BJT whose model carries
/// a positive series resistance (`rb`/`re`/`rc`) owns one internal unknown per
/// nonzero terminal resistance; the intrinsic base/emitter/collector the
/// Gummel-Poon core moves onto, with the ohmic resistor stamped between the
/// external node and the internal one. These unknowns live INSIDE the node
/// block (`< n_nodes`), never in the branch block, because they are KCL rows:
/// they take the gmin shunt, participate in the node-block residual and the
/// `vntol` convergence test, and must never receive the staged-DC
/// `branch_reg` term. They have NO [`hauksbee_ir::NodeId`]: the netlist,
/// `Device::nodes`, `conduction_nodes`, `map_nodes` and the partition/tear
/// layers never see them, so a partitioner keeps a BJT and its internal nodes
/// in one island BY CONSTRUCTION, each sub-circuit's own `Layout::new`
/// re-allocates them locally. Allocation is keyed on the MODEL FIELDS ONLY
/// (a zero-valued resistance allocates nothing, so the common default-model
/// case pays nothing and stays bit-identical); the `series_resistance`
/// effects toggle is honored at STAMP time, when it is off, the core stamps
/// on the external nodes and any allocated internal unknown is pinned to 0 by
/// a unit diagonal, an isolated row that couples to nothing.
#[derive(Debug, Clone)]
pub struct Layout {
    /// Number of node-block unknowns: non-ground netlist nodes plus
    /// device-private internal nodes.
    pub n_nodes: usize,
    /// Total unknowns = nodes (incl. internal) + extra branches.
    pub size: usize,
    /// Number of external (netlist/as-built) nodes before private device nodes.
    external_nodes: usize,
    node_map: NodeMap,
    /// For each device that owns a branch current, its unknown index.
    /// `None` for devices without a branch.
    branch_of: Vec<Option<usize>>,
    /// For each BJT with series resistance, its internal-node unknowns in
    /// terminal order `[c_int, b_int, e_int]` (`None` per terminal whose
    /// series resistance is zero). Empty when the circuit has no such BJT,
    /// so existing circuits pay one `is_empty` check and nothing else.
    bjt_internal_of: Vec<[Option<usize>; 3]>,
    /// For each MOSFET with drain/source ohmic resistance, its internal-node
    /// unknowns in terminal order `[d_int, s_int]` (`None` per terminal whose
    /// resistance is zero: `d_int` exists iff `rd > 0`, `s_int` iff `rs > 0`).
    /// The transistor intrinsic (channel, body junctions, gate charges) moves
    /// onto these, the datasheet-Rds(on) split, with the ohmic resistor
    /// stamped between the external terminal and the internal one, exactly the
    /// BJT `rb`/`re`/`rc` machinery above. Empty when the circuit has no such
    /// MOSFET, so existing circuits pay one `is_empty` check and nothing else;
    /// a FET with `rd == rs == 0` allocates NOTHING, preserving today's node
    /// numbering and bit-identity.
    mos_internal_of: Vec<[Option<usize>; 2]>,
    diode_internal_of: Vec<Option<usize>>,
    /// Mutual-inductance partners per device, indexed by `DeviceId.0`
    /// (dev-plan 04 §2.3): for each inductor in a coupled group, the OTHER
    /// windings it couples to as `(partner, M)` with `M = k·sqrt(L1·L2)`
    /// precomputed from the deck's `Device::Coupling` relationships. Chained
    /// K cards compose pairwise, so a row here IS the off-diagonal of the
    /// group's inductance matrix; the inductor stamp sums over it directly.
    /// EMPTY (zero-length outer vec) when the circuit has no coupling, so a
    /// K-free deck pays one `is_empty` branch per inductor stamp and not one
    /// float op; the bit-identity fast path.
    couplings: Vec<Vec<(DeviceId, f64)>>,
}

impl Layout {
    /// Build the layout for a circuit.
    pub fn new(circuit: &Circuit) -> Layout {
        let named_nodes = circuit.node_count().saturating_sub(1);
        let max_node = circuit.max_node() as usize;
        let (n_real, node_map) = if max_node == named_nodes {
            (named_nodes, NodeMap::Dense)
        } else {
            // Preserve every ordinary named node's historical unknown index,
            // then append only the actually referenced out-of-band ids. This
            // keeps dense circuits bit-identical while a cut node such as
            // NodeId(800_000) costs one row, not 800_000 rows.
            let mut node_of: Vec<NodeId> = (1..circuit.node_count())
                .map(|i| NodeId(i as u32))
                .collect();
            let mut extra: Vec<NodeId> = circuit
                .devices
                .iter()
                .flat_map(Device::nodes)
                .filter(|n| !n.is_ground() && n.0 as usize >= circuit.node_count())
                .collect();
            extra.sort_unstable();
            extra.dedup();
            node_of.extend(extra);
            let unknown_of = node_of
                .iter()
                .copied()
                .enumerate()
                .map(|(i, n)| (n, i))
                .collect();
            let n_real = node_of.len();
            (
                n_real,
                NodeMap::Sparse {
                    unknown_of,
                    node_of,
                },
            )
        };
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
        // MOSFET drain/source internal nodes (the datasheet-Rds(on) split),
        // allocated exactly like the BJT block above and keyed on the MODEL
        // FIELDS ONLY: a FET with rd == rs == 0 allocates nothing, so a default
        // model and every pre-existing deck keep today's node numbering
        // bit-identically. `d_int` exists iff rd > 0, `s_int` iff rs > 0
        // (independent per terminal). Placed after the BJT allocation so a deck
        // with both keeps a deterministic layout (BJT internals then MOS).
        let mut mos_internal_of: Vec<[Option<usize>; 2]> = Vec::new();
        for (_, dev) in circuit.iter() {
            if let Device::Mosfet { model, .. } = dev {
                if model.rd > 0.0 || model.rs > 0.0 {
                    if mos_internal_of.is_empty() {
                        mos_internal_of = vec![[None; 2]; circuit.devices.len()];
                    }
                }
            }
        }
        if !mos_internal_of.is_empty() {
            for (id, dev) in circuit.iter() {
                if let Device::Mosfet { model, .. } = dev {
                    let mut alloc = |r: f64| {
                        if r > 0.0 {
                            let i = next;
                            next += 1;
                            Some(i)
                        } else {
                            None
                        }
                    };
                    mos_internal_of[id.0 as usize] = [alloc(model.rd), alloc(model.rs)];
                }
            }
        }
        // Diode series resistance: one intrinsic anode per diode that sets one,
        // keyed on the MODEL FIELD only, so a diode with rs == 0 allocates
        // nothing and every deck that omits RS keeps today's node numbering
        // bit-identically. Placed last so a deck with all three device kinds
        // has a deterministic layout (BJT internals, then MOS, then diode).
        let mut diode_internal_of: Vec<Option<usize>> = Vec::new();
        for (_, dev) in circuit.iter() {
            if let Device::Diode { model, .. } = dev {
                if model.rs > 0.0 && diode_internal_of.is_empty() {
                    diode_internal_of = vec![None; circuit.devices.len()];
                }
            }
        }
        if !diode_internal_of.is_empty() {
            for (id, dev) in circuit.iter() {
                if let Device::Diode { model, .. } = dev {
                    if model.rs > 0.0 {
                        diode_internal_of[id.0 as usize] = Some(next);
                        next += 1;
                    }
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
            // adds no unknown; it only WRITES into its control source's branch
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
        // Mutual-inductance map from the deck's Coupling relationships. The
        // loader guarantees l1/l2 resolve to inductors; a PROGRAMMATIC circuit
        // (or an extraction pass that copied a Coupling without its windings /
        // without retargeting) violating that is unstampable, and the loud
        // panic here beats a mutual term silently landing on the wrong device.
        let mut couplings: Vec<Vec<(DeviceId, f64)>> = Vec::new();
        for (_, dev) in circuit.iter() {
            if let Device::Coupling { name, l1, l2, k } = dev {
                if couplings.is_empty() {
                    couplings = vec![Vec::new(); circuit.devices.len()];
                }
                let henries_of = |id: DeviceId| -> f64 {
                    match circuit.devices.get(id.0 as usize) {
                        Some(Device::Inductor { henries, .. }) => *henries,
                        other => panic!(
                            "coupling `{name}` references device {id:?} which is \
                             not an inductor in this system ({other:?}); a pass \
                             that extracts sub-circuits must carry both windings \
                             and retarget the coupling's slots"
                        ),
                    }
                };
                let m = k * (henries_of(*l1) * henries_of(*l2)).sqrt();
                couplings[l1.0 as usize].push((*l2, m));
                couplings[l2.0 as usize].push((*l1, m));
            }
        }
        Layout {
            n_nodes,
            size: next,
            external_nodes: n_real,
            node_map,
            branch_of,
            bjt_internal_of,
            mos_internal_of,
            diode_internal_of,
            couplings,
        }
    }

    /// Unknown index of a non-ground node. Ground returns `None`.
    #[inline]
    pub fn node(&self, node: hauksbee_ir::NodeId) -> Option<usize> {
        if node.is_ground() {
            None
        } else {
            match &self.node_map {
                NodeMap::Dense => {
                    let i = node.0 as usize - 1;
                    (i < self.external_nodes).then_some(i)
                }
                NodeMap::Sparse { unknown_of, .. } => unknown_of.get(&node).copied(),
            }
        }
    }

    /// External node id represented by a node-block unknown. Device-private
    /// internal unknowns and branch-current unknowns return `None`.
    #[inline]
    pub fn node_id(&self, unknown: usize) -> Option<NodeId> {
        if unknown >= self.external_nodes {
            return None;
        }
        match &self.node_map {
            NodeMap::Dense => Some(NodeId((unknown + 1) as u32)),
            NodeMap::Sparse { node_of, .. } => node_of.get(unknown).copied(),
        }
    }

    /// Branch-current unknown index for a device, if it owns one.
    #[inline]
    pub fn branch(&self, id: DeviceId) -> Option<usize> {
        self.branch_of[id.0 as usize]
    }

    /// Mutual-inductance partners of an inductor: `(other winding, M)` per
    /// coupling it participates in. The empty slice for every device in a
    /// K-free circuit (one `is_empty` branch, no allocation, no float math,
    /// the fast path the bit-identity bar demands), and for non-inductors.
    #[inline]
    pub fn mutual_partners(&self, id: DeviceId) -> &[(DeviceId, f64)] {
        if self.couplings.is_empty() {
            return &[];
        }
        &self.couplings[id.0 as usize]
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

    /// Internal-node unknowns of a drain/source series-resistance MOSFET, in
    /// terminal order `[d_int, s_int]`. `None` when the device allocated none
    /// (the common case: default models, or not a MOSFET), so a rd/rs-free
    /// circuit takes one `is_empty` branch and nothing else.
    #[inline]
    /// The intrinsic anode of a diode whose model sets a series resistance.
    /// `None` when it sets none and the junction sits on the external anode.
    pub fn diode_internal(&self, id: DeviceId) -> Option<usize> {
        if self.diode_internal_of.is_empty() {
            return None;
        }
        self.diode_internal_of[id.0 as usize]
    }

    pub fn mos_internal(&self, id: DeviceId) -> Option<&[Option<usize>; 2]> {
        if self.mos_internal_of.is_empty() {
            return None;
        }
        let ints = &self.mos_internal_of[id.0 as usize];
        if ints.iter().all(|i| i.is_none()) {
            None
        } else {
            Some(ints)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hauksbee_ir::BjtModel;

    #[test]
    fn sparse_asbuilt_node_id_uses_one_dense_unknown() {
        let mut c = Circuit::new();
        let collector = c.node("collector");
        let floated_base = NodeId(800_000);
        c.add(Device::Bjt {
            name: "Q_cut".into(),
            c: collector,
            b: floated_base,
            e: NodeId::GROUND,
            model: BjtModel::default(),
        });

        let layout = Layout::new(&c);
        assert_eq!(layout.n_nodes, 2, "one named node plus one cut terminal");
        assert_eq!(layout.node(collector), Some(0));
        assert_eq!(layout.node(floated_base), Some(1));
        assert_eq!(layout.node_id(1), Some(floated_base));
    }
}

/// Per-step reactive history a device needs for its companion model.
///
/// Companion (integration) models replace a capacitor/inductor with a
/// conductance in parallel/series with a current source whose value depends on
/// the previous step(s). This struct carries that history per timestep so the
/// stamping code stays stateless.
///
/// Slot packing for multi-state devices (dev-plan 04 §3.2/§3.3): the primary
/// bank (`x1`/`dx1`/`x2`) holds ONE state per device, capacitor voltage,
/// inductor current, a charge-storing diode's junction charge, a BJT's
/// BASE-EMITTER charge, or a MOSFET's GATE-SOURCE charge. Devices with more
/// independent charges spill into the SECONDARY banks `xb[k]`, one extra
/// state each, in a documented per-device order:
///
/// * BJT (two charges):    bank A = Q_be,  `xb[0]` = Q_bc.
/// * MOSFET (up to four):  bank A = Q_gs,  `xb[0]` = Q_gd,
///                         `xb[1]` = Q_bd, `xb[2]` = Q_bs.
///
/// (The §3.2 arc introduced a single named secondary bank for "the one
/// device with two charges"; the MOSFET's four charges are why it is now an
/// indexed array, bank letters do not scale, indices do.) All banks are
/// indexed by `DeviceId.0` and roll forward together; a device leaves the
/// banks it does not use at zero.
#[derive(Debug, Clone, Default)]
pub struct ReactiveState {
    /// Per-device stored voltage (caps) or current (inductors) at the accepted
    /// previous step, indexed by `DeviceId.0`.
    pub x1: Vec<f64>,
    /// Time-derivative at the previous step (for trapezoidal / Gear-2).
    pub dx1: Vec<f64>,
    /// One step further back (for Gear-2's two-point history).
    pub x2: Vec<f64>,
    /// Secondary banks (extra charges of multi-charge devices; see the
    /// packing table above), same indexing as the primary bank.
    pub xb: [SecondaryBank; 3],
}

/// One secondary reactive-history bank: the `x1`/`dx1`/`x2` trio of
/// [`ReactiveState`], for one extra state of a multi-charge device.
#[derive(Debug, Clone, Default)]
pub struct SecondaryBank {
    /// Stored state at the accepted previous step, indexed by `DeviceId.0`.
    pub x1: Vec<f64>,
    /// Time-derivative at the previous step.
    pub dx1: Vec<f64>,
    /// One step further back.
    pub x2: Vec<f64>,
}

impl ReactiveState {
    /// History zeroed for `n` devices.
    pub fn new(n: usize) -> Self {
        let bank = || SecondaryBank {
            x1: vec![0.0; n],
            dx1: vec![0.0; n],
            x2: vec![0.0; n],
        };
        ReactiveState {
            x1: vec![0.0; n],
            dx1: vec![0.0; n],
            x2: vec![0.0; n],
            xb: [bank(), bank(), bank()],
        }
    }
}
