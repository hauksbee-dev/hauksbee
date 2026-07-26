//! Linear-island fast path: state-space `x' = A x + B u` advanced by a cached
//! matrix exponential; the Tarski closed-form trick, generalized.
//!
//! A purely linear island (resistors, capacitors, inductors, current sources)
//! has a finite-dimensional linear state: every capacitor contributes its
//! voltage and every inductor its current. The resistive part of the network is
//! algebraic and is eliminated, leaving an ODE
//!
//! ```text
//!     x' = A x + B u
//! ```
//!
//! where `x` is the stacked cap-voltage / inductor-current state and `u` is the
//! vector of boundary inputs (ideal-source node voltages and shared boundary
//! nodes), held at their step value (zero-order hold). For a **fixed** timestep
//! `dt` the exact ZOH update is
//!
//! ```text
//!     x(t+dt) = Ad x(t) + Bd u,   Ad = exp(A dt),  Bd = A^{-1}(Ad - I) B.
//! ```
//!
//! Both `Ad` and `Bd` come from one exponential of the augmented block
//! `[[A, B],[0, 0]]` (Van Loan), so `A^{-1}` is never formed even when `A` is
//! singular. We compute that exponential once per `dt` by scaling-and-squaring
//! and cache it; every step is then a single dense mat-vec. Islands are small,
//! so the dense exponential is cheap and amortizes over thousands of steps.
//!
//! ## Building A, B and the output map
//!
//! We assemble the island's resistive system once with capacitors entered as
//! voltage constraints (an MNA branch fixing `v_a - v_b` to the cap's state) and
//! inductors entered as current injections (their current is the state). One
//! dense LU factorization then yields, for unit excitation of each state and
//! each input, the resulting free-node voltages and the current the network
//! delivers into each capacitor. From those:
//!
//! * a capacitor's `v' = i_C / C` gives a row of `A` (state columns) and `B`
//!   (input columns);
//! * an inductor's `i' = (v_a - v_b) / L`, read from the reconstructed node
//!   voltages, gives its row.
//!
//! The same solve gives `Sx`, `Su` so node voltages can be reconstructed for
//! probing and inter-island coupling: `v_free = Sx x + Su u`.
//!
//! Long-form how-and-why (motivation, theory, rejected alternatives, the
//! buried bodies): docs/how-and-why/hauksbee-solve/linear.md

use hauksbee_ir::{Circuit, Device, DeviceId, NodeId};

use crate::partition::Island;

/// A linear island compiled to state-space form, with a per-`dt` exponential
/// cache.
pub struct LinearIsland {
    states: Vec<(DeviceId, StateKind)>,
    inputs: Vec<NodeId>,
    /// Current sources inside the island: extra input columns after the
    /// voltage inputs, evaluated by the caller per step (ZOH).
    isources: Vec<(DeviceId, NodeId, NodeId)>,
    /// Continuous `A` (n_states x n_states), row-major.
    a: Vec<f64>,
    /// Continuous `B` (n_states x n_inputs), row-major.
    b: Vec<f64>,
    /// Node id -> island-local free-node index, sized `n_nodes+1`.
    node_local: Vec<Option<usize>>,
    n_free: usize,
    /// Free-node reconstruction: `v_free = Sx x + Su u`.
    sx: Vec<f64>,
    su: Vec<f64>,
    /// Sparse CSR of `A` (state coupling), for the matrix-free advance used on
    /// large islands where a dense `exp(A dt)` would be ruinous.
    a_sparse: Csr,
    /// Sparse CSR of the free-node reconstruction `Sx` (n_free x n_states), used
    /// for large islands so probing doesn't cost a dense n_free*n_states matvec.
    sx_sparse: Rect,
    cache: Option<ExpCache>,
    /// Matrix-free propagator parameters, cached per dt (substep count + Taylor
    /// order). Used when `n_states` exceeds [`DENSE_EXPM_MAX`].
    mf: Option<MatrixFree>,
}

/// Above this state count, a dense `exp(A dt)` (O(n^3) to build, O(n^2)/step to
/// apply) loses to a matrix-free sparse advance, so we switch modes. Small
/// islands keep the dense cached exponential, one mat-vec per step, unbeatable.
const DENSE_EXPM_MAX: usize = 48;

/// Compressed sparse row matrix (square, `n x n`).
struct Csr {
    n: usize,
    row_ptr: Vec<usize>,
    col: Vec<usize>,
    val: Vec<f64>,
}

impl Csr {
    fn from_dense(a: &[f64], n: usize) -> Csr {
        let mut row_ptr = vec![0usize; n + 1];
        let mut col = Vec::new();
        let mut val = Vec::new();
        for i in 0..n {
            for j in 0..n {
                let v = a[i * n + j];
                if v != 0.0 {
                    col.push(j);
                    val.push(v);
                }
            }
            row_ptr[i + 1] = col.len();
        }
        Csr {
            n,
            row_ptr,
            col,
            val,
        }
    }

    /// `y = A x`.
    #[inline]
    fn matvec(&self, x: &[f64], y: &mut [f64]) {
        for i in 0..self.n {
            let mut s = 0.0;
            for p in self.row_ptr[i]..self.row_ptr[i + 1] {
                s += self.val[p] * x[self.col[p]];
            }
            y[i] = s;
        }
    }

    /// Estimated 1-norm (max abs row sum, a cheap proxy for scaling).
    fn norm_inf(&self) -> f64 {
        let mut m = 0.0f64;
        for i in 0..self.n {
            let mut s = 0.0;
            for p in self.row_ptr[i]..self.row_ptr[i + 1] {
                s += self.val[p].abs();
            }
            m = m.max(s);
        }
        m
    }
}

/// A sparse rectangular `rows x cols` matrix in CSR form (for `Sx`).
struct Rect {
    rows: usize,
    cols: usize,
    row_ptr: Vec<usize>,
    col: Vec<usize>,
    val: Vec<f64>,
}

impl Rect {
    fn from_dense(a: &[f64], rows: usize, cols: usize) -> Rect {
        let mut row_ptr = vec![0usize; rows + 1];
        let mut col = Vec::new();
        let mut val = Vec::new();
        let drop = 1e-300;
        for i in 0..rows {
            for j in 0..cols {
                let v = a[i * cols + j];
                if v.abs() > drop {
                    col.push(j);
                    val.push(v);
                }
            }
            row_ptr[i + 1] = col.len();
        }
        Rect {
            rows,
            cols,
            row_ptr,
            col,
            val,
        }
    }

    /// `y = M x` where `x` has `cols` entries and `y` has `rows`.
    #[inline]
    fn matvec(&self, x: &[f64], y: &mut [f64]) {
        let _ = self.cols;
        for i in 0..self.rows {
            let mut s = 0.0;
            for p in self.row_ptr[i]..self.row_ptr[i + 1] {
                s += self.val[p] * x[self.col[p]];
            }
            y[i] = s;
        }
    }
}

/// Matrix-free propagator for one fixed step: substep count `2^s` and Taylor
/// order, chosen so each micro-propagator `exp(A dt/2^s)` is machine-exact.
struct MatrixFree {
    dt: f64,
    substeps: usize,
    order: usize,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum StateKind {
    Cap { a: NodeId, b: NodeId, c: f64 },
    Ind { a: NodeId, b: NodeId, l: f64 },
}

struct ExpCache {
    dt: f64,
    ad: Vec<f64>,
    bd: Vec<f64>,
}

enum Slot {
    Ground,
    Free(usize),
    Input(usize),
}

impl LinearIsland {
    /// Number of linear states (caps + inductors).
    pub fn n_states(&self) -> usize {
        self.states.len()
    }

    /// Number of free (solvable) nodes owned by this island.
    pub fn n_free(&self) -> usize {
        self.n_free
    }

    /// Input nodes, in `u` order.
    pub fn inputs(&self) -> &[NodeId] {
        &self.inputs
    }

    /// Current sources whose values the caller must append to the input
    /// vector, in order, after the voltage inputs.
    pub fn isources(&self) -> &[(DeviceId, NodeId, NodeId)] {
        &self.isources
    }

    /// Total input vector length: voltage inputs then current inputs.
    pub fn n_inputs_total(&self) -> usize {
        self.inputs.len() + self.isources.len()
    }

    /// The island-local index for a node, if it is a free node here.
    pub fn local_of(&self, n: NodeId) -> Option<usize> {
        self.node_local.get(n.0 as usize).copied().flatten()
    }

    /// State devices with a flag: `true` = capacitor (voltage state), `false`
    /// = inductor (current state).
    pub fn state_devices(&self) -> impl Iterator<Item = (DeviceId, bool)> + '_ {
        self.states
            .iter()
            .map(|(id, k)| (*id, matches!(k, StateKind::Cap { .. })))
    }

    /// Try to compile a linear island to state-space. Returns `None` if the
    /// island has no dynamics or a structure the dense reducer can't factor;
    /// the caller then routes it through MNA.
    ///
    /// `temp_c` is the EFFECTIVE model temperature
    /// ([`crate::SolverOptions::model_temp`]): the reducer bakes device values
    /// into its A/B matrices at compile time, so a `tc1`-bearing resistor must
    /// be evaluated at the same temperature the monolithic stamp uses
    /// (`stamp::resistor_value`) or the fast path silently disagrees with the
    /// reference engine on every thermal sweep. Callers with temperature
    /// effects off pass TNOM (27), which makes the derating a no-op.
    pub fn compile(
        circuit: &Circuit,
        island: &Island,
        gmin: f64,
        temp_c: f64,
    ) -> Option<LinearIsland> {
        if !island.linear {
            return None;
        }
        let n_nodes = circuit.max_node() as usize;

        let mut is_input = vec![false; n_nodes + 1];
        for n in &island.boundary_in {
            is_input[n.0 as usize] = true;
        }
        let inputs: Vec<NodeId> = island.boundary_in.clone();

        let mut node_local: Vec<Option<usize>> = vec![None; n_nodes + 1];
        let mut n_free = 0;
        for n in &island.nodes {
            let ni = n.0 as usize;
            if !is_input[ni] && node_local[ni].is_none() {
                node_local[ni] = Some(n_free);
                n_free += 1;
            }
        }

        // Exhaustive device walk, no `_` arm ON PURPOSE. This reducer models
        // exactly R (algebraic), C/L (states), and Isource (input columns);
        // any other kind in a linear-classified island must route the WHOLE
        // island back to MNA by returning `None`. A catch-all here is the most
        // dangerous line in the solver: a device that `is_linear()` but is not
        // assembled below (a VCVS in an RC chain) would leave the island on the
        // matrix-exponential path with that device silently absent from the A
        // matrix, a plausible wrong waveform, not a crash (see
        // `docs/dev-plans/04-spice-compat.md` §1, the compile row). Modeling
        // controlled sources inside the state-space reduction is future
        // optimization work with its own exactness gate; refusing is exact
        // today because the MNA sub-solve stamps them in full.
        let mut states: Vec<(DeviceId, StateKind)> = Vec::new();
        for &id in &island.devices {
            match &circuit.devices[id.0 as usize] {
                Device::Capacitor { a, b, farads, .. } => {
                    // A non-positive capacitance would divide the A/B rows below
                    // (`w[..] / *c`) by zero/negative, poisoning the discretized
                    // matrices with Inf/NaN and, because this fast path skips
                    // Newton's per-iterate finite check, streaming a silently
                    // wrong waveform. Refuse; the MNA sub-solve stamps c<=0 as an
                    // open (geq = 0) exactly, the same honest-answer discipline
                    // as the E/G/coupled refusals below.
                    if *farads <= 0.0 {
                        return None;
                    }
                    states.push((
                        id,
                        StateKind::Cap {
                            a: *a,
                            b: *b,
                            c: *farads,
                        },
                    ));
                }
                Device::Inductor { a, b, henries, .. } => {
                    // Symmetric to the capacitor above: a non-positive inductance
                    // divides the `vl / *l` rows by zero. The MNA path stamps an
                    // l<=0 inductor as an ideal-short branch exactly, so refuse
                    // and defer to it rather than fabricate Inf/NaN dynamics.
                    if *henries <= 0.0 {
                        return None;
                    }
                    states.push((
                        id,
                        StateKind::Ind {
                            a: *a,
                            b: *b,
                            l: *henries,
                        },
                    ));
                }
                // Modeled by the dedicated loops below (resistive backbone,
                // current-input columns).
                Device::Resistor { .. } | Device::Isource { .. } => {}
                // Linear but NOT modeled by this reducer: force the island to
                // the MNA path. (Ideal Vsources are cut by the partitioner and
                // never land in an island, EXCEPT one demoted to island
                // member because an F/H reads its branch current; refusing is
                // the only honest answer for it too.) F/H join E/G here:
                // constant-gain, linear, and absent from the reducer's
                // A-matrix vocabulary, compiling past them would be the
                // silent-drop hazard of 04-spice-compat.md §1; the MNA
                // sub-solve stamps them exactly.
                Device::Vcvs { .. }
                | Device::Vccs { .. }
                | Device::Cccs { .. }
                | Device::Ccvs { .. }
                | Device::Vsource { .. } => {
                    return None;
                }
                // Coupled inductors are linear-with-state but NOT modeled by
                // this reducer: its inductor states assume `di/dt = v/L` per
                // winding, and a coupled group's `di/dt = L⁻¹·v` needs the
                // group inductance matrix INVERTED, which k = 1 (legal on
                // the K card) makes singular. The MNA path stamps L directly
                // and never inverts it, so refusing the island is both exact
                // and the only shape that survives perfect coupling. Modeling
                // k < 1 groups in the reduction is a later optimization with
                // its own exactness gate (the E/G precedent).
                Device::Coupling { .. } => return None,
                // Nonlinear / event-driven kinds cannot appear in a linear
                // island (the `island.linear` gate above already returned
                // `None`), but the match stays exhaustive so a future variant
                // must be placed deliberately. The behavioral B-source is
                // nonlinear by construction (`is_linear() == false` taints the
                // island), so it can only reach this walk through that gate,
                // and refusing is still the only honest answer here.
                Device::Diode { .. }
                | Device::Bjt { .. }
                | Device::Mosfet { .. }
                | Device::VSwitch { .. }
                | Device::OpAmp { .. }
                | Device::Comparator { .. }
                | Device::Behavioral { .. } => return None,
            }
        }
        if states.is_empty() {
            return None;
        }

        let mut isources: Vec<(DeviceId, NodeId, NodeId)> = Vec::new();
        for &id in &island.devices {
            if let Device::Isource { p, n, .. } = &circuit.devices[id.0 as usize] {
                isources.push((id, *p, *n));
            }
        }

        let n_states = states.len();
        let n_in = inputs.len();
        let n_in_t = n_in + isources.len();

        // Augmented MNA unknowns: [free nodes (n_free) | cap branches (n_cap)].
        let n_cap = states
            .iter()
            .filter(|(_, k)| matches!(k, StateKind::Cap { .. }))
            .count();
        let dim = n_free + n_cap;
        if dim == 0 {
            return None;
        }
        let mi = |r: usize, c: usize| r * dim + c;
        let mut m = vec![0.0f64; dim * dim];
        for f in 0..n_free {
            m[mi(f, f)] += gmin;
        }

        // RHS columns: [states (n_states) | voltage inputs (n_in) | current
        // inputs (n_isrc)] so one factorization serves every excitation.
        let rhs_cols = n_states + n_in_t;
        let ri = |r: usize, c: usize| r * rhs_cols + c;
        let mut rhs = vec![0.0f64; dim * rhs_cols];

        let input_pos = |n: NodeId| -> Option<usize> { inputs.iter().position(|&x| x == n) };
        let slot = |n: NodeId| -> Slot {
            if n.is_ground() {
                Slot::Ground
            } else if let Some(p) = input_pos(n) {
                Slot::Input(p)
            } else if let Some(l) = node_local[n.0 as usize] {
                Slot::Free(l)
            } else {
                Slot::Ground
            }
        };

        // Resistors: conductance into M, input nodes drive the RHS. The
        // resistance is temperature-derated exactly as the monolithic stamp
        // does (`stamp::resistor_value`): nominal ohms scaled by
        // 1 + tc1·(T − TNOM). `temp_c` already encodes the effects.temperature
        // gate (it is TNOM when the effect is off), so the formula below is
        // byte-identical to the reference path in both modes.
        for &id in &island.devices {
            if let Device::Resistor {
                a, b, ohms, tc1, ..
            } = &circuit.devices[id.0 as usize]
            {
                let r = match tc1 {
                    Some(tc) => *ohms * (1.0 + tc * (temp_c - 27.0)),
                    None => *ohms,
                };
                // Non-positive resistance = SHORT (matching the interpreted
                // stamp): clamp to the shared 1e-6 Ω floor and stamp a stiff
                // conductance rather than skipping, which would silently turn
                // a 0-Ω jumper into an open on this fast path only.
                let g = 1.0 / r.max(1e-6);
                match (slot(*a), slot(*b)) {
                    (Slot::Free(fa), Slot::Free(fb)) => {
                        m[mi(fa, fa)] += g;
                        m[mi(fb, fb)] += g;
                        m[mi(fa, fb)] -= g;
                        m[mi(fb, fa)] -= g;
                    }
                    (Slot::Free(f), Slot::Ground) | (Slot::Ground, Slot::Free(f)) => {
                        m[mi(f, f)] += g;
                    }
                    (Slot::Free(f), Slot::Input(p)) | (Slot::Input(p), Slot::Free(f)) => {
                        m[mi(f, f)] += g;
                        rhs[ri(f, n_states + p)] += g;
                    }
                    _ => {}
                }
            }
        }

        // Current sources: a unit of source k injects +1 into its n node and
        // pulls -1 from its p node (matching the monolithic stamp convention).
        for (k, (_, ip, inn)) in isources.iter().enumerate() {
            let col = n_states + n_in + k;
            if let Slot::Free(f) = slot(*ip) {
                rhs[ri(f, col)] -= 1.0;
            }
            if let Slot::Free(f) = slot(*inn) {
                rhs[ri(f, col)] += 1.0;
            }
        }

        // Capacitor voltage constraints (one MNA branch each).
        let mut cap_branch_row = vec![usize::MAX; n_states];
        {
            let mut br = n_free;
            for (si, (_, k)) in states.iter().enumerate() {
                if let StateKind::Cap { a, b, .. } = k {
                    cap_branch_row[si] = br;
                    if let Slot::Free(fa) = slot(*a) {
                        m[mi(fa, br)] += 1.0;
                        m[mi(br, fa)] += 1.0;
                    }
                    if let Slot::Free(fb) = slot(*b) {
                        m[mi(fb, br)] -= 1.0;
                        m[mi(br, fb)] -= 1.0;
                    }
                    rhs[ri(br, si)] += 1.0;
                    if let Slot::Input(p) = slot(*a) {
                        rhs[ri(br, n_states + p)] -= 1.0;
                    }
                    if let Slot::Input(p) = slot(*b) {
                        rhs[ri(br, n_states + p)] += 1.0;
                    }
                    br += 1;
                }
            }
        }

        // Inductor states inject their current between nodes.
        for (si, (_, k)) in states.iter().enumerate() {
            if let StateKind::Ind { a, b, .. } = k {
                if let Slot::Free(fa) = slot(*a) {
                    rhs[ri(fa, si)] -= 1.0;
                }
                if let Slot::Free(fb) = slot(*b) {
                    rhs[ri(fb, si)] += 1.0;
                }
            }
        }

        // Factor once; solve for every excitation column.
        let lu = DenseLu::factor(&m, dim)?;
        let mut w = vec![0.0f64; dim * rhs_cols];
        let mut col = vec![0.0f64; dim];
        for c in 0..rhs_cols {
            for r in 0..dim {
                col[r] = rhs[ri(r, c)];
            }
            lu.solve(&mut col);
            for r in 0..dim {
                w[r * rhs_cols + c] = col[r];
            }
        }
        let wi = |r: usize, c: usize| r * rhs_cols + c;

        // Reconstruction maps.
        let mut sx = vec![0.0f64; n_free * n_states];
        let mut su = vec![0.0f64; n_free * n_in_t];
        for f in 0..n_free {
            for s in 0..n_states {
                sx[f * n_states + s] = w[wi(f, s)];
            }
            for p in 0..n_in_t {
                su[f * n_in_t + p] = w[wi(f, n_states + p)];
            }
        }

        // Helper: reconstructed voltage of `node` in solution column `col`,
        // where `col` is a state column (is_input=false) or input column.
        let node_v = |node: NodeId, c: usize, is_input: bool| -> f64 {
            if node.is_ground() {
                0.0
            } else if let Some(p) = input_pos(node) {
                if is_input && c == p {
                    1.0
                } else {
                    0.0
                }
            } else if let Some(fl) = node_local[node.0 as usize] {
                let gcol = if is_input { n_states + c } else { c };
                w[wi(fl, gcol)]
            } else {
                0.0
            }
        };

        // Assemble A, B.
        let mut a_mat = vec![0.0f64; n_states * n_states];
        let mut b_mat = vec![0.0f64; n_states * n_in_t];
        for (si, (_, k)) in states.iter().enumerate() {
            match k {
                StateKind::Cap { c, .. } => {
                    // i_C = cap branch current (a->b) the network delivers; v' = i_C/C.
                    let br = cap_branch_row[si];
                    for s2 in 0..n_states {
                        a_mat[si * n_states + s2] = w[wi(br, s2)] / *c;
                    }
                    for p in 0..n_in_t {
                        b_mat[si * n_in_t + p] = w[wi(br, n_states + p)] / *c;
                    }
                }
                StateKind::Ind { a, b, l } => {
                    // v_L = v_a - v_b reconstructed; i' = v_L / L.
                    for s2 in 0..n_states {
                        let vl = node_v(*a, s2, false) - node_v(*b, s2, false);
                        a_mat[si * n_states + s2] = vl / *l;
                    }
                    for p in 0..n_in_t {
                        let vl = node_v(*a, p, true) - node_v(*b, p, true);
                        b_mat[si * n_in_t + p] = vl / *l;
                    }
                }
            }
        }

        let a_sparse = Csr::from_dense(&a_mat, n_states);
        let sx_sparse = Rect::from_dense(&sx, n_free, n_states);

        Some(LinearIsland {
            states,
            inputs,
            isources,
            a: a_mat,
            b: b_mat,
            node_local,
            n_free,
            sx,
            su,
            a_sparse,
            sx_sparse,
            cache: None,
            mf: None,
        })
    }

    /// True if this island uses the matrix-free advance (large island).
    fn uses_matrix_free(&self) -> bool {
        self.states.len() > DENSE_EXPM_MAX
    }

    /// Ensure the discrete update for `dt` is prepared. Small islands build a
    /// dense `[Ad|Bd]`; large islands prepare matrix-free propagator parameters.
    pub fn ensure_cache(&mut self, dt: f64) {
        if self.uses_matrix_free() {
            if let Some(mf) = &self.mf {
                if mf.dt == dt {
                    return;
                }
            }
            // Choose substeps so ||A dt / 2^s|| <= 0.5, then a Taylor order that
            // is machine-exact at that norm (order ~ -log2(eps) terms suffices).
            let norm = self.a_sparse.norm_inf() * dt.abs();
            let s = if norm > 0.5 {
                (norm.log2().ceil() as i32 + 1).max(0) as usize
            } else {
                0
            };
            self.mf = Some(MatrixFree {
                dt,
                substeps: 1usize << s,
                order: 16,
            });
            return;
        }
        if let Some(c) = &self.cache {
            if c.dt == dt {
                return;
            }
        }
        let n = self.states.len();
        let m = self.n_inputs_total();
        let (ad, bd) = discretize(&self.a, &self.b, n, m, dt);
        self.cache = Some(ExpCache { dt, ad, bd });
    }

    /// Advance the state one step.
    pub fn step(&self, x: &mut [f64], u: &[f64]) {
        if self.uses_matrix_free() {
            self.step_matrix_free(x, u);
        } else {
            self.step_dense(x, u);
        }
    }

    /// Dense advance: `x <- Ad x + Bd u` (one mat-vec). Small islands.
    fn step_dense(&self, x: &mut [f64], u: &[f64]) {
        let cache = self.cache.as_ref().expect("ensure_cache first");
        let n = self.states.len();
        let m = self.n_inputs_total();
        let mut nx = vec![0.0f64; n];
        for i in 0..n {
            let mut s = 0.0;
            let row = &cache.ad[i * n..i * n + n];
            for j in 0..n {
                s += row[j] * x[j];
            }
            let brow = &cache.bd[i * m..i * m + m];
            for j in 0..m {
                s += brow[j] * u[j];
            }
            nx[i] = s;
        }
        x.copy_from_slice(&nx);
    }

    /// Matrix-free advance for large islands. The ZOH update over `dt` is
    /// `x' = A x + d`, `d = B u` constant. We integrate it exactly with `2^s`
    /// substeps of `exp(A h)` applied via a Taylor series to the vector, where
    /// each substep also folds in the constant drive: for `y' = A y + d`,
    /// `y(h) = exp(Ah) y(0) + (exp(Ah) - I) A^{-1} d`. We avoid `A^{-1}` by
    /// augmenting: track `[y; 1]` with the constant row, i.e. apply the Taylor
    /// recurrence to `(A y + d)`. Concretely each substep does
    /// `y <- y + sum_{k>=1} (h^k/k!) A^{k-1}(A y + d)`.
    fn step_matrix_free(&self, x: &mut [f64], u: &[f64]) {
        let mf = self.mf.as_ref().expect("ensure_cache first");
        let n = self.states.len();
        let m = self.n_inputs_total();
        // Constant drive d = B u (dense B, but n_inputs is tiny).
        let mut d = vec![0.0f64; n];
        for i in 0..n {
            let mut s = 0.0;
            let brow = &self.b[i * m..i * m + m];
            for j in 0..m {
                s += brow[j] * u[j];
            }
            d[i] = s;
        }
        let h = mf.dt / mf.substeps as f64;
        let mut y = x.to_vec();
        let mut term = vec![0.0f64; n]; // running A^{k-1}(A y + d)
        let mut ay = vec![0.0f64; n];
        let mut tmp = vec![0.0f64; n];
        for _ in 0..mf.substeps {
            // f0 = A y + d
            self.a_sparse.matvec(&y, &mut ay);
            for i in 0..n {
                term[i] = ay[i] + d[i];
            }
            // y <- y + sum_{k=1..order} h^k/k! * A^{k-1} f0
            let mut coef = 1.0f64;
            for k in 1..=mf.order {
                coef *= h / k as f64;
                for i in 0..n {
                    y[i] += coef * term[i];
                }
                // term <- A term
                if k < mf.order {
                    self.a_sparse.matvec(&term, &mut tmp);
                    std::mem::swap(&mut term, &mut tmp);
                }
            }
        }
        x.copy_from_slice(&y);
    }

    /// Reconstruct free-node voltages from state and inputs into `out_v`
    /// (indexed by island-local free-node index).
    pub fn node_voltages(&self, x: &[f64], u: &[f64], out_v: &mut [f64]) {
        let n = self.states.len();
        let m = self.n_inputs_total();
        if self.uses_matrix_free() {
            // Sparse Sx (large island); dense Su (n_inputs is tiny).
            self.sx_sparse.matvec(x, out_v);
            for f in 0..self.n_free {
                let mut s = 0.0;
                for j in 0..m {
                    s += self.su[f * m + j] * u[j];
                }
                out_v[f] += s;
            }
            return;
        }
        for f in 0..self.n_free {
            let mut s = 0.0;
            for j in 0..n {
                s += self.sx[f * n + j] * x[j];
            }
            for j in 0..m {
                s += self.su[f * m + j] * u[j];
            }
            out_v[f] = s;
        }
    }
}

// --- dense linear algebra (tiny; islands are small) -------------------------

/// Dense LU with partial pivoting over a row-major `n x n` matrix.
struct DenseLu {
    n: usize,
    lu: Vec<f64>,
    piv: Vec<usize>,
}

impl DenseLu {
    fn factor(a: &[f64], n: usize) -> Option<DenseLu> {
        let mut lu = a.to_vec();
        let mut piv: Vec<usize> = (0..n).collect();
        for k in 0..n {
            let mut p = k;
            let mut best = lu[k * n + k].abs();
            for i in (k + 1)..n {
                let v = lu[i * n + k].abs();
                if v > best {
                    best = v;
                    p = i;
                }
            }
            if best < 1e-300 {
                return None;
            }
            if p != k {
                for c in 0..n {
                    lu.swap(k * n + c, p * n + c);
                }
                piv.swap(k, p);
            }
            let akk = lu[k * n + k];
            for i in (k + 1)..n {
                let f = lu[i * n + k] / akk;
                lu[i * n + k] = f;
                for c in (k + 1)..n {
                    lu[i * n + c] -= f * lu[k * n + c];
                }
            }
        }
        Some(DenseLu { n, lu, piv })
    }

    fn solve(&self, b: &mut [f64]) {
        let n = self.n;
        let mut y = vec![0.0f64; n];
        for i in 0..n {
            y[i] = b[self.piv[i]];
        }
        for i in 0..n {
            let mut s = y[i];
            for j in 0..i {
                s -= self.lu[i * n + j] * y[j];
            }
            y[i] = s;
        }
        for i in (0..n).rev() {
            let mut s = y[i];
            for j in (i + 1)..n {
                s -= self.lu[i * n + j] * y[j];
            }
            y[i] = s / self.lu[i * n + i];
        }
        b[..n].copy_from_slice(&y[..n]);
    }
}

/// Discretize `(A,B)` over `dt` via the augmented matrix exponential (Van Loan):
/// `expm([[A,B],[0,0]] dt) = [[Ad, Bd],[0, I]]`.
fn discretize(a: &[f64], b: &[f64], n: usize, m: usize, dt: f64) -> (Vec<f64>, Vec<f64>) {
    let dim = n + m;
    let mut aug = vec![0.0f64; dim * dim];
    for i in 0..n {
        for j in 0..n {
            aug[i * dim + j] = a[i * n + j] * dt;
        }
        for j in 0..m {
            aug[i * dim + (n + j)] = b[i * m + j] * dt;
        }
    }
    let e = expm(&aug, dim);
    let mut ad = vec![0.0f64; n * n];
    let mut bd = vec![0.0f64; n * m];
    for i in 0..n {
        for j in 0..n {
            ad[i * n + j] = e[i * dim + j];
        }
        for j in 0..m {
            bd[i * m + j] = e[i * dim + (n + j)];
        }
    }
    (ad, bd)
}

/// Dense matrix exponential by scaling-and-squaring with a truncated Taylor
/// core. Scaling drives `||A/2^s||_1 <= 0.5`, where ~18 Taylor terms are
/// machine-exact; squaring `s` times recovers `exp(A)`.
fn expm(a: &[f64], n: usize) -> Vec<f64> {
    if n == 0 {
        return Vec::new();
    }
    let mut norm = 0.0f64;
    for j in 0..n {
        let mut s = 0.0;
        for i in 0..n {
            s += a[i * n + j].abs();
        }
        norm = norm.max(s);
    }
    let s = if norm > 0.5 {
        (norm.log2().ceil() as i32 + 1).max(0)
    } else {
        0
    };
    let scale = 2f64.powi(-s);
    let mut sa = a.to_vec();
    for v in sa.iter_mut() {
        *v *= scale;
    }
    let mut m = expm_taylor(&sa, n, 18);
    for _ in 0..s {
        m = matmul(&m, &m, n);
    }
    m
}

/// Taylor series `I + A + A^2/2! + ...` for a well-scaled `A`.
fn expm_taylor(a: &[f64], n: usize, terms: usize) -> Vec<f64> {
    let mut result = identity(n);
    // `power` holds A^k; `coef` holds 1/k!.
    let mut power = identity(n);
    let mut coef = 1.0f64;
    for k in 1..=terms {
        power = matmul(&power, a, n);
        coef /= k as f64;
        for (r, p) in result.iter_mut().zip(power.iter()) {
            *r += coef * *p;
        }
    }
    result
}

fn identity(n: usize) -> Vec<f64> {
    let mut m = vec![0.0f64; n * n];
    for i in 0..n {
        m[i * n + i] = 1.0;
    }
    m
}

fn matmul(a: &[f64], b: &[f64], n: usize) -> Vec<f64> {
    let mut c = vec![0.0f64; n * n];
    for i in 0..n {
        for k in 0..n {
            let aik = a[i * n + k];
            if aik == 0.0 {
                continue;
            }
            let arow = i * n;
            let brow = k * n;
            for j in 0..n {
                c[arow + j] += aik * b[brow + j];
            }
        }
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;
    use hauksbee_ir::{Circuit, Device, NodeId, SourceKind};

    #[test]
    fn expm_identity_of_zero() {
        let e = expm(&vec![0.0; 4], 2);
        assert!((e[0] - 1.0).abs() < 1e-12);
        assert!((e[3] - 1.0).abs() < 1e-12);
        assert!(e[1].abs() < 1e-12);
    }

    #[test]
    fn expm_scalar_decay() {
        let e = expm(&[-1.0], 1);
        assert!((e[0] - (-1.0f64).exp()).abs() < 1e-9, "{}", e[0]);
    }

    #[test]
    fn expm_2x2_rotation() {
        let e = expm(&[0.0, -1.0, 1.0, 0.0], 2);
        let (c1, s1) = (1.0f64.cos(), 1.0f64.sin());
        assert!((e[0] - c1).abs() < 1e-8, "{e:?}");
        assert!((e[1] + s1).abs() < 1e-8, "{e:?}");
        assert!((e[2] - s1).abs() < 1e-8, "{e:?}");
        assert!((e[3] - c1).abs() < 1e-8, "{e:?}");
    }

    #[test]
    fn expm_large_norm_scaled() {
        // A = -1000 ; exp(-10) over dt to stress scaling-and-squaring.
        let e = expm(&[-10.0], 1);
        assert!((e[0] - (-10.0f64).exp()).abs() < 1e-9, "{}", e[0]);
    }

    fn rc() -> Circuit {
        let mut c = Circuit::new();
        let vin = c.node("in");
        let out = c.node("out");
        c.add(Device::Vsource {
            name: "V1".into(),
            p: vin,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(1.0),
        });
        c.add(Device::Resistor {
            name: "R".into(),
            a: vin,
            b: out,
            ohms: 1e3,
            tc1: None,
        });
        c.add(Device::Capacitor {
            name: "C".into(),
            a: out,
            b: NodeId::GROUND,
            farads: 1e-6,
            ic: Some(0.0),
        });
        c
    }

    #[test]
    fn single_rc_state_space() {
        let c = rc();
        let part = crate::partition::Partition::analyze(&c);
        let li = LinearIsland::compile(&c, &part.islands[0], 0.0, 27.0).expect("compile");
        assert_eq!(li.n_states(), 1);
        assert!((li.a[0] + 1000.0).abs() < 1e-6, "A={}", li.a[0]);
        assert!((li.b[0] - 1000.0).abs() < 1e-6, "B={}", li.b[0]);
    }

    #[test]
    fn rc_step_matches_analytic() {
        let c = rc();
        let part = crate::partition::Partition::analyze(&c);
        let mut li = LinearIsland::compile(&c, &part.islands[0], 0.0, 27.0).unwrap();
        let dt = 1e-5;
        li.ensure_cache(dt);
        let mut x = vec![0.0];
        let u = vec![1.0];
        let tau = 1e-3;
        for step in 1..=100 {
            li.step(&mut x, &u);
            let t = step as f64 * dt;
            let want = 1.0 - (-t / tau).exp();
            assert!((x[0] - want).abs() < 1e-4, "t={t} got {} want {want}", x[0]);
        }
    }

    #[test]
    fn zero_farad_cap_refuses_linearization() {
        // Bug-hunt #3: a capacitor with farads == 0 would divide the A/B state
        // rows by zero (`w[..] / *c`), poisoning the matrix-exponential fast
        // path with Inf/NaN with no Newton finite-check to catch it. compile
        // must REFUSE (return None) so the island falls back to the MNA path,
        // which stamps a 0 F cap as an open exactly.
        let mut c = Circuit::new();
        let vin = c.node("in");
        let out = c.node("out");
        c.add(Device::Vsource {
            name: "V1".into(),
            p: vin,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(1.0),
        });
        c.add(Device::Resistor {
            name: "R".into(),
            a: vin,
            b: out,
            ohms: 1e3,
            tc1: None,
        });
        c.add(Device::Capacitor {
            name: "C".into(),
            a: out,
            b: NodeId::GROUND,
            farads: 0.0,
            ic: Some(0.0),
        });
        let part = crate::partition::Partition::analyze(&c);
        assert!(
            LinearIsland::compile(&c, &part.islands[0], 0.0, 27.0).is_none(),
            "compile must refuse a zero-farad cap island rather than divide by zero"
        );
    }

    #[test]
    fn zero_henry_inductor_refuses_linearization() {
        // Bug-hunt #3, symmetric: a 0 H inductor would divide the `vl / *l`
        // rows by zero. Refuse so the MNA path (ideal-short stamp) handles it.
        let mut c = Circuit::new();
        let vin = c.node("in");
        let mid = c.node("mid");
        let out = c.node("out");
        c.add(Device::Vsource {
            name: "V1".into(),
            p: vin,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(1.0),
        });
        c.add(Device::Resistor {
            name: "R".into(),
            a: vin,
            b: mid,
            ohms: 50.0,
            tc1: None,
        });
        c.add(Device::Inductor {
            name: "L".into(),
            a: mid,
            b: out,
            henries: 0.0,
            ic: Some(0.0),
        });
        c.add(Device::Capacitor {
            name: "C".into(),
            a: out,
            b: NodeId::GROUND,
            farads: 1e-7,
            ic: Some(0.0),
        });
        let part = crate::partition::Partition::analyze(&c);
        assert!(
            LinearIsland::compile(&c, &part.islands[0], 0.0, 27.0).is_none(),
            "compile must refuse a zero-henry inductor island rather than divide by zero"
        );
    }

    #[test]
    fn rlc_state_space_has_two_states() {
        // V - R - L - C - gnd. Two states (C voltage, L current).
        let mut c = Circuit::new();
        let vin = c.node("in");
        let mid = c.node("mid");
        let out = c.node("out");
        c.add(Device::Vsource {
            name: "V1".into(),
            p: vin,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(1.0),
        });
        c.add(Device::Resistor {
            name: "R".into(),
            a: vin,
            b: mid,
            ohms: 50.0,
            tc1: None,
        });
        c.add(Device::Inductor {
            name: "L".into(),
            a: mid,
            b: out,
            henries: 1e-3,
            ic: Some(0.0),
        });
        c.add(Device::Capacitor {
            name: "C".into(),
            a: out,
            b: NodeId::GROUND,
            farads: 1e-7,
            ic: Some(0.0),
        });
        let part = crate::partition::Partition::analyze(&c);
        let li = LinearIsland::compile(&c, &part.islands[0], 0.0, 27.0).expect("compile");
        assert_eq!(li.n_states(), 2);
    }
}
