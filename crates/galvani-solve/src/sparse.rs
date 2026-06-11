//! A small sparse LU with a reusable column ordering and partial pivoting.
//!
//! In a transient run the matrix *pattern* is fixed across steps; only the
//! values change. The expensive fill-reducing column ordering is therefore
//! computed once in [`SparseMatrix::factorize_symbolic`] and frozen, and every
//! step calls [`Symbolic::refactor`] / [`Symbolic::solve`]. That ordering reuse
//! is the main speed lever over re-analyzing the matrix each Newton iteration.
//!
//! The numeric factorization is Gilbert-Peierls left-looking sparse LU with
//! threshold partial pivoting (Gilbert & Peierls, 1988): each column's
//! nonzero structure is found by a depth-first reachability search over the
//! already-computed columns, then its values are computed and a pivot chosen.
//! Partial pivoting is essential for modified nodal analysis, whose voltage
//! source and inductor rows have structurally zero diagonals. The per-column
//! work is proportional to the arithmetic actually performed, so a tridiagonal
//! ladder factorizes in O(n), not O(n^3), while a voltage source's zero pivot
//! is handled by a row swap rather than failing.

/// A matrix being assembled in coordinate form, accumulating stamps.
#[derive(Debug, Clone, Default)]
pub struct SparseMatrix {
    n: usize,
    /// Rows of `(col, value)`, kept sorted by column.
    rows: Vec<Vec<(usize, f64)>>,
}

impl SparseMatrix {
    /// An `n x n` matrix of structural zeros.
    pub fn new(n: usize) -> Self {
        SparseMatrix {
            n,
            rows: vec![Vec::new(); n],
        }
    }

    /// Dimension.
    pub fn dim(&self) -> usize {
        self.n
    }

    /// Add `value` into entry `(row, col)`, creating the slot if new.
    pub fn add(&mut self, row: usize, col: usize, value: f64) {
        let r = &mut self.rows[row];
        match r.binary_search_by_key(&col, |&(c, _)| c) {
            Ok(i) => r[i].1 += value,
            Err(i) => r.insert(i, (col, value)),
        }
    }

    /// Ensure a structural slot exists at `(row, col)` even if its value is
    /// zero, so the frozen ordering accounts for every coordinate the assembler
    /// may later touch (e.g. a diode conductance that is momentarily zero).
    pub fn touch(&mut self, row: usize, col: usize) {
        let r = &mut self.rows[row];
        if let Err(i) = r.binary_search_by_key(&col, |&(c, _)| c) {
            r.insert(i, (col, 0.0));
        }
    }

    /// Resolve `(row, col)` to a stable `(row, position)` handle into the frozen
    /// pattern, or `None` if no slot exists. Used by the compiled stamp plan to
    /// write entries without a per-write binary search in the hot loop.
    ///
    /// Only valid after the pattern is fully reserved (see `reserve_pattern`);
    /// the returned position indexes into `rows[row]`.
    pub fn slot(&self, row: usize, col: usize) -> Option<(usize, usize)> {
        self.rows[row]
            .binary_search_by_key(&col, |&(c, _)| c)
            .ok()
            .map(|i| (row, i))
    }

    /// Add `value` into a pre-resolved `(row, position)` slot. Caller guarantees
    /// the slot came from [`Self::slot`] on the same (unchanged) pattern.
    #[inline]
    pub fn add_at(&mut self, slot: (usize, usize), value: f64) {
        self.rows[slot.0][slot.1].1 += value;
    }

    /// Reset all stored values to zero, keeping the pattern.
    pub fn clear_values(&mut self) {
        for r in &mut self.rows {
            for e in r.iter_mut() {
                e.1 = 0.0;
            }
        }
    }

    /// Column-compressed view `(col_ptr, row_idx, vals)` of the matrix.
    fn to_csc(&self) -> (Vec<usize>, Vec<usize>, Vec<f64>) {
        let n = self.n;
        let mut counts = vec![0usize; n];
        for row in &self.rows {
            for &(c, _) in row {
                counts[c] += 1;
            }
        }
        let mut col_ptr = vec![0usize; n + 1];
        for c in 0..n {
            col_ptr[c + 1] = col_ptr[c] + counts[c];
        }
        let mut row_idx = vec![0usize; col_ptr[n]];
        let mut vals = vec![0.0f64; col_ptr[n]];
        let mut next = col_ptr.clone();
        for (i, row) in self.rows.iter().enumerate() {
            for &(c, v) in row {
                let p = next[c];
                row_idx[p] = i;
                vals[p] = v;
                next[c] += 1;
            }
        }
        (col_ptr, row_idx, vals)
    }

    /// Compute the fill-reducing column ordering. Reuse the result every step.
    pub fn factorize_symbolic(&self) -> Symbolic {
        Symbolic::analyze(self)
    }
}

/// Reusable factorization state: the frozen column ordering plus numeric
/// factors recomputed by [`Self::refactor`].
#[derive(Debug, Clone)]
pub struct Symbolic {
    n: usize,
    /// Column elimination order: `perm[k]` is the original column factorized
    /// k-th. Computed once (min-degree) and frozen.
    perm: Vec<usize>,
    /// L factor (strictly lower, unit diagonal), CSC by elimination step.
    l: DynCsc,
    /// U factor (upper incl. diagonal), CSC by elimination step.
    u: DynCsc,
    /// Row pivot: `pivot_row[k]` = original row used as pivot at step k.
    pivot_row: Vec<usize>,
    /// `row_pos[orig_row]` = current elimination position of that row.
    row_pos: Vec<usize>,
    /// Diagonal pivot value at each elimination step.
    diag: Vec<f64>,
    /// Persistent scratch reused across refactors to avoid per-step allocation.
    scratch: Scratch,
    /// Cached CSC column pointers/row indices of the matrix pattern (fixed);
    /// only `csc_vals` is refreshed each refactor.
    csc_col_ptr: Vec<usize>,
    csc_row_idx: Vec<usize>,
    csc_vals: Vec<f64>,
    /// Scatter map: position in the assembled row vector -> CSC slot, built once.
    fill_map: Vec<usize>,
}

/// Reusable dense work arrays for the Gilbert-Peierls inner loop.
#[derive(Debug, Clone, Default)]
struct Scratch {
    x: Vec<f64>,
    marked: Vec<bool>,
    stack: Vec<usize>,
    reach: Vec<usize>,
    cursor: Vec<usize>,
    pivot_pos: Vec<usize>,
}

/// A grow-on-refactor CSC factor (structure can change with pivoting).
#[derive(Debug, Clone, Default)]
struct DynCsc {
    col_ptr: Vec<usize>,
    row_idx: Vec<usize>,
    vals: Vec<f64>,
}

impl DynCsc {
    fn clear(&mut self) {
        self.col_ptr.clear();
        self.col_ptr.push(0);
        self.row_idx.clear();
        self.vals.clear();
    }
}

impl Symbolic {
    fn analyze(m: &SparseMatrix) -> Symbolic {
        let n = m.n;
        // Symmetric adjacency of A + A^T for the ordering heuristic.
        let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
        let mut seen = vec![usize::MAX; n];
        for (i, row) in m.rows.iter().enumerate() {
            for &(j, _) in row {
                for &(a, b) in &[(i, j), (j, i)] {
                    if seen[b] != a {
                        seen[b] = a;
                        adj[a].push(b);
                    }
                }
            }
        }
        let perm = min_degree_order(&adj, n);

        // Build the fixed CSC structure once: column pointers and row indices
        // come straight from the matrix pattern; values are refreshed later.
        let (csc_col_ptr, csc_row_idx, _) = m.to_csc();
        let nnz = csc_row_idx.len();

        Symbolic {
            n,
            perm,
            l: DynCsc::default(),
            u: DynCsc::default(),
            pivot_row: vec![0; n],
            row_pos: vec![0; n],
            diag: vec![0.0; n],
            scratch: Scratch {
                x: vec![0.0; n],
                marked: vec![false; n],
                stack: Vec::with_capacity(n),
                reach: Vec::with_capacity(n),
                cursor: Vec::with_capacity(n),
                pivot_pos: vec![usize::MAX; n],
            },
            csc_col_ptr,
            csc_row_idx,
            csc_vals: vec![0.0; nnz],
            fill_map: Vec::new(),
        }
    }

    /// Recompute the numeric factors of `m` (same pattern) with partial
    /// pivoting. Returns `false` if a structurally empty column is hit.
    ///
    /// All scratch is reused across calls, so a transient step pays only for
    /// the arithmetic, not for reallocation.
    pub fn refactor(&mut self, m: &SparseMatrix) -> bool {
        let n = self.n;
        // Refresh the cached CSC values in place (structure is fixed).
        if self.fill_map.len() != n {
            self.fill_map = self.csc_col_ptr[..n].to_vec();
        } else {
            self.fill_map.copy_from_slice(&self.csc_col_ptr[..n]);
        }
        for v in self.csc_vals.iter_mut() {
            *v = 0.0;
        }
        for row in &m.rows {
            for &(c, val) in row {
                let p = self.fill_map[c];
                self.csc_vals[p] = val;
                self.fill_map[c] += 1;
            }
        }
        let col_ptr = &self.csc_col_ptr;
        let row_idx = &self.csc_row_idx;
        let vals = &self.csc_vals;

        self.l.clear();
        self.u.clear();

        // Reuse the persistent scratch (moved out to satisfy the borrow checker
        // while we also borrow self.l/self.u).
        let mut sc = std::mem::take(&mut self.scratch);
        let x = &mut sc.x;
        let marked = &mut sc.marked;
        let stack = &mut sc.stack;
        let reach = &mut sc.reach;
        let cursor = &mut sc.cursor;
        let pivot_pos = &mut sc.pivot_pos;
        for p in pivot_pos.iter_mut() {
            *p = usize::MAX;
        }

        for k in 0..n {
            let col = self.perm[k];
            // Scatter A(:,col) into x and seed the reachable set.
            reach.clear();
            for p in col_ptr[col]..col_ptr[col + 1] {
                let i = row_idx[p];
                x[i] = vals[p];
                if !marked[i] {
                    // depth-first reach over U structure of already-pivoted rows
                    dfs_reach(i, pivot_pos, &self.l, marked, stack, cursor, reach);
                }
            }

            // Solve L (lower part) against the scattered column: apply each
            // earlier pivot row present in the reach, in elimination order.
            // `reach` is in reverse topological order from the DFS, so iterate
            // it reversed to get ascending pivot order.
            for idx in (0..reach.len()).rev() {
                let i = reach[idx];
                if let Some(step) = pivot_index(i, pivot_pos) {
                    let xi = x[i];
                    if xi != 0.0 {
                        // x -= xi * L(:, step) below the pivot.
                        let (ls, le) = (self.l.col_ptr[step], self.l.col_ptr[step + 1]);
                        for p in ls..le {
                            x[self.l.row_idx[p]] -= self.l.vals[p] * xi;
                        }
                    }
                }
            }

            // Choose pivot: largest magnitude among not-yet-pivoted rows in x.
            let mut piv_row = usize::MAX;
            let mut piv_val = 0.0f64;
            for &i in reach.iter() {
                if pivot_pos[i] == usize::MAX && x[i].abs() > piv_val {
                    piv_val = x[i].abs();
                    piv_row = i;
                }
            }
            if piv_row == usize::MAX || piv_val == 0.0 {
                // structurally/numerically singular column
                for &i in reach.iter() {
                    x[i] = 0.0;
                    marked[i] = false;
                }
                self.scratch = sc;
                return false;
            }
            let pivot = x[piv_row];
            pivot_pos[piv_row] = k;
            self.pivot_row[k] = piv_row;
            self.diag[k] = pivot;

            // Emit, all in elimination-step coordinates:
            //   U(:,k) = entries on rows pivoted at steps < k (strictly upper),
            //   L(:,k) = remaining rows divided by the pivot (strictly lower).
            for &i in reach.iter() {
                let xi = x[i];
                if i != piv_row {
                    let step = pivot_pos[i];
                    if step != usize::MAX {
                        // already pivoted -> upper, lives at row `step` < k.
                        self.u.row_idx.push(step);
                        self.u.vals.push(xi);
                    } else if xi != 0.0 {
                        // not yet pivoted -> lower; record original row for now,
                        // remapped to its step after the whole factorization.
                        self.l.row_idx.push(i);
                        self.l.vals.push(xi / pivot);
                    }
                }
                x[i] = 0.0;
                marked[i] = false;
            }
            self.u.col_ptr.push(self.u.row_idx.len());
            self.l.col_ptr.push(self.l.row_idx.len());
        }

        // Remap L's stored original rows to elimination steps now that every
        // row has a pivot position.
        for ri in self.l.row_idx.iter_mut() {
            *ri = pivot_pos[*ri];
        }
        for k in 0..n {
            self.row_pos[self.pivot_row[k]] = k;
        }
        self.scratch = sc;
        true
    }

    /// Solve `A x = b` in place, all triangular sweeps in step coordinates.
    pub fn solve(&self, b: &mut [f64]) {
        let n = self.n;
        // Permute rhs by pivot rows: y[k] = b[pivot_row[k]].
        let mut y = vec![0.0; n];
        for k in 0..n {
            y[k] = b[self.pivot_row[k]];
        }
        // Forward solve L y = Pb. L(:,k) holds sub-diagonal rows (steps > k).
        for k in 0..n {
            let yk = y[k];
            let (ls, le) = (self.l.col_ptr[k], self.l.col_ptr[k + 1]);
            for p in ls..le {
                y[self.l.row_idx[p]] -= self.l.vals[p] * yk;
            }
        }
        // Back solve U x = y. U(:,k) holds strictly-upper rows (steps < k); the
        // diagonal pivot is kept separately in `diag`.
        for k in (0..n).rev() {
            let xk = y[k] / self.diag[k];
            y[k] = xk;
            let (us, ue) = (self.u.col_ptr[k], self.u.col_ptr[k + 1]);
            for p in us..ue {
                y[self.u.row_idx[p]] -= self.u.vals[p] * xk;
            }
        }
        // Unpermute: step k corresponds to column perm[k].
        for k in 0..n {
            b[self.perm[k]] = y[k];
        }
    }

    /// Number of stored nonzeros in L+U after the last refactor.
    pub fn nnz(&self) -> usize {
        self.l.vals.len() + self.u.vals.len()
    }
}

/// Map an original row to its elimination step if it has been pivoted.
#[inline]
fn pivot_index(orig_row: usize, pivot_pos: &[usize]) -> Option<usize> {
    let s = pivot_pos[orig_row];
    if s == usize::MAX {
        None
    } else {
        Some(s)
    }
}

/// Depth-first reachability over the L structure for Gilbert-Peierls. Pushes
/// every original row reachable from `start` (through already-pivoted columns)
/// onto `reach` in reverse topological order.
#[allow(clippy::too_many_arguments)]
fn dfs_reach(
    start: usize,
    pivot_pos: &[usize],
    l: &DynCsc,
    marked: &mut [bool],
    stack: &mut Vec<usize>,
    cursor: &mut Vec<usize>,
    reach: &mut Vec<usize>,
) {
    stack.clear();
    cursor.clear();
    stack.push(start);
    cursor.push(0);
    marked[start] = true;
    while let Some(&node) = stack.last() {
        let step = pivot_pos[node];
        let mut advanced = false;
        if step != usize::MAX {
            let (ls, le) = (l.col_ptr[step], l.col_ptr[step + 1]);
            let ci = *cursor.last().unwrap();
            let mut p = ls + ci;
            while p < le {
                let child = l.row_idx[p];
                if !marked[child] {
                    marked[child] = true;
                    *cursor.last_mut().unwrap() = (p - ls) + 1;
                    stack.push(child);
                    cursor.push(0);
                    advanced = true;
                    break;
                }
                p += 1;
            }
            if !advanced {
                stack.pop();
                cursor.pop();
                reach.push(node);
            }
        } else {
            stack.pop();
            cursor.pop();
            reach.push(node);
        }
    }
}

/// Minimum-degree ordering over a symmetric adjacency: a cheap Markowitz proxy
/// that keeps fill low. `perm[k]` = original column eliminated k-th.
fn min_degree_order(adj: &[Vec<usize>], n: usize) -> Vec<usize> {
    let mut deg: Vec<usize> = adj.iter().map(Vec::len).collect();
    let mut eliminated = vec![false; n];
    let mut g: Vec<Vec<usize>> = adj.to_vec();
    let mut perm = Vec::with_capacity(n);

    for _ in 0..n {
        let mut best = usize::MAX;
        let mut best_deg = usize::MAX;
        for v in 0..n {
            if !eliminated[v] && deg[v] < best_deg {
                best_deg = deg[v];
                best = v;
            }
        }
        if best == usize::MAX {
            break;
        }
        perm.push(best);
        eliminated[best] = true;

        let neigh: Vec<usize> = g[best]
            .iter()
            .copied()
            .filter(|&u| !eliminated[u])
            .collect();
        for (ai, &a) in neigh.iter().enumerate() {
            for &c in neigh.iter().skip(ai + 1) {
                if !g[a].contains(&c) {
                    g[a].push(c);
                    g[c].push(a);
                }
            }
        }
        for &u in &neigh {
            deg[u] = g[u].iter().filter(|&&w| !eliminated[w]).count();
        }
    }
    perm
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solve_with(m: &SparseMatrix, b: &[f64]) -> Vec<f64> {
        let mut sym = m.factorize_symbolic();
        assert!(sym.refactor(m), "refactor failed");
        let mut x = b.to_vec();
        sym.solve(&mut x);
        x
    }

    #[test]
    fn solves_2x2() {
        let mut m = SparseMatrix::new(2);
        m.add(0, 0, 2.0);
        m.add(0, 1, 1.0);
        m.add(1, 0, 1.0);
        m.add(1, 1, 3.0);
        let x = solve_with(&m, &[3.0, 5.0]);
        assert!((x[0] - 0.8).abs() < 1e-12, "{x:?}");
        assert!((x[1] - 1.4).abs() < 1e-12, "{x:?}");
    }

    #[test]
    fn solves_diagonal() {
        let mut m = SparseMatrix::new(3);
        m.add(0, 0, 4.0);
        m.add(1, 1, 2.0);
        m.add(2, 2, 8.0);
        let x = solve_with(&m, &[8.0, 6.0, 16.0]);
        assert!((x[0] - 2.0).abs() < 1e-12);
        assert!((x[1] - 3.0).abs() < 1e-12);
        assert!((x[2] - 2.0).abs() < 1e-12);
    }

    #[test]
    fn zero_diagonal_needs_pivoting() {
        // MNA-like: row 0 has no diagonal (a voltage-source branch row).
        let mut m = SparseMatrix::new(2);
        m.add(0, 1, 1.0); // v_node = ...
        m.add(1, 0, 1.0);
        m.add(1, 1, 1.0);
        let x = solve_with(&m, &[2.0, 3.0]);
        // x1 = 2; x0 + x1 = 3 -> x0 = 1.
        assert!((x[0] - 1.0).abs() < 1e-12, "{x:?}");
        assert!((x[1] - 2.0).abs() < 1e-12, "{x:?}");
    }

    #[test]
    fn solves_tridiagonal() {
        let n = 5;
        let mut m = SparseMatrix::new(n);
        for i in 0..n {
            m.add(i, i, 2.0);
            if i > 0 {
                m.add(i, i - 1, -1.0);
            }
            if i + 1 < n {
                m.add(i, i + 1, -1.0);
            }
        }
        let b: Vec<f64> = (0..n)
            .map(|i| 2.0 - (i > 0) as i32 as f64 - (i + 1 < n) as i32 as f64)
            .collect();
        let x = solve_with(&m, &b);
        for xi in x {
            assert!((xi - 1.0).abs() < 1e-9, "got {xi}");
        }
    }

    #[test]
    fn mna_voltage_source_block() {
        // 3 unknowns: node v (0), source branch i (1) tying v to 5 V, and a
        // load resistor from v to ground folded into the node diagonal.
        // Equations:
        //   row0 (KCL at v): g*v + i = 0
        //   row1 (branch):   v       = 5
        let mut m = SparseMatrix::new(2);
        let gload = 1.0 / 1000.0;
        m.add(0, 0, gload);
        m.add(0, 1, 1.0);
        m.add(1, 0, 1.0);
        let x = solve_with(&m, &[0.0, 5.0]);
        assert!((x[0] - 5.0).abs() < 1e-9, "v = {}", x[0]);
        assert!((x[1] + gload * 5.0).abs() < 1e-9, "i = {}", x[1]);
    }

    #[test]
    fn reuses_ordering_with_new_values() {
        let mut m = SparseMatrix::new(2);
        m.add(0, 0, 1.0);
        m.add(1, 1, 1.0);
        let mut sym = m.factorize_symbolic();

        m.clear_values();
        m.add(0, 0, 4.0);
        m.add(1, 1, 2.0);
        assert!(sym.refactor(&m));
        let mut x = vec![8.0, 6.0];
        sym.solve(&mut x);
        assert!((x[0] - 2.0).abs() < 1e-12);
        assert!((x[1] - 3.0).abs() < 1e-12);
    }
}
