//! Sweeping the variant and corner sets a board leaves open.
//!
//! A part the board does not pin to one member of its variant set binds a
//! flagged stand-in, so one run speaks only for that member. Every surface that
//! claims more sweeps the set instead: `hauksbee-ci` crosses each open set into
//! its seed plans, and the static `--check` suite re-judges the board at each
//! assumed corner of a class-only part (a generic MOSFET symbol with no part
//! number). Both find the open sets and pin a member the same way, here.
//!
//! The static sweep is bounded: with at most [`EXHAUSTIVE_RUNS`] corner
//! combinations it judges every one; past that it judges corner k of every set
//! together (all weak, all strong), [`CornerCoverage::Joint`], rather than
//! growing combinatorially with the number of class-only parts.

use hauksbee_extract::assembly::AssemblyState;
use hauksbee_extract::ExtractedBoard;
use hauksbee_models::variants::{CornerCoverage, SWEEP_PROPERTY};
use hauksbee_models::ModelLibrary;

use crate::bind_report::BindReport;

/// One fitted part's undetermined same-pinout set, in member order (the first
/// member is the stand-in a run that pins nothing binds).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenSet {
    pub reference: String,
    pub ids: Vec<String>,
    /// Every member is an assumed corner of a class, not an orderable variant.
    pub corners: bool,
}

/// Every fitted part on `board` whose same-pinout variant set the board leaves
/// undetermined, so a run binds a stand-in for it.
#[must_use]
pub fn open_sets(board: &ExtractedBoard, lib: &ModelLibrary) -> Vec<OpenSet> {
    let open = |comp: &hauksbee_extract::Component| {
        let part = AssemblyState::of(comp).fitted()?;
        let s = crate::binder::resolve(lib, part).selection?;
        let ids: Vec<String> = s.survivors().map(|m| m.id.clone()).collect();
        let open = s.determined.is_none() && s.same_pinout && ids.len() > 1;
        open.then(|| OpenSet {
            reference: comp.reference.clone(),
            ids,
            corners: s.corners,
        })
    };
    board.components.iter().filter_map(open).collect()
}

/// Pin each `(reference, member id)` on `board` for one sweep run.
pub fn pin(board: &mut ExtractedBoard, picks: &[(String, String)]) {
    for comp in &mut board.components {
        let ids = picks.iter().filter(|(r, _)| *r == comp.reference);
        comp.properties
            .extend(ids.map(|(_, id)| (SWEEP_PROPERTY.into(), id.clone())));
    }
}

/// The most corner combinations the static sweep judges exhaustively.
pub const EXHAUSTIVE_RUNS: usize = 4;

/// The static corner sweep for a board's class-only parts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CornerPlan {
    pub coverage: CornerCoverage,
    /// The corner sets swept.
    pub sets: Vec<OpenSet>,
    /// One `(reference, corner id)` pin list per run, beyond the unpinned run
    /// (every set at its stand-in), which the caller already made.
    pub runs: Vec<Vec<(String, String)>>,
}

impl CornerPlan {
    /// The plan for `board`'s corner sets; `None` when it has none.
    #[must_use]
    pub fn of(board: &ExtractedBoard, lib: &ModelLibrary) -> Option<Self> {
        Self::for_sets(
            open_sets(board, lib)
                .into_iter()
                .filter(|s| s.corners)
                .collect(),
        )
    }

    /// The plan for `sets`: every combination while there are at most
    /// [`EXHAUSTIVE_RUNS`], else corner k of every set together.
    #[must_use]
    pub fn for_sets(sets: Vec<OpenSet>) -> Option<Self> {
        if sets.is_empty() {
            return None;
        }
        let total = (sets.iter()).try_fold(1usize, |n, s| n.checked_mul(s.ids.len()));
        let exhaustive = total.is_some_and(|n| n <= EXHAUSTIVE_RUNS);
        let mut runs: Vec<Vec<(String, String)>> = if exhaustive {
            let mut runs = vec![Vec::new()];
            for s in &sets {
                let pick = |run: &Vec<(String, String)>, id: &String| {
                    let mut run = run.clone();
                    run.push((s.reference.clone(), id.clone()));
                    run
                };
                runs = (runs.iter())
                    .flat_map(|run| s.ids.iter().map(move |id| pick(run, id)))
                    .collect();
            }
            runs
        } else {
            let widest = sets.iter().map(|s| s.ids.len()).max().unwrap_or(0);
            (0..widest)
                .map(|k| {
                    let at = |s: &OpenSet| s.ids.get(k).or_else(|| s.ids.last()).cloned();
                    (sets.iter())
                        .filter_map(|s| Some((s.reference.clone(), at(s)?)))
                        .collect()
                })
                .collect()
        };
        // Every set at its first member is what the unpinned run binds.
        let stand_in = |run: &Vec<(String, String)>| {
            (run.iter().zip(&sets)).all(|((_, id), s)| s.ids.first() == Some(id))
        };
        runs.retain(|run| !stand_in(run));
        let coverage = if exhaustive {
            CornerCoverage::Exhaustive
        } else {
            CornerCoverage::Joint
        };
        Some(Self {
            coverage,
            sets,
            runs,
        })
    }

    /// Record on `report` that its corner sets were swept, so the evidence says
    /// the result holds across the corners rather than at one.
    pub fn mark(&self, report: &mut BindReport) {
        let swept = |reference: &str| self.sets.iter().any(|s| s.reference == reference);
        for (&index, s) in &mut report.selections {
            let row = report.rows.get(index).map(|r| r.reference.as_str());
            if s.corners && row.is_some_and(swept) {
                s.swept = Some(self.coverage);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(reference: &str) -> OpenSet {
        OpenSet {
            reference: reference.into(),
            ids: vec![
                format!("{reference}_high_vth"),
                format!("{reference}_low_vth"),
            ],
            corners: true,
        }
    }

    #[test]
    fn one_or_two_sets_are_crossed_exhaustively_without_the_stand_in_run() {
        let plan = CornerPlan::for_sets(vec![set("Q1")]).unwrap();
        assert_eq!(plan.coverage, CornerCoverage::Exhaustive);
        assert_eq!(plan.runs, vec![vec![("Q1".into(), "Q1_low_vth".into())]]);
        let plan = CornerPlan::for_sets(vec![set("Q1"), set("Q2")]).unwrap();
        assert_eq!(plan.coverage, CornerCoverage::Exhaustive);
        assert_eq!(
            plan.runs.len(),
            3,
            "four combinations less the stand-in run"
        );
    }

    #[test]
    fn many_sets_are_swept_jointly_not_combinatorially() {
        let sets: Vec<OpenSet> = (1..=12).map(|i| set(&format!("Q{i}"))).collect();
        let plan = CornerPlan::for_sets(sets).unwrap();
        assert_eq!(plan.coverage, CornerCoverage::Joint);
        // All weak is the unpinned run; all strong is the one extra.
        assert_eq!(plan.runs.len(), 1);
        assert!(plan.runs[0].iter().all(|(_, id)| id.ends_with("_low_vth")));
        assert_eq!(plan.runs[0].len(), 12);
    }

    #[test]
    fn no_corner_set_is_no_plan() {
        assert!(CornerPlan::for_sets(Vec::new()).is_none());
    }
}
