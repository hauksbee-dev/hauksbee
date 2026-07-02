//! The rail-balance outer loop: closing a balance tear's scalar KCL.
//!
//! A balance tear replaces one rail node's row of the global MNA system with
//! a scalar equation solved outside the blocks:
//!
//! ```text
//! f(v_rail) = (v_feed - v_rail) / R_shunt  -  sum_blocks I_drawn(v_rail)  =  0
//! ```
//!
//! Every block depends on the rest of the circuit only through `v_rail`
//! (bordered-block-diagonal, guaranteed by the conduction analysis that
//! proposed the tear), so driving `f` to zero makes the torn node voltages
//! equal the monolithic solution to Newton tolerance. Nothing is
//! approximated; the array is re-grouped around its one genuine coupling
//! scalar. That claim is enforced, not asserted: the `rail_tear` integration
//! suite gates torn-vs-monolithic agreement at 1e-6 on every commit.
//!
//! ## The gmin double-count correction (load-bearing, do not remove)
//!
//! Each block is solved as its own sub-circuit, and each sub-circuit stamps
//! its OWN gmin-to-ground shunt on its local copy of the rail node. The
//! monolithic reference stamps exactly ONE such shunt on the single shared
//! rail node. Summing the blocks' boundary currents therefore over-draws the
//! rail by `(n_loads - 1) * gmin * v_rail`, and without the correction the
//! torn solution is O(n_loads * gmin) below the monolithic one: small enough
//! to pass a sloppy eyeball, large enough to fail the 1e-6 gate, and
//! maddening to rediscover (it was found by bisecting the residual books
//! line by line against the monolith's row; see the how-and-why doc). The
//! surplus term is added back inside [`settle_rails`] so every executor gets
//! the correction whether or not its author has read this paragraph.
//!
//! ## Scope and honesty
//!
//! The loop *accepts* the state it reached when the pass budget runs out and
//! says so in the [`BalanceReport`]; it does not error, because the proven
//! partitioned engine's behaviour (proceed, let the step-level machinery
//! judge the result) is what the exactness gate certifies today. Callers
//! that can escalate (the strategy ladder's Decompose rung) must check
//! `report.converged` and refuse rather than stream a silently-unbalanced
//! result; the report exists precisely so non-convergence cannot be
//! invisible.
//!
//! Long-form how-and-why (motivation, theory, rejected alternatives, the
//! buried bodies): docs/how-and-why/hauksbee-solve/orchestrate.md

use hauksbee_ir::NodeId;

/// What the balance loop needs from the blocks loading the rails. The trait
/// reads live trial state: after [`RailLoads::resolve`] moves a rail, the
/// other accessors must reflect the re-solved blocks.
pub trait RailLoads {
    /// Set rail `i`'s trial voltage to `v_rail` and re-solve every block
    /// loading it (in place, no history advance). Blocks not loading rail
    /// `i` must not be re-solved (the loop's cost model depends on it).
    fn resolve(&mut self, i: usize, v_rail: f64) -> Result<(), String>;
    /// Rail `i`'s current trial voltage.
    fn rail_voltage(&self, i: usize) -> f64;
    /// The feed-node voltage seen by rail `i`'s shunt at the current trial
    /// state (0.0 for a grounded feed).
    fn feed_voltage(&self, i: usize) -> f64;
    /// Total current the blocks draw out of rail `i` at the current trial
    /// state (as summed from their boundary-source branch unknowns).
    fn current_drawn(&self, i: usize) -> f64;
    /// Number of distinct blocks loading rail `i`. Each stamps its own gmin
    /// shunt on its local rail copy; the loop uses this count to undo the
    /// double count (see the module doc).
    fn n_loads(&self, i: usize) -> usize;
}

/// Static description of one torn rail under balance.
#[derive(Debug, Clone, Copy)]
pub struct RailChannel {
    /// The torn rail node (for reporting; the loop itself is index-based).
    pub rail: NodeId,
    /// The series feed resistance the balance equation divides by.
    pub shunt_ohms: f64,
}

/// Tunables with their provenance. Defaults reproduce the proven partitioned
/// engine bit for bit; change them only with the `rail_tear` gate watching.
#[derive(Debug, Clone, Copy)]
pub struct BalancePolicy {
    /// Convergence target as a fraction of the node voltage tolerance. A
    /// residual current `r` leaves a rail-voltage error of at most
    /// `r * R_shunt`, so each rail's residual is driven below
    /// `v_target / R_shunt` with `v_target = vntol * v_target_frac`. The
    /// default 0.1 gives a 10x margin under the 1e-6 exactness gate,
    /// independent of the shunt value. (Empirically NOT the accuracy
    /// limiter: tightening it 10x moved the bypass-cap fixture's worst
    /// node error by under 1e-8; what bounds node agreement is the block
    /// Newtons' own vntol, in both the torn and monolithic formulations.)
    pub v_target_frac: f64,
    /// Cap on outer round-robin passes. Rails sharing a feed couple only at
    /// second order, so real boards converge in a handful of passes; 60 is
    /// a generous ceiling, not a tuning.
    pub max_outer: usize,
    /// Relative size of the finite-difference probe for df/dv_rail.
    pub probe_rel: f64,
    /// Absolute probe floor (volts), for rails sitting near zero.
    pub probe_floor: f64,
}

impl Default for BalancePolicy {
    fn default() -> Self {
        BalancePolicy {
            v_target_frac: 0.1,
            max_outer: 60,
            probe_rel: 1e-6,
            probe_floor: 1e-6,
        }
    }
}

/// What a settle pass did, for callers that must refuse on non-convergence
/// and for telemetry (outer-pass counts calibrate the cost model's
/// `outer_iters` estimate against reality).
#[derive(Debug, Clone, PartialEq)]
pub struct BalanceReport {
    /// Outer round-robin passes actually executed.
    pub outer_passes: usize,
    /// Each rail's KCL residual current (A) measured at the START of its
    /// final scalar-Newton update, i.e. the value the convergence test saw.
    pub final_residuals: Vec<f64>,
    /// True when every rail met its voltage-referred tolerance.
    pub converged: bool,
}

/// Drive every rail's KCL residual below its voltage-referred tolerance by
/// round-robin scalar Newton, re-solving only the blocks a moved rail feeds.
///
/// `gmin`, `vntol`, and `abstol` are the solver options the blocks were
/// stamped with; they parameterize the surplus correction and the two
/// convergence tests exactly as the partitioned engine's inlined loop did.
pub fn settle_rails<L: RailLoads>(
    loads: &mut L,
    channels: &[RailChannel],
    gmin: f64,
    vntol: f64,
    abstol: f64,
    policy: &BalancePolicy,
) -> Result<BalanceReport, String> {
    let v_target = (vntol * policy.v_target_frac).max(1e-12);
    let mut residuals = vec![0.0f64; channels.len()];
    let mut passes = 0usize;
    let mut converged = false;

    while passes < policy.max_outer {
        passes += 1;
        converged = true;
        for (i, ch) in channels.iter().enumerate() {
            let i_tol = (v_target / ch.shunt_ohms).max(1e-15);
            let resid = balance_one(loads, i, ch, gmin, abstol, policy)?;
            residuals[i] = resid;
            if resid.abs() > i_tol {
                converged = false;
            }
        }
        if converged {
            break;
        }
    }

    Ok(BalanceReport {
        outer_passes: passes,
        final_residuals: residuals,
        converged,
    })
}

/// One scalar-Newton update on rail `i`. Returns the residual current found
/// BEFORE the update, so the outer loop tests convergence on the actual
/// balance error rather than on the step size.
fn balance_one<L: RailLoads>(
    loads: &mut L,
    i: usize,
    ch: &RailChannel,
    gmin: f64,
    abstol: f64,
    policy: &BalancePolicy,
) -> Result<f64, String> {
    // The surplus gmin current the per-block shunts over-drew (module doc).
    let surplus = (loads.n_loads(i).saturating_sub(1)) as f64 * gmin;
    let residual = |l: &L| -> f64 {
        let v_rail = l.rail_voltage(i);
        let i_in = (l.feed_voltage(i) - v_rail) / ch.shunt_ohms;
        i_in - l.current_drawn(i) + surplus * v_rail
    };

    let v0 = loads.rail_voltage(i);
    let f0 = residual(loads);
    if f0.abs() <= abstol.max(1e-12) {
        return Ok(f0);
    }

    // Numeric df/dv_rail from a small perturbation: the shunt contributes
    // -1/R_shunt, the blocks their incremental rail conductance, which only
    // they can know (they are nonlinear; that is why they are blocks).
    let dv_probe = (v0.abs() * policy.probe_rel).max(policy.probe_floor);
    loads.resolve(i, v0 + dv_probe)?;
    let f1 = residual(loads);

    let slope = (f1 - f0) / dv_probe;
    let v_new = if slope.abs() > 1e-15 {
        v0 - f0 / slope
    } else {
        // A flat residual gives Newton nothing to chew; hold position and
        // let the outer loop's report say we did not converge.
        v0
    };
    loads.resolve(i, v_new)?;
    Ok(f0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An analytic stand-in for the block solves: each block on each rail is
    /// a linear conductance to ground, plus (to model what real sub-circuit
    /// blocks do) a per-block gmin shunt stamped on its local rail copy.
    /// The exact monolithic answer is closed-form, so the loop's output can
    /// be checked against truth rather than against itself.
    struct LinearLoads {
        /// Per rail: feed voltage.
        feeds: Vec<f64>,
        /// Per rail: block conductances to ground (one entry per block).
        blocks: Vec<Vec<f64>>,
        /// Per rail: current trial voltage.
        v: Vec<f64>,
        /// The gmin each block stamps on its local rail copy.
        gmin: f64,
        /// Count of resolve() calls, to verify only-touched-blocks costing.
        resolves: usize,
    }

    impl LinearLoads {
        /// The monolithic rail voltage: one shared node with ONE gmin shunt,
        /// fed through r_shunt, loaded by the blocks in parallel.
        fn monolithic_v(&self, i: usize, r_shunt: f64) -> f64 {
            let g_blocks: f64 = self.blocks[i].iter().sum();
            let g_total = 1.0 / r_shunt + g_blocks + self.gmin;
            (self.feeds[i] / r_shunt) / g_total
        }
    }

    impl RailLoads for LinearLoads {
        fn resolve(&mut self, i: usize, v_rail: f64) -> Result<(), String> {
            self.v[i] = v_rail;
            self.resolves += 1;
            Ok(())
        }
        fn rail_voltage(&self, i: usize) -> f64 {
            self.v[i]
        }
        fn feed_voltage(&self, i: usize) -> f64 {
            self.feeds[i]
        }
        fn current_drawn(&self, i: usize) -> f64 {
            // Every block draws its conductive current PLUS its own local
            // gmin draw, exactly like a sub-circuit with a stamped shunt.
            self.blocks[i]
                .iter()
                .map(|g| (g + self.gmin) * self.v[i])
                .sum()
        }
        fn n_loads(&self, i: usize) -> usize {
            self.blocks[i].len()
        }
    }

    const GMIN: f64 = 1e-9;
    const VNTOL: f64 = 1e-6;
    const ABSTOL: f64 = 1e-12;

    fn channels(n: usize, r: f64) -> Vec<RailChannel> {
        (0..n)
            .map(|k| RailChannel {
                rail: NodeId(k as u32 + 1),
                shunt_ohms: r,
            })
            .collect()
    }

    /// The loop must land on the monolithic answer, and for a LINEAR system
    /// the secant slope is exact, so one outer pass suffices.
    #[test]
    fn linear_system_settles_to_the_monolithic_answer_in_one_pass() {
        let r_shunt = 1e3;
        let mut loads = LinearLoads {
            feeds: vec![5.0],
            blocks: vec![vec![1e-4, 2e-4, 5e-4]],
            v: vec![5.0], // deliberately bad start: the unloaded feed value
            gmin: GMIN,
            resolves: 0,
        };
        let ch = channels(1, r_shunt);
        let rep =
            settle_rails(&mut loads, &ch, GMIN, VNTOL, ABSTOL, &BalancePolicy::default()).unwrap();
        assert!(rep.converged, "{rep:?}");
        let expect = loads.monolithic_v(0, r_shunt);
        let err = (loads.v[0] - expect).abs();
        assert!(err < 1e-9, "settled {} vs monolithic {expect}", loads.v[0]);
        // Pass 1 does the Newton update; pass 2 only measures the residual
        // and declares convergence.
        assert!(rep.outer_passes <= 2, "{rep:?}");
        // Exactly one probe + one commit: the converged re-measure must
        // early-return without re-solving anything.
        assert_eq!(loads.resolves, 2, "{rep:?}");
    }

    /// The regression this file exists to hold: WITHOUT the surplus
    /// correction, per-block gmin stamps drag the settled voltage below the
    /// monolithic answer by ~(n_loads-1)*gmin*R_shunt*v. With 40 blocks the
    /// uncorrected error is measurable at the gate tolerance; the corrected
    /// loop must not show it.
    #[test]
    fn gmin_double_count_is_corrected() {
        let r_shunt = 1e3;
        let n_blocks = 40;
        let mut loads = LinearLoads {
            feeds: vec![5.0],
            blocks: vec![vec![1e-4; n_blocks]],
            v: vec![0.0],
            gmin: GMIN,
            resolves: 0,
        };
        let ch = channels(1, r_shunt);
        let rep =
            settle_rails(&mut loads, &ch, GMIN, VNTOL, ABSTOL, &BalancePolicy::default()).unwrap();
        assert!(rep.converged);
        let expect = loads.monolithic_v(0, r_shunt);
        let err = (loads.v[0] - expect).abs();
        // The uncorrected bias for this fixture is (n-1)*gmin*v/g_total:
        // 39e-9 * 1V / 5e-3 S, about 8e-6 V, three decades above this bar.
        assert!(
            err < 1e-8,
            "gmin surplus leaked into the balance: err {err:.3e} vs expected {expect}"
        );
    }

    /// Multiple rails settle round-robin, each to its own answer.
    #[test]
    fn independent_rails_settle_together() {
        let r_shunt = 2.2e3;
        let mut loads = LinearLoads {
            feeds: vec![5.0, 3.3],
            blocks: vec![vec![1e-3, 1e-4], vec![4e-4]],
            v: vec![0.0, 0.0],
            gmin: GMIN,
            resolves: 0,
        };
        let ch = channels(2, r_shunt);
        let rep =
            settle_rails(&mut loads, &ch, GMIN, VNTOL, ABSTOL, &BalancePolicy::default()).unwrap();
        assert!(rep.converged, "{rep:?}");
        for i in 0..2 {
            let expect = loads.monolithic_v(i, r_shunt);
            assert!(
                (loads.v[i] - expect).abs() < 1e-9,
                "rail {i}: {} vs {expect}",
                loads.v[i]
            );
        }
    }

    /// A flat residual (blocks whose draw ignores the rail voltage entirely,
    /// slope exactly zero after the shunt term cancels... impossible
    /// physically, easy to mock) must exhaust the pass budget and REPORT
    /// non-convergence instead of erroring or spinning forever.
    struct FlatLoads;
    impl RailLoads for FlatLoads {
        fn resolve(&mut self, _i: usize, _v: f64) -> Result<(), String> {
            Ok(())
        }
        fn rail_voltage(&self, _i: usize) -> f64 {
            1.0
        }
        fn feed_voltage(&self, _i: usize) -> f64 {
            // Chosen so the residual is a constant 1 mA that Newton cannot
            // move: resolve() ignores the update entirely.
            2.0
        }
        fn current_drawn(&self, _i: usize) -> f64 {
            0.0
        }
        fn n_loads(&self, _i: usize) -> usize {
            1
        }
    }

    #[test]
    fn non_convergence_is_reported_not_hidden() {
        let ch = channels(1, 1e3);
        let policy = BalancePolicy {
            max_outer: 5,
            ..BalancePolicy::default()
        };
        let rep = settle_rails(&mut FlatLoads, &ch, GMIN, VNTOL, ABSTOL, &policy).unwrap();
        assert!(!rep.converged);
        assert_eq!(rep.outer_passes, 5);
        assert!(rep.final_residuals[0].abs() > 1e-6, "{rep:?}");
    }
}
