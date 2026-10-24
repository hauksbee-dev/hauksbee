//! The supply-envelope, logic-level and analog-input judges on SOLVED levels.
//!
//! One implementation serves both surfaces. A CI run hands it the span each
//! net covered over the run ([`SolvedLevels::run_assumptions`], evidence); the
//! static `check` hands it the levels its DC operating point settled on
//! ([`SolvedLevels::lint`], findings). Both end in the same three judges
//! ([`super::envelope`], [`super::logic_levels`], [`super::analog_inputs`]),
//! so a limit one surface applies the other applies identically.
//!
//! A net the solve covers is judged on its solved level. A net it does not
//! cover, because nothing in the netlist set its level or because the board
//! solved nowhere, is judged on the rail knowledge the board carries (a
//! regulator, a battery, a divider, the net's name), exactly as before a
//! solve existed: with no solved level at all, [`SolvedLevels::lint`] is the
//! old table-based result.

use std::collections::{BTreeMap, BTreeSet};

use hauksbee_extract::{ExtractedBoard, LintFinding};
use hauksbee_ir::evidence::Assumption;

use super::envelope::RailRange;
use super::BoardStatics;

/// The levels a solve gave, per net name.
#[derive(Debug, Clone, Default)]
pub struct SolvedLevels {
    /// The span each net covered: a run's min and max, or the low and high of
    /// the settled states a static pass solved.
    span: BTreeMap<String, RailRange>,
    /// The level a net settled at: a run's final level, or a static pass's
    /// first (rest) state.
    point: BTreeMap<String, f64>,
    /// The solver's `(reltol, vntol)`, so a level the solver delivers to its
    /// own tolerance is not a violation of a limit it sits on.
    tolerance: (f64, f64),
    /// Nets whose solved level is not trusted, so later states do not bring
    /// them back.
    distrusted: BTreeSet<String>,
}

/// A static solve's net within this of 0 V in every state is dark: unpowered,
/// switched off, or lifted by leakage through a gmin-grade path. That is not a
/// level any supply window or logic threshold is judged against, and no rail
/// is meant to sit there.
const DARK_V: f64 = 0.5;

/// A solved level within this share of the level its net's name states (or
/// [`ECHO_FLOOR_V`], whichever is more) is the name read back.
const ECHO_SHARE: f64 = 0.01;

/// The floor on that slack, in volts.
const ECHO_FLOOR_V: f64 = 0.05;

impl SolvedLevels {
    /// Levels from a solve: `span` per net, `point` per net.
    #[must_use]
    pub const fn new(
        span: BTreeMap<String, RailRange>,
        point: BTreeMap<String, f64>,
        tolerance: (f64, f64),
    ) -> Self {
        Self {
            span,
            point,
            tolerance,
            distrusted: BTreeSet::new(),
        }
    }

    /// An empty set of levels, judged to the solver's tolerance.
    #[must_use]
    pub const fn empty(tolerance: (f64, f64)) -> Self {
        Self::new(BTreeMap::new(), BTreeMap::new(), tolerance)
    }

    /// Record `net` at `volts` in one settled state. The first state seen is
    /// the net's point level; every state widens its span.
    pub fn observe(&mut self, net: &str, volts: f64) {
        if !volts.is_finite() || self.distrusted.contains(net) {
            return;
        }
        self.point.entry(net.to_string()).or_insert(volts);
        self.span
            .entry(net.to_string())
            .and_modify(|r| {
                r.min_v = r.min_v.min(volts);
                r.max_v = r.max_v.max(volts);
            })
            .or_insert_with(|| RailRange::point(volts));
    }

    /// Forget `net`: its level is not trusted (it fell below the level its
    /// name states, so the rail is dead or held off), and the judges fall back
    /// to what the board states of it.
    pub fn forget(&mut self, net: &str) {
        self.span.remove(net);
        self.point.remove(net);
        self.distrusted.insert(net.to_string());
    }

    /// The levels without every net that sat dark in every state observed: a
    /// net nothing powered is no measurement, and a supply window judged at
    /// ~0 V would call an unpowered part out of range.
    #[must_use]
    pub fn without_dark_nets(mut self) -> Self {
        let dark = |r: &RailRange| r.min_v.abs() <= DARK_V && r.max_v.abs() <= DARK_V;
        self.span.retain(|_, r| !dark(r));
        let span = &self.span;
        self.point.retain(|net, _| span.contains_key(net));
        self
    }

    /// The levels with every net dropped whose solve only echoes what its name
    /// states (`rails`): a rail the binder sources at the level its name
    /// implies solves at that level, or through a series conductor at its
    /// neighbour's, and that agreement is the name read back, not a finding of
    /// the solve. Such a net stays with the board's own knowledge of it (its
    /// regulator, or the name's tolerance band, which says the level is only
    /// the name's); a net the solve moves off its name (a droop, a divider
    /// tap, a regulator below its name) keeps its solved level.
    fn without_name_echoes(&self, rails: &dyn Fn(&str) -> Option<f64>) -> Self {
        let mut kept = self.clone();
        for (net, r) in &self.span {
            let Some(named) = rails(net) else { continue };
            let slack = (ECHO_SHARE * named.abs()).max(ECHO_FLOOR_V);
            if (r.min_v - named).abs() <= slack && (r.max_v - named).abs() <= slack {
                kept.span.remove(net);
                kept.point.remove(net);
            }
        }
        kept
    }

    /// Whether the solve covered no net at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.span.is_empty()
    }

    /// The span `net` covered, when the solve covered it.
    #[must_use]
    pub fn span_of(&self, net: &str) -> Option<RailRange> {
        self.span.get(net).copied()
    }

    /// The level `net` settled at, when the solve covered it.
    #[must_use]
    pub fn volts_of(&self, net: &str) -> Option<f64> {
        self.point.get(net).copied()
    }

    /// The three judges as run evidence, for the CI report: each limit the
    /// run measured violated undermines, each lesser one qualifies. `ambient`
    /// is the run's ambient (°C) and whether the scenario stated it.
    #[must_use]
    pub fn run_assumptions(
        &self,
        board: &ExtractedBoard,
        st: &BoardStatics<'_>,
        ambient: (f64, bool),
    ) -> Vec<Assumption> {
        // Comparator channels with every input above their common-mode
        // ceiling, ADC pins behind too high a source impedance.
        let volts = |net: &str| self.volts_of(net);
        let mut out = super::analog_inputs::run_assumptions(board, st, &volts, ambient);
        // A logic input whose span sat, or peaked, between its VCC-scaled VIL
        // and VIH: the datasheet guarantees no state there.
        let span = |net: &str| self.span_of(net);
        out.extend(super::logic_levels::run_assumptions(board, st, &span));
        // Supply and rail-order envelopes, on the span each net covered (a
        // sag that recovers is still a violation).
        out.extend(super::envelope::run_assumptions(
            board,
            st,
            &self.span,
            self.tolerance,
            ambient,
        ));
        out
    }

    /// The three judges as lint findings, for the static check. A net the
    /// solve covers is judged on its solved level; any other on `rails`, the
    /// level its name states (and, for the envelope, on the regulator,
    /// battery or divider that defines it).
    #[must_use]
    pub fn lint(
        &self,
        board: &ExtractedBoard,
        st: &BoardStatics<'_>,
        rails: &dyn Fn(&str) -> Option<f64>,
    ) -> SolvedFindings {
        let levels = self.without_name_echoes(rails);
        let envelope =
            super::envelope::envelope_lint_solved(board, st, &levels.span, levels.tolerance)
                .findings;
        let volts = |net: &str| levels.volts_of(net).or_else(|| rails(net));
        let analog = super::analog_inputs::analog_input_lint_in(board, st, &volts).findings;
        let span = |net: &str| {
            levels
                .span_of(net)
                .or_else(|| rails(net).map(RailRange::point))
        };
        let logic = super::logic_levels::logic_level_lint_in(board, st, &span).findings;
        SolvedFindings {
            envelope,
            analog,
            logic,
        }
    }
}

/// What [`SolvedLevels::lint`] found, by judge.
#[derive(Debug, Clone, Default)]
pub struct SolvedFindings {
    /// Supply and rail-order envelope findings.
    pub envelope: Vec<LintFinding>,
    /// Comparator common-mode and ADC source-impedance findings.
    pub analog: Vec<LintFinding>,
    /// Logic inputs between VIL and VIH.
    pub logic: Vec<LintFinding>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOL: (f64, f64) = (1e-3, 1e-6);

    #[test]
    fn first_state_is_the_point_and_every_state_widens_the_span() {
        let mut levels = SolvedLevels::empty(TOL);
        levels.observe("V_MID", 3.3);
        levels.observe("V_MID", 5.0);
        levels.observe("V_MID", 1.8);
        assert_eq!(levels.volts_of("V_MID"), Some(3.3));
        let span = levels.span_of("V_MID").expect("net was observed");
        assert!((span.min_v - 1.8).abs() < 1e-12 && (span.max_v - 5.0).abs() < 1e-12);
    }

    #[test]
    fn a_non_finite_level_is_no_observation() {
        let mut levels = SolvedLevels::empty(TOL);
        levels.observe("V_NAN", f64::NAN);
        levels.observe("V_INF", f64::INFINITY);
        assert!(levels.is_empty());
    }

    #[test]
    fn a_forgotten_net_stays_forgotten_in_later_states() {
        let mut levels = SolvedLevels::empty(TOL);
        levels.observe("V_DEAD", 0.4);
        levels.forget("V_DEAD");
        levels.observe("V_DEAD", 3.3);
        assert!(levels.span_of("V_DEAD").is_none());
        assert!(levels.volts_of("V_DEAD").is_none());
    }

    #[test]
    fn a_solve_that_only_reads_the_name_back_is_no_measurement() {
        let mut levels = SolvedLevels::empty(TOL);
        levels.observe("+3V3", 3.296);
        levels.observe("+5V", 4.2);
        levels.observe("MID", 1.65);
        let named = |net: &str| match net {
            "+3V3" => Some(3.3),
            "+5V" => Some(5.0),
            _ => None,
        };
        let kept = levels.without_name_echoes(&named);
        // Within a percent of its name: the name read back.
        assert!(kept.span_of("+3V3").is_none() && kept.volts_of("+3V3").is_none());
        // Off its name by a droop: the solve found something.
        assert_eq!(kept.volts_of("+5V"), Some(4.2));
        // No name level to agree with: the solve is all there is.
        assert_eq!(kept.volts_of("MID"), Some(1.65));
    }

    #[test]
    fn a_net_dark_in_every_state_is_dropped_but_one_lit_in_any_state_is_kept() {
        let mut levels = SolvedLevels::empty(TOL);
        // Leakage lifts a switched-off rail a few tens of millivolts, and a
        // gmin path can hold a net a little below ground: neither is a level.
        levels.observe("V_OFF", 0.075);
        levels.observe("V_OFF", -0.3);
        levels.observe("V_SWITCHED", 0.0);
        levels.observe("V_SWITCHED", 3.3);
        levels.observe("V_NEG", -3.3);
        let levels = levels.without_dark_nets();
        assert!(levels.span_of("V_OFF").is_none());
        assert!(levels.volts_of("V_OFF").is_none());
        assert!(levels.span_of("V_SWITCHED").is_some());
        assert!(levels.span_of("V_NEG").is_some());
    }
}
