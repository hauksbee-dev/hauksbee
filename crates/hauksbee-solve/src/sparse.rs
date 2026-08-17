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
//!
//! Long-form how-and-why (motivation, theory, rejected alternatives, the
//! buried bodies): docs/how-and-why/hauksbee-solve/sparse.md

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

    /// The assembled `(col, value)` entries of one row. Used to evaluate the KCL
    /// residual `g·x - rhs` at a candidate operating point without a full solve.
    pub fn row(&self, i: usize) -> &[(usize, f64)] {
        &self.rows[i]
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
    /// When true, `refactor` falls back to the dynamic Markowitz re-pivot if the
    /// frozen ordering hits a singular pivot. Default false: `refactor` then
    /// keeps the original "frozen singular => return false" behaviour exactly, so
    /// every existing solve (and its bit-exactness / failure semantics) is
    /// untouched. The staged-DC driver flips it on for its diode-laden solves,
    /// where a stale frozen order would otherwise abort a factorization the
    /// matrix actually admits.
    allow_dynamic: bool,
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
            allow_dynamic: false,
        }
    }

    /// Enable/disable the dynamic-pivot fallback (off by default). The staged-DC
    /// driver turns it on only for the diode-laden solves that need it.
    pub fn set_allow_dynamic(&mut self, allow: bool) {
        self.allow_dynamic = allow;
    }

    /// Whether the dynamic-pivot fallback is currently enabled.
    pub fn allow_dynamic(&self) -> bool {
        self.allow_dynamic
    }

    /// Recompute the numeric factors of `m` (same pattern). Tries the fast
    /// frozen-ordering path first; if that order hits a singular/zero pivot
    /// (which a diode-reshaped board can provoke even when the matrix is
    /// non-singular), falls back to a full dynamic re-pivot that chooses both
    /// the pivot column and row by value+sparsity on the CURRENT numerics.
    ///
    /// The frozen path is byte-for-byte unchanged, so any circuit that
    /// factorizes on it stays bit-identical (the `Partitioning::Off`
    /// guarantee). The dynamic fallback fires ONLY after the frozen path
    /// returns false, and replaces the factors with an equivalent
    /// (`solve`-compatible) factorization of the same matrix.
    pub fn refactor(&mut self, m: &SparseMatrix) -> bool {
        if self.refactor_frozen(m) {
            return true;
        }
        if !self.allow_dynamic {
            // Default: preserve the original semantics exactly, a frozen-order
            // singular factorization reports failure and lets the caller decide.
            return false;
        }
        // The frozen elimination order found no viable pivot for some column.
        // Re-analyze the current numeric matrix from scratch with dynamic
        // (Markowitz threshold) pivoting and factor that.
        // `refactor_dynamic` writes the dynamic column order into `self.perm`
        // (so `solve` uses it). That ALSO PROMOTES it for the next call: the
        // matrix PATTERN is fixed across Newton iterations / transient steps, so
        // once diodes conduct and the original order fails every iteration, the
        // next `refactor` re-tries `refactor_frozen` with this improved order and
        // takes the fast path (its left-looking LU still does partial ROW
        // pivoting, adapting to the changing values). A later iterate the
        // promoted order can't factor simply re-triggers one more dynamic
        // re-analysis. This turns N dynamic factorizations into ~1 dynamic +
        // (N-1) fast frozen, removing the per-iteration re-analysis cost.
        let ok = self.refactor_dynamic(m);
        if !ok && std::env::var("HAUKSBEE_LU_DBG").is_ok() {
            eprintln!("[lu] frozen+dynamic both singular (n={})", self.n);
        }
        ok
    }

    /// The fast path: recompute the numeric factors of `m` (same pattern) with
    /// the FROZEN column ordering and threshold partial pivoting. Returns
    /// `false` if the frozen order leaves a column with no viable pivot.
    ///
    /// All scratch is reused across calls, so a transient step pays only for
    /// the arithmetic, not for reallocation. This body is intentionally
    /// unchanged from the original `refactor`: its floating-point operation
    /// order is the `Partitioning::Off` bit-exactness contract.
    pub fn refactor_frozen(&mut self, m: &SparseMatrix) -> bool {
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

    /// Dynamic-pivot fallback: factor the CURRENT numeric matrix `m` with a
    /// right-looking sparse LU that chooses each pivot by a Markowitz
    /// sparsity-vs-stability rule, picking BOTH the elimination column and the
    /// pivot row dynamically. This dissolves the failure mode of the frozen
    /// path: a column whose only remaining candidate rows have gone (near-)zero
    /// after conducting diodes reshaped the elimination. Because the order is
    /// recomputed for these exact values, a structurally non-singular matrix
    /// factors regardless of how stale the frozen order has become.
    ///
    /// On success it overwrites `perm` / `pivot_row` / `row_pos` / `diag` /
    /// `l` / `u` with a representation `solve` consumes identically to the
    /// frozen path, so callers need no special case. Returns `false` only if
    /// the matrix is genuinely (numerically) singular.
    ///
    /// This is O(work) sparse, but it rebuilds the ordering, so it is the slow
    /// path by design; it runs only when `refactor_frozen` fails.
    pub fn refactor_dynamic(&mut self, m: &SparseMatrix) -> bool {
        let n = self.n;
        if n == 0 {
            self.l.clear();
            self.u.clear();
            return true;
        }

        // Active sparse representation, by row and by column, of the remaining
        // (not-yet-eliminated) submatrix. We keep both so Markowitz counts and
        // the rank-1 update are both cheap. Values live in the row map; the
        // column map stores only membership (row indices) for counting/update.
        let mut row: Vec<Vec<(usize, f64)>> = vec![Vec::new(); n];
        let mut col: Vec<Vec<usize>> = vec![Vec::new(); n];
        for (i, r) in m.rows.iter().enumerate() {
            for &(j, v) in r {
                if v != 0.0 {
                    row[i].push((j, v));
                    col[j].push(i);
                }
            }
        }

        let mut row_elim = vec![false; n];
        let mut col_elim = vec![false; n];
        let mut row_cnt: Vec<usize> = row.iter().map(Vec::len).collect();
        let mut col_cnt: Vec<usize> = col.iter().map(Vec::len).collect();

        // Output factors, in elimination-step coordinates (same convention the
        // frozen path emits): perm[k] = column eliminated k-th, pivot_row[k] =
        // original row used at step k.
        self.l.clear();
        self.u.clear();
        let mut perm = vec![0usize; n];
        let mut pivot_row = vec![0usize; n];
        let mut diag = vec![0.0f64; n];
        // step at which an original row was used as pivot (usize::MAX if not yet).
        let mut row_step = vec![usize::MAX; n];

        // Dense scratch for assembling a pivot column / row contributions.
        let mut work = vec![0.0f64; n];
        let mut work_touched: Vec<usize> = Vec::new();

        // Deferred U entries: at step k, the pivot row's value in a still-live
        // column j becomes U[k, step_of_j]. We record (k, original_col_j, value)
        // and remap original_col_j -> its elimination step once every step is
        // known, then build U's CSC by column-step (the form `solve` consumes:
        // U(:,kk) holds entries on rows pivoted at steps < kk).
        let mut u_def: Vec<(usize, usize, f64)> = Vec::new();

        // Threshold partial pivoting: a candidate pivot must be at least
        // PIVOT_THRESH times the largest magnitude in its column to be accepted
        // for stability; among acceptable candidates pick the lowest Markowitz
        // count (product of remaining row and column nonzeros).
        const PIVOT_THRESH: f64 = 0.1;

        for k in 0..n {
            // Choose the pivot (row r, col c) over the remaining submatrix.
            // Scan columns in increasing nonzero count to keep the Markowitz
            // search cheap; break ties / accept by stability threshold.
            let mut best_r = usize::MAX;
            let mut best_c = usize::MAX;
            let mut best_mark = usize::MAX;
            let mut best_mag = 0.0f64;

            // Visit remaining columns ordered by current nonzero count.
            let mut cand_cols: Vec<usize> =
                (0..n).filter(|&c| !col_elim[c] && col_cnt[c] > 0).collect();
            cand_cols.sort_by_key(|&c| col_cnt[c]);

            // To bound work, once we have a candidate whose Markowitz count is
            // already minimal possible for the columns left to scan, stop.
            'cols: for &c in &cand_cols {
                if col_cnt[c] == 0 {
                    continue;
                }
                // Largest live magnitude in this column (for the threshold).
                let mut colmax = 0.0f64;
                for &i in &col[c] {
                    if !row_elim[i] {
                        // value of entry (i, c)
                        if let Some(v) = entry(&row[i], c) {
                            let a = v.abs();
                            if a > colmax {
                                colmax = a;
                            }
                        }
                    }
                }
                if colmax == 0.0 {
                    continue;
                }
                let thresh = PIVOT_THRESH * colmax;
                for &i in &col[c] {
                    if row_elim[i] {
                        continue;
                    }
                    let v = match entry(&row[i], c) {
                        Some(v) => v,
                        None => continue,
                    };
                    if v.abs() < thresh {
                        continue;
                    }
                    // Markowitz count: (row_cnt-1)*(col_cnt-1).
                    let mark =
                        (row_cnt[i].saturating_sub(1)).saturating_mul(col_cnt[c].saturating_sub(1));
                    if mark < best_mark || (mark == best_mark && v.abs() > best_mag) {
                        best_mark = mark;
                        best_mag = v.abs();
                        best_r = i;
                        best_c = c;
                    }
                }
                // A Markowitz count of zero (a singleton) can't be beaten;
                // and since columns are sorted ascending by count, once the
                // best possible mark for the NEXT column exceeds best_mark we
                // could stop, but keep it simple and correct: stop on a 0.
                if best_mark == 0 {
                    break 'cols;
                }
            }

            if best_r == usize::MAX {
                // No numerically usable pivot remains: the trailing submatrix is
                // (numerically) singular. On a DC operating-point solve this is
                // the genuinely-floating-node degeneracy, e.g. a stretch-cap /
                // diode-anode node connected to the rest only through a
                // reverse-biased junction (~0 S) and a DC-open capacitor, whose
                // DC voltage is physically undefined. Rather than fail the whole
                // factorization (which would discard the 4900+ well-defined
                // unknowns we already eliminated), ANCHOR each remaining live
                // (row==col) unknown with a unit pivot, pinning it to 0, exactly
                // what an infinitesimal gmin-to-ground does in the limit. This is
                // the standard Gmin/`option rshunt` resolution of a floating node
                // and keeps the factorization (and the solve) well-posed; those
                // nodes carry no signal current, so pinning them to 0 does not
                // perturb any physically-defined node within tolerance.
                // Anchor exactly ONE remaining live (row, column) pair at this
                // step with a unit pivot, pinning that unknown to ~0. The outer
                // `for k` loop re-enters here for each subsequent floating
                // unknown until the block is drained. The block is numerically
                // ~0, so pinning these decoupled unknowns to 0 is the standard
                // floating-node / gmin resolution and does not perturb any
                // physically-defined node. We pair a live row with a live column
                // (the trailing block is square, so counts match); the pivot is a
                // synthetic unit so `solve` yields x≈0 for the anchored unknown.
                let c = (0..n).find(|&c| !col_elim[c]).unwrap();
                let pr = (0..n).find(|&r| !row_elim[r]).unwrap();
                if std::env::var("HAUKSBEE_LU_DBG").is_ok() {
                    let remaining = (0..n).filter(|&c| !col_elim[c]).count();
                    eprintln!("[lu] dynamic: anchoring floating unknown (row {pr}, col {c}; {remaining} live) at step {k}/{n}");
                }
                perm[k] = c;
                pivot_row[k] = pr;
                diag[k] = 1.0;
                row_step[pr] = k;
                row_elim[pr] = true;
                col_elim[c] = true;
                // Fully DECOUPLE the anchored unknown: emit no L and no U for it.
                // `solve` then sets x[c] = b[pr] / 1.0; the anchored row is the
                // residual of a numerically-singular (floating) block whose rhs
                // is at the gmin/leak scale, so x[c] resolves to ~0; the
                // floating-node convention. Emitting the U couplings instead made
                // the anchored unknown chase later unknowns and could amplify to
                // NaN on a stiff transient, so we keep it isolated.
                self.l.col_ptr.push(self.l.row_idx.len());
                continue;
            }
            let (pr, pc) = (best_r, best_c);
            let pivot = entry(&row[pr], pc).unwrap();
            perm[k] = pc;
            pivot_row[k] = pr;
            diag[k] = pivot;
            row_step[pr] = k;
            row_elim[pr] = true;
            col_elim[pc] = true;

            // The pivot row over still-live columns is both (a) the "U row" that
            // updates the trailing submatrix and (b) the source of U[k, *]
            // entries (each live column j contributes to U at its future step).
            // Eliminated columns never remain in a row (they are dropped when
            // their step gathers), so `row[pr]` over `!col_elim` is exactly the
            // live pivot row.
            let pivrow: Vec<(usize, f64)> = row[pr]
                .iter()
                .copied()
                .filter(|&(j, _)| j != pc && !col_elim[j])
                .collect();
            for &(j, v) in &pivrow {
                // U[k, step_of_j]; remap j -> its step after the loop.
                u_def.push((k, j, v));
            }

            // For each remaining row i that has an entry in pivot column pc,
            // eliminate: factor = a(i,pc)/pivot; row_i -= factor * pivrow.
            // Record factor into L(:,k) (strictly lower, step coords).
            // `col[pc]` membership can carry stale/duplicate rows (a cancelled
            // fill-in leaves membership behind), so dedup and re-check the live
            // entry via `entry` below.
            let mut pcol_rows: Vec<usize> =
                col[pc].iter().copied().filter(|&i| !row_elim[i]).collect();
            pcol_rows.sort_unstable();
            pcol_rows.dedup();

            for &i in &pcol_rows {
                let aic = match entry(&row[i], pc) {
                    Some(v) => v,
                    None => continue,
                };
                if aic == 0.0 {
                    continue;
                }
                let factor = aic / pivot;
                // L entry (row i, step k); remap row i to its step later.
                self.l.row_idx.push(i);
                self.l.vals.push(factor);

                // row_i := row_i - factor * pivrow, over live columns.
                // Scatter row_i into dense work.
                work_touched.clear();
                for &(j, v) in &row[i] {
                    if !col_elim[j] || j == pc {
                        work[j] = v;
                        work_touched.push(j);
                    }
                }
                // remove the pivot-column entry (it becomes part of L, drops out)
                work[pc] = 0.0;
                for &(j, vj) in &pivrow {
                    let before = work[j];
                    if before == 0.0 {
                        // fill-in: register membership in col[j]
                        work_touched.push(j);
                        col[j].push(i);
                        col_cnt[j] += 1;
                    }
                    work[j] = before - factor * vj;
                }
                // Gather back into row[i], dropping the eliminated pivot column
                // and any numeric-zero results from cancellation.
                let mut newrow: Vec<(usize, f64)> = Vec::with_capacity(work_touched.len());
                for &j in &work_touched {
                    if j == pc {
                        continue;
                    }
                    let v = work[j];
                    work[j] = 0.0;
                    if v != 0.0 {
                        newrow.push((j, v));
                    }
                }
                newrow.sort_by_key(|&(j, _)| j);
                row[i] = newrow;
                row_cnt[i] = row[i].len();
            }

            // Mark the pivot column consumed.
            col_cnt[pc] = 0;

            // Finalize L(:,k) pointer.
            self.l.col_ptr.push(self.l.row_idx.len());
        }

        // Every column now has an elimination step: col_step[orig_col] = k.
        let mut col_step = vec![usize::MAX; n];
        for (k, &c) in perm.iter().enumerate() {
            col_step[c] = k;
        }

        // Remap L's stored original rows to their elimination steps.
        for ri in self.l.row_idx.iter_mut() {
            *ri = row_step[*ri];
        }

        // Build U's CSC by column-step from the deferred entries. Entry
        // (k, orig_col_j, v) means U[row=k, col=step_of_j=kk]; `solve` reads
        // U(:,kk) and subtracts U.vals * x[kk] from y[U.row_idx] where row_idx
        // is a step < kk. Group by kk = col_step[orig_col_j].
        let mut u_cols: Vec<Vec<(usize, f64)>> = vec![Vec::new(); n];
        for (k, cj, v) in u_def {
            let kk = col_step[cj];
            debug_assert!(kk > k, "U entry must be strictly upper");
            u_cols[kk].push((k, v));
        }
        self.u.clear();
        for col in u_cols {
            for (rowk, v) in col {
                self.u.row_idx.push(rowk);
                self.u.vals.push(v);
            }
            self.u.col_ptr.push(self.u.row_idx.len());
        }

        self.perm = perm;
        self.pivot_row = pivot_row;
        self.diag = diag;
        for k in 0..n {
            self.row_pos[self.pivot_row[k]] = k;
        }
        true
    }

    /// Solve `A x = b` in place, all triangular sweeps in step coordinates.
    ///
    /// `scratch` is a caller-owned work buffer of length >= `n` (the matrix
    /// dimension); it holds the permuted/eliminated right-hand side and is
    /// fully overwritten before any read, so its incoming contents are
    /// irrelevant. Keeping it caller-owned keeps a heap allocation off the
    /// Newton hot path, where an internal buffer would cost one `vec![0.0; n]`
    /// per solve, per iteration, per step. `&self` stays immutable so callers
    /// can hold the factorization while bringing their own scratch, which is
    /// also what lets each thread solve a private island matrix without
    /// contending on a shared buffer (see plan §4.1).
    pub fn solve(&self, b: &mut [f64], scratch: &mut [f64]) {
        let n = self.n;
        // Permute rhs by pivot rows: y[k] = b[pivot_row[k]].
        let y = &mut scratch[..n];
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

/// Look up the value at column `c` in a sorted `(col, value)` row, if present.
#[inline]
fn entry(rowvec: &[(usize, f64)], c: usize) -> Option<f64> {
    rowvec
        .binary_search_by_key(&c, |&(j, _)| j)
        .ok()
        .map(|i| rowvec[i].1)
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

    // Drain structurally isolated unknowns in one pass. The general minimum
    // scan below is intentionally simple and O(n^2), which is fine for a real
    // connected circuit but disastrous when a sparse external node id has
    // accidentally created a huge diagonal-only block: it would rescan every
    // ghost row once per elimination and appear to run forever. Isolated rows
    // cannot create fill, so emitting them first is the exact min-degree order
    // (degree zero is unbeatable) and preserves ascending tie-breaking.
    for v in 0..n {
        // Preserve the historical adjacency (including diagonal self-edges)
        // and therefore the exact healthy-matrix ordering. A vertex whose only
        // neighbour is itself is nevertheless isolated for elimination: its
        // gmin diagonal cannot create fill.
        let isolated = g[v].is_empty() || (g[v].len() == 1 && g[v][0] == v);
        if isolated {
            perm.push(v);
            eliminated[v] = true;
        }
    }

    for _ in perm.len()..n {
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
        let mut scratch = vec![0.0; x.len()];
        sym.solve(&mut x, &mut scratch);
        x
    }

    /// Solve forcing the DYNAMIC re-pivot path (no frozen attempt), to validate
    /// the fallback factorization in isolation.
    fn solve_dynamic(m: &SparseMatrix, b: &[f64]) -> Vec<f64> {
        let mut sym = m.factorize_symbolic();
        assert!(sym.refactor_dynamic(m), "dynamic refactor failed");
        let mut x = b.to_vec();
        let mut scratch = vec![0.0; x.len()];
        sym.solve(&mut x, &mut scratch);
        x
    }

    #[test]
    fn structurally_empty_row_is_anchored_instead_of_spinning() {
        let mut m = SparseMatrix::new(2);
        m.add(0, 0, 2.0);
        // Row and column 1 are structurally empty. The dynamic singular-block
        // convention anchors that genuinely floating unknown to zero.
        let x = solve_dynamic(&m, &[4.0, 0.0]);
        assert!((x[0] - 2.0).abs() < 1e-12, "defined row changed: {}", x[0]);
        assert_eq!(x[1], 0.0, "floating unknown was not anchored");
    }

    #[test]
    fn gmin_only_unknowns_are_drained_without_quadratic_symbolic_scan() {
        // This is the exact ghost-row shape that a high as-built NodeId used to
        // manufacture: every unknown has only its gmin diagonal. Large enough
        // that the former rescan-per-elimination path is prohibitive, while the
        // correct isolated drain is linear.
        const N: usize = 20_000;
        let mut m = SparseMatrix::new(N);
        for i in 0..N {
            m.add(i, i, 1e-12);
        }
        let mut symbolic = m.factorize_symbolic();
        assert_eq!(symbolic.perm.len(), N);
        assert!(symbolic.refactor(&m));
        let mut x = vec![0.0; N];
        let mut scratch = vec![0.0; N];
        symbolic.solve(&mut x, &mut scratch);
        assert!(x.iter().all(|v| *v == 0.0));
    }

    #[test]
    fn dynamic_matches_frozen_on_random_spd() {
        // A handful of well-conditioned systems: the dynamic path must produce
        // the same solution as the frozen path (to tight tolerance).
        let cases: &[(usize, &[(usize, usize, f64)], &[f64])] = &[
            (
                3,
                &[
                    (0, 0, 4.0),
                    (0, 1, 1.0),
                    (1, 0, 1.0),
                    (1, 1, 3.0),
                    (1, 2, 1.0),
                    (2, 1, 1.0),
                    (2, 2, 5.0),
                ],
                &[1.0, 2.0, 3.0],
            ),
            (
                4,
                &[
                    (0, 0, 10.0),
                    (0, 2, 2.0),
                    (1, 1, 7.0),
                    (1, 3, -1.0),
                    (2, 0, 2.0),
                    (2, 2, 9.0),
                    (3, 1, -1.0),
                    (3, 3, 6.0),
                ],
                &[5.0, -3.0, 4.0, 2.0],
            ),
        ];
        for (n, ent, b) in cases {
            let mut m = SparseMatrix::new(*n);
            for &(r, c, v) in *ent {
                m.add(r, c, v);
            }
            let xf = solve_with(&m, b);
            let xd = solve_dynamic(&m, b);
            for (a, c) in xf.iter().zip(xd.iter()) {
                assert!((a - c).abs() < 1e-9, "frozen {xf:?} vs dynamic {xd:?}");
            }
        }
    }

    #[test]
    fn dynamic_handles_zero_diagonal_mna() {
        // Same MNA-with-zero-diagonal block the frozen path covers; dynamic must
        // also pick the off-diagonal pivot.
        let mut m = SparseMatrix::new(2);
        let gload = 1.0 / 1000.0;
        m.add(0, 0, gload);
        m.add(0, 1, 1.0);
        m.add(1, 0, 1.0);
        let x = solve_dynamic(&m, &[0.0, 5.0]);
        assert!((x[0] - 5.0).abs() < 1e-9, "v = {}", x[0]);
        assert!((x[1] + gload * 5.0).abs() < 1e-9, "i = {}", x[1]);
    }

    #[test]
    fn dynamic_recovers_where_frozen_order_fails() {
        // Construct a matrix whose min-degree column order leaves a column with
        // a (numerically) zero pivot when factored in that fixed order, but
        // which is non-singular and solvable with dynamic column choice.
        // Arrow-like pattern: the frozen order can corner a near-zero pivot;
        // the dynamic Markowitz path reorders and succeeds. We assert the
        // dynamic path solves it and matches a dense reference.
        let n = 5;
        let mut m = SparseMatrix::new(n);
        // dense-ish coupled block with a tiny (1e-13) diagonal on node 2 that a
        // fixed order can hit as a pivot, but which has strong off-diagonals.
        let ent = [
            (0, 0, 3.0),
            (0, 2, 4.0),
            (1, 1, 2.0),
            (1, 2, 5.0),
            (2, 0, 4.0),
            (2, 1, 5.0),
            (2, 2, 1e-13),
            (2, 3, 6.0),
            (3, 2, 6.0),
            (3, 3, 2.0),
            (3, 4, 1.0),
            (4, 3, 1.0),
            (4, 4, 3.0),
        ];
        for &(r, c, v) in &ent {
            m.add(r, c, v);
        }
        let b = [1.0, 2.0, 3.0, 4.0, 5.0];
        // Dense reference (partial-pivot Gaussian elimination).
        let xref = dense_solve(n, &ent, &b);
        let xd = solve_dynamic(&m, &b);
        for (a, c) in xref.iter().zip(xd.iter()) {
            assert!((a - c).abs() < 1e-6, "ref {xref:?} vs dynamic {xd:?}");
        }
        // And the residual of the dynamic solution is near zero.
        let res = residual(n, &ent, &xd, &b);
        assert!(res < 1e-8, "dynamic residual {res}");
    }

    fn dense_solve(n: usize, ent: &[(usize, usize, f64)], b: &[f64]) -> Vec<f64> {
        let mut a = vec![0.0f64; n * n];
        for &(r, c, v) in ent {
            a[r * n + c] += v;
        }
        let mut bb = b.to_vec();
        let mut piv: Vec<usize> = (0..n).collect();
        for k in 0..n {
            let mut p = k;
            let mut best = a[piv[k] * n + k].abs();
            for i in (k + 1)..n {
                let mag = a[piv[i] * n + k].abs();
                if mag > best {
                    best = mag;
                    p = i;
                }
            }
            piv.swap(k, p);
            let rk = piv[k];
            let pivot = a[rk * n + k];
            for i in (k + 1)..n {
                let ri = piv[i];
                let f = a[ri * n + k] / pivot;
                for j in k..n {
                    let t = f * a[rk * n + j];
                    a[ri * n + j] -= t;
                }
                let t = f * bb[rk];
                bb[ri] -= t;
            }
        }
        let mut x = vec![0.0; n];
        for i in (0..n).rev() {
            let ri = piv[i];
            let mut s = bb[ri];
            for j in (i + 1)..n {
                s -= a[ri * n + j] * x[j];
            }
            x[i] = s / a[ri * n + i];
        }
        x
    }

    #[test]
    fn refactor_default_never_uses_dynamic_and_is_unchanged() {
        // The public `refactor` must, by default, keep the exact frozen-only
        // behaviour: on a matrix the frozen order can factor, the result is
        // identical to refactor_frozen; and `allow_dynamic` is off unless set.
        let mut m = SparseMatrix::new(3);
        m.add(0, 0, 4.0);
        m.add(0, 1, 1.0);
        m.add(1, 0, 1.0);
        m.add(1, 1, 3.0);
        m.add(1, 2, 1.0);
        m.add(2, 1, 1.0);
        m.add(2, 2, 5.0);
        let b = [1.0, 2.0, 3.0];

        let mut a = m.factorize_symbolic();
        assert!(!a.allow_dynamic, "dynamic must default OFF");
        assert!(a.refactor(&m));
        let mut xa = b.to_vec();
        let mut scratch_a = vec![0.0; xa.len()];
        a.solve(&mut xa, &mut scratch_a);

        let mut f = m.factorize_symbolic();
        assert!(f.refactor_frozen(&m));
        let mut xf = b.to_vec();
        let mut scratch_f = vec![0.0; xf.len()];
        f.solve(&mut xf, &mut scratch_f);

        // Byte-for-byte identical: refactor (default) == refactor_frozen.
        assert_eq!(
            xa.to_bits_vec(),
            xf.to_bits_vec(),
            "default refactor diverged from frozen"
        );
    }

    trait Bits {
        fn to_bits_vec(&self) -> Vec<u64>;
    }
    impl Bits for Vec<f64> {
        fn to_bits_vec(&self) -> Vec<u64> {
            self.iter().map(|v| v.to_bits()).collect()
        }
    }

    #[test]
    fn dynamic_solves_larger_stiff_diode_like_system() {
        // A larger system mixing strong couplings with several tiny (diode-OFF
        // ~1e-12) diagonals: the kind of stiffness that traps a fixed ordering.
        // The dynamic path must solve it to a tight residual versus a dense
        // partial-pivot reference.
        let n = 12;
        let mut ent: Vec<(usize, usize, f64)> = Vec::new();
        // a tridiagonal-ish backbone with some long-range coupling
        for i in 0..n {
            let diag = if i % 4 == 2 {
                1e-12
            } else {
                2.0 + (i as f64) * 0.1
            };
            ent.push((i, i, diag));
            if i > 0 {
                ent.push((i, i - 1, -0.7));
                ent.push((i - 1, i, -0.6));
            }
        }
        // some off-band entries to defeat a banded fixed order
        ent.push((0, n - 1, 0.5));
        ent.push((n - 1, 0, 0.4));
        ent.push((3, 8, 0.9));
        ent.push((8, 3, 0.8));
        let mut m = SparseMatrix::new(n);
        for &(r, c, v) in &ent {
            m.add(r, c, v);
        }
        let b: Vec<f64> = (0..n).map(|i| 1.0 + 0.3 * i as f64).collect();
        let xref = dense_solve(n, &ent, &b);
        let xd = solve_dynamic(&m, &b);
        for (a, c) in xref.iter().zip(xd.iter()) {
            assert!((a - c).abs() < 1e-6, "ref {xref:?} vs dynamic {xd:?}");
        }
        let res = residual(n, &ent, &xd, &b);
        assert!(res < 1e-8, "dynamic residual {res}");
    }

    fn residual(n: usize, ent: &[(usize, usize, f64)], x: &[f64], b: &[f64]) -> f64 {
        let mut r = b.to_vec();
        for &(rr, c, v) in ent {
            r[rr] -= v * x[c];
        }
        let _ = n;
        r.iter().map(|v| v * v).sum::<f64>().sqrt()
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
        let mut scratch = vec![0.0; x.len()];
        sym.solve(&mut x, &mut scratch);
        assert!((x[0] - 2.0).abs() < 1e-12);
        assert!((x[1] - 3.0).abs() < 1e-12);
    }
}
