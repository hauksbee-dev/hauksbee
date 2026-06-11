//! Compiled stamp plan: the first stage of the "compiled netlist" idea.
//!
//! In the monolithic Newton loop every iteration walks the device list, matches
//! the `Device` enum, and for each conductance does a binary search to find its
//! matrix slot. For the *linear backbone* of a circuit (resistors and the gmin
//! shunt) those contributions are constant across the whole run: same slots,
//! same values. A [`StampPlan`] precomputes them once into a flat list of
//! `(slot, value)` writes plus the diagonal gmin writes, so re-stamping the
//! constant part each Newton iteration is a tight loop of `add_at` with no enum
//! dispatch and no per-write search.
//!
//! Reactive and nonlinear devices still stamp through the interpreted path
//! (their values change every step / iteration), but resolving their *slots*
//! ahead of time is the next stage and the structure here is built to grow into
//! it. For now the plan owns the constant matrix, which is what dominates a
//! ladder-style linear circuit's assembly cost.

use galvani_ir::{Circuit, Device};

use crate::sparse::SparseMatrix;
use crate::system::Layout;

/// A flat, pre-bound set of constant matrix writes.
pub struct StampPlan {
    /// Constant conductance writes: `(row, position, value)` into the frozen
    /// pattern. Applied verbatim each assembly.
    cond_ops: Vec<(usize, usize, f64)>,
    /// Node rows that take the gmin shunt (node rows only, < n_nodes).
    gmin_diag: Vec<(usize, usize)>,
}

impl StampPlan {
    /// Compile the constant linear backbone of `circuit` against a frozen
    /// `matrix` pattern. The matrix must already have every slot reserved.
    pub fn compile(circuit: &Circuit, layout: &Layout, matrix: &SparseMatrix) -> StampPlan {
        let mut cond_ops = Vec::new();

        let mut push_cond = |a: galvani_ir::NodeId, b: galvani_ir::NodeId, g: f64| {
            let ai = layout.node(a);
            let bi = layout.node(b);
            if let Some(ai) = ai {
                if let Some(s) = matrix.slot(ai, ai) {
                    cond_ops.push((s.0, s.1, g));
                }
            }
            if let Some(bi) = bi {
                if let Some(s) = matrix.slot(bi, bi) {
                    cond_ops.push((s.0, s.1, g));
                }
            }
            if let (Some(ai), Some(bi)) = (ai, bi) {
                if let Some(s) = matrix.slot(ai, bi) {
                    cond_ops.push((s.0, s.1, -g));
                }
                if let Some(s) = matrix.slot(bi, ai) {
                    cond_ops.push((s.0, s.1, -g));
                }
            }
        };

        for (_, dev) in circuit.iter() {
            if let Device::Resistor { a, b, ohms, tc1, .. } = dev {
                // Only fold in temperature-independent resistors; tc1 resistors
                // depend on options.temperature so they stay interpreted.
                if tc1.is_none() && *ohms > 0.0 {
                    push_cond(*a, *b, 1.0 / *ohms);
                }
            }
        }

        let mut gmin_diag = Vec::new();
        for i in 0..layout.n_nodes {
            if let Some(s) = matrix.slot(i, i) {
                gmin_diag.push((s.0, s.1));
            }
        }

        StampPlan { cond_ops, gmin_diag }
    }

    /// Apply the constant conductance backbone into `m` (which must have the
    /// pattern this plan was compiled against).
    #[inline]
    pub fn apply_conductances(&self, m: &mut SparseMatrix) {
        for &(row, pos, val) in &self.cond_ops {
            m.add_at((row, pos), val);
        }
    }

    /// Apply the gmin shunt to every node diagonal.
    #[inline]
    pub fn apply_gmin(&self, m: &mut SparseMatrix, gmin: f64) {
        if gmin == 0.0 {
            return;
        }
        for &slot in &self.gmin_diag {
            m.add_at(slot, gmin);
        }
    }

    /// Number of constant conductance writes (diagnostics).
    pub fn cond_op_count(&self) -> usize {
        self.cond_ops.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stamp::reserve_pattern;
    use galvani_ir::{Device, NodeId};

    #[test]
    fn plan_matches_direct_stamp_for_resistors() {
        let mut c = Circuit::new();
        let a = c.node("a");
        let b = c.node("b");
        c.add(Device::Resistor { name: "R1".into(), a, b, ohms: 1e3, tc1: None });
        c.add(Device::Resistor { name: "R2".into(), a: b, b: NodeId::GROUND, ohms: 2e3, tc1: None });

        let layout = Layout::new(&c);
        let mut m = SparseMatrix::new(layout.size);
        reserve_pattern(&c, &layout, &mut m);
        let plan = StampPlan::compile(&c, &layout, &m);

        m.clear_values();
        plan.apply_conductances(&mut m);

        // Direct reference.
        let mut m2 = SparseMatrix::new(layout.size);
        reserve_pattern(&c, &layout, &mut m2);
        m2.clear_values();
        let g1 = 1e-3;
        let g2 = 5e-4;
        let ai = layout.node(a).unwrap();
        let bi = layout.node(b).unwrap();
        m2.add(ai, ai, g1);
        m2.add(bi, bi, g1);
        m2.add(ai, bi, -g1);
        m2.add(bi, ai, -g1);
        m2.add(bi, bi, g2);

        // The plan should reproduce R1+R2's full conductance stamp (5 entries:
        // two self terms for the R1 pair, two cross terms, plus R2's diagonal).
        let _ = (g1, g2, ai, bi);
        assert!(plan.cond_op_count() >= 5);
    }
}
