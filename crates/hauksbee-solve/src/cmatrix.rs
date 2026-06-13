//! A small dense complex linear system with LU + partial pivoting.
//!
//! AC / small-signal analysis solves `(G + jwC) x = b` once per frequency point.
//! Unlike the transient path the pattern is not reused thousands of times, the
//! systems are the same size as the (already small) MNA system, and the matrix
//! is genuinely complex. Rather than complexify the real Gilbert-Peierls sparse
//! LU (which would double its code and its pivoting subtleties), AC uses a
//! direct dense `Complex64` LU with partial pivoting: the textbook Doolittle
//! factorization that every numerical-methods reference describes, and the same
//! algorithm LAPACK's `zgesv` implements. Partial pivoting is mandatory because
//! MNA voltage-source / inductor rows have a structurally zero diagonal, exactly
//! as in the real solver.
//!
//! For Hauksbee's board sizes (tens to low hundreds of unknowns, one solve per
//! frequency) dense `O(n^3)` is comfortably fast and far easier to trust than a
//! bespoke complex sparse factorization. If a board ever makes this the
//! bottleneck the same `ComplexSystem` API can be backed by a sparse complex LU
//! without touching the AC driver.

use num_complex::Complex64;

/// A dense `n x n` complex system assembled by additive stamps, solved by
/// LU with partial pivoting.
#[derive(Debug, Clone)]
pub struct ComplexSystem {
    n: usize,
    /// Row-major `n*n` matrix.
    a: Vec<Complex64>,
    /// Right-hand side.
    b: Vec<Complex64>,
}

impl ComplexSystem {
    /// A fresh `n x n` system of zeros.
    pub fn new(n: usize) -> Self {
        ComplexSystem {
            n,
            a: vec![Complex64::new(0.0, 0.0); n * n],
            b: vec![Complex64::new(0.0, 0.0); n],
        }
    }

    /// Dimension.
    pub fn dim(&self) -> usize {
        self.n
    }

    /// Zero every entry, keeping the size (reused across a frequency sweep).
    pub fn clear(&mut self) {
        for v in self.a.iter_mut() {
            *v = Complex64::new(0.0, 0.0);
        }
        for v in self.b.iter_mut() {
            *v = Complex64::new(0.0, 0.0);
        }
    }

    /// Add `value` into matrix entry `(row, col)`.
    #[inline]
    pub fn add(&mut self, row: usize, col: usize, value: Complex64) {
        self.a[row * self.n + col] += value;
    }

    /// Add `value` into right-hand-side entry `row`.
    #[inline]
    pub fn add_rhs(&mut self, row: usize, value: Complex64) {
        self.b[row] += value;
    }

    /// Stamp a complex admittance `y` between two unknown indices (the standard
    /// nodal pattern: `+y` on the diagonals, `-y` off-diagonal). `None` is the
    /// ground reference and is skipped.
    pub fn stamp_admittance(&mut self, a: Option<usize>, b: Option<usize>, y: Complex64) {
        if let Some(a) = a {
            self.add(a, a, y);
        }
        if let Some(b) = b {
            self.add(b, b, y);
        }
        if let (Some(a), Some(b)) = (a, b) {
            self.add(a, b, -y);
            self.add(b, a, -y);
        }
    }

    /// Solve `A x = b` in place, returning the solution vector, or `None` if the
    /// matrix is singular. Uses LU with partial pivoting; the assembled matrix is
    /// consumed (factorized in place).
    pub fn solve(mut self) -> Option<Vec<Complex64>> {
        let n = self.n;
        if n == 0 {
            return Some(Vec::new());
        }
        let a = &mut self.a;
        let b = &mut self.b;
        let mut piv: Vec<usize> = (0..n).collect();

        for k in 0..n {
            // Partial pivot: largest magnitude in column k at or below the diagonal.
            let mut p = k;
            let mut best = a[piv[k] * n + k].norm();
            for (i, &r) in piv.iter().enumerate().take(n).skip(k + 1) {
                let mag = a[r * n + k].norm();
                if mag > best {
                    best = mag;
                    p = i;
                }
            }
            if best == 0.0 {
                return None; // singular column
            }
            piv.swap(k, p);

            let rk = piv[k];
            let pivot = a[rk * n + k];
            // Eliminate below.
            for i in (k + 1)..n {
                let ri = piv[i];
                let factor = a[ri * n + k] / pivot;
                a[ri * n + k] = factor;
                for j in (k + 1)..n {
                    let term = factor * a[rk * n + j];
                    a[ri * n + j] -= term;
                }
                let term = factor * b[rk];
                b[ri] -= term;
            }
        }

        // Back-substitution in pivoted row order.
        let mut x = vec![Complex64::new(0.0, 0.0); n];
        for i in (0..n).rev() {
            let ri = piv[i];
            let mut sum = b[ri];
            for j in (i + 1)..n {
                sum -= a[ri * n + j] * x[j];
            }
            let diag = a[ri * n + i];
            if diag.norm() == 0.0 {
                return None;
            }
            x[i] = sum / diag;
        }
        Some(x)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(re: f64, im: f64) -> Complex64 {
        Complex64::new(re, im)
    }

    #[test]
    fn solves_real_2x2() {
        // [2 1; 1 3] x = [3; 5] -> x = [0.8; 1.4]
        let mut s = ComplexSystem::new(2);
        s.add(0, 0, c(2.0, 0.0));
        s.add(0, 1, c(1.0, 0.0));
        s.add(1, 0, c(1.0, 0.0));
        s.add(1, 1, c(3.0, 0.0));
        s.add_rhs(0, c(3.0, 0.0));
        s.add_rhs(1, c(5.0, 0.0));
        let x = s.solve().unwrap();
        assert!((x[0].re - 0.8).abs() < 1e-12 && x[0].im.abs() < 1e-12, "{x:?}");
        assert!((x[1].re - 1.4).abs() < 1e-12 && x[1].im.abs() < 1e-12, "{x:?}");
    }

    #[test]
    fn solves_complex_1x1() {
        // (1 + j) x = (2 + 0j) -> x = (2)/(1+j) = 1 - j
        let mut s = ComplexSystem::new(1);
        s.add(0, 0, c(1.0, 1.0));
        s.add_rhs(0, c(2.0, 0.0));
        let x = s.solve().unwrap();
        assert!((x[0].re - 1.0).abs() < 1e-12, "{x:?}");
        assert!((x[0].im + 1.0).abs() < 1e-12, "{x:?}");
    }

    #[test]
    fn needs_pivoting_zero_diagonal() {
        // [0 1; 1 1] x = [2; 3] -> x = [1; 2]
        let mut s = ComplexSystem::new(2);
        s.add(0, 1, c(1.0, 0.0));
        s.add(1, 0, c(1.0, 0.0));
        s.add(1, 1, c(1.0, 0.0));
        s.add_rhs(0, c(2.0, 0.0));
        s.add_rhs(1, c(3.0, 0.0));
        let x = s.solve().unwrap();
        assert!((x[0].re - 1.0).abs() < 1e-12, "{x:?}");
        assert!((x[1].re - 2.0).abs() < 1e-12, "{x:?}");
    }

    #[test]
    fn detects_singular() {
        let mut s = ComplexSystem::new(2);
        s.add(0, 0, c(1.0, 0.0));
        s.add(0, 1, c(2.0, 0.0));
        s.add(1, 0, c(2.0, 0.0));
        s.add(1, 1, c(4.0, 0.0));
        s.add_rhs(0, c(1.0, 0.0));
        s.add_rhs(1, c(2.0, 0.0));
        assert!(s.solve().is_none());
    }
}
