//! The one place user-facing terms are explained.
//!
//! Every surface (the CLI, `hauksbee-ci`, report JSON, the HTML export and the
//! web app) prints a term's explanation from [`Term::entry`], so a reader sees
//! the same words wherever the term appears. Report JSON carries a `glossary`
//! limited to the terms that report uses ([`for_terms`]). The copy is plain
//! English for someone who has never read the docs: what it means, then what to
//! do about it.

use serde::{Deserialize, Serialize};

/// A term a report can show a reader.
///
/// The wire name (`snake_case`) is what report JSON and the web app key on.
/// Declaration order is display order; `Term::iter()` lists every term (for a
/// full glossary endpoint).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
    strum::EnumIter,
)]
#[serde(rename_all = "snake_case")]
pub enum Term {
    Pass,
    Fail,
    Invalid,
    /// A pass that holds only under the listed assumptions ("qualified").
    ConditionalPass,
    OpenPart,
    SubstituteModel,
    ReducedFidelity,
    AssumedCorner,
    NotExercised,
    NotChecked,
    InferredPinRole,
    DefaultParameter,
    FittedByDefault,
    OperatingLimit,
    ParserLimitation,
    Waived,
    DefaultRules,
    IdentityOnly,
    ExecutablePartial,
    ScopeUnspecified,
    ReportOnlyExit,
    McuHalted,
    UartActivity,
}

/// One glossary row: a term's display label, what it means, and what to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct TermEntry {
    /// The wire name, e.g. `invalid`.
    pub term: Term,
    /// How the term is displayed, e.g. `INVALID`.
    pub label: &'static str,
    /// What it means, in one or two plain sentences.
    pub short: &'static str,
    /// What the reader does about it.
    pub what_to_do: &'static str,
}

impl Term {
    /// The explanation every surface prints for this term.
    #[must_use]
    // One row per term: the copy reads best as a single table, not split up.
    #[allow(clippy::too_many_lines)]
    pub const fn entry(self) -> TermEntry {
        let (label, short, what_to_do) = match self {
            Self::Pass => (
                "PASS",
                "The check held, and nothing it depends on had to be guessed.",
                "Nothing. Keep the check so a later change that breaks it is caught.",
            ),
            Self::Fail => (
                "FAIL",
                "The board was simulated and the check did not hold. This is a finding about \
                 your design.",
                "Read the reason under the check: it names the net or part and what was seen. \
                 Fix the design (or the limit, if the limit is wrong) and rerun.",
            ),
            Self::Invalid => (
                "INVALID",
                "Hauksbee refused to give this a pass or fail, because something it rests on was \
                 not simulated (an open part, an unpowered regulator, firmware that never ran). \
                 It is not a failure of your board.",
                "Close the named gap and rerun.",
            ),
            Self::ConditionalPass => (
                "Conditional pass",
                "Passed, but only under the assumptions listed.",
                "Each assumption names what would make the pass unconditional.",
            ),
            Self::OpenPart => (
                "Open part",
                "A part with no model. It is left out of the circuit (treated as not there), so \
                 readings on its nets show the board without it.",
                "Give it a model (draft one from its datasheet), or mark it DNP if the board \
                 does not fit it.",
            ),
            Self::SubstituteModel => (
                "Stand-in model",
                "No model matched this exact part, so a similar one stood in. Where the two \
                 differ, results describe the stand-in.",
                "Add a model for the real part, or name the exact part fitted.",
            ),
            Self::ReducedFidelity => (
                "Reduced fidelity",
                "This part or result was simulated with a simpler stand-in. It is good enough \
                 for the claim shown, but not for finer ones.",
                "Usually nothing. To tighten it, give the run what the entry asks for.",
            ),
            Self::AssumedCorner => (
                "Assumed corner",
                "The board names only the kind of part (e.g. 'LED'), so Hauksbee checked a high \
                 and a low plausible part.",
                "Put the real part number on it to check that part instead.",
            ),
            Self::NotExercised => (
                "Not exercised",
                "The run never reached this (a regulator with no input supply, a bus never \
                 addressed, firmware that never ran), so there is no result for it.",
                "Supply what the entry names (a supply, firmware, a stimulus) and rerun.",
            ),
            Self::NotChecked => (
                "Not checked",
                "This check could not run on these inputs. No findings here does not mean there \
                 are none.",
                "Give the run the input the entry names and rerun.",
            ),
            Self::InferredPinRole => (
                "Inferred pin role",
                "Nothing said what this pin does, so Hauksbee went by the usual pin order for \
                 the package.",
                "If that is wrong for this part, name the pin roles in its model.",
            ),
            Self::DefaultParameter => (
                "Default value",
                "A value the inputs did not give was set to a documented default.",
                "Set the real value in the part's model or in the spec.",
            ),
            Self::FittedByDefault => (
                "Assumed fitted",
                "With no BOM, every placed part is treated as fitted, including any left off \
                 the real board.",
                "Supply a BOM that marks the parts that are not fitted.",
            ),
            Self::OperatingLimit => (
                "Outside part limits",
                "The run measured a part outside a limit its datasheet sets, so results that \
                 depend on it are not trusted.",
                "Bring the part inside the named limit (change the circuit or the part).",
            ),
            Self::ParserLimitation => (
                "Reader limitation",
                "The file reader has a known gap here, so findings in this area may be missing \
                 or wrong.",
                "Re-export from a supported tool version, or confirm these findings by hand.",
            ),
            Self::Waived => (
                "Waived",
                "Someone accepted this finding by hand, with a reason and an end date. It does \
                 not fail the run until that date.",
                "Fix the finding before the waiver ends, or renew it with a fresh reason.",
            ),
            Self::DefaultRules => (
                "Default rules",
                "The board's own spacing and width rules were not available, so checks used \
                 Hauksbee's defaults.",
                "Put the project's .kicad_pro next to the board so its own rules are used.",
            ),
            Self::IdentityOnly => (
                "Identity only",
                "Hauksbee knows the part and its pins, but not what it does, so it stays open in \
                 simulation.",
                "Give its model behaviour (draft it from the datasheet) to simulate it.",
            ),
            Self::ExecutablePartial => (
                "Partly modelled",
                "The part simulates, but only some of what it does is modelled.",
                "Results that need the rest are not evidence; extend the model to cover them.",
            ),
            Self::ScopeUnspecified => (
                "Scope not stated",
                "The part simulates, but its model does not say which behaviours it covers, so \
                 Hauksbee cannot tell which results it vouches for.",
                "List what the model covers in its card.",
            ),
            Self::ReportOnlyExit => (
                "Report only",
                "This command reports problems but still exits 0, so a script or CI job will \
                 not fail on them.",
                "Add --strict to make serious findings fail the build.",
            ),
            Self::McuHalted => (
                "Halted",
                "The emulated microcontroller is not executing firmware right now: the \
                 simulation is paused, the firmware has stopped (a fault, a sleep it never \
                 wakes from, or the end of main), or its supply is below the operating range.",
                "Press play; if it stays halted while the sim runs, check the MCU's supply rail \
                 and the serial output for a crash message.",
            ),
            Self::UartActivity => (
                "UART traffic",
                "The microcontroller sent or received bytes on its serial port (UART) in the last \
                 two seconds of real time; the console shows what it printed.",
                "Open the Serial tab to read the output or type input to the firmware.",
            ),
        };
        TermEntry {
            term: self,
            label,
            short,
            what_to_do,
        }
    }
}

/// The glossary rows for `terms`, each once, in declaration order: what a
/// report embeds so it explains exactly the terms it uses.
#[must_use]
pub fn for_terms(terms: impl IntoIterator<Item = Term>) -> Vec<TermEntry> {
    let mut terms: Vec<Term> = terms.into_iter().collect();
    terms.sort_unstable();
    terms.dedup();
    terms.into_iter().map(Term::entry).collect()
}

/// Every glossary row: what the web report embeds for its term popovers.
#[must_use]
pub fn all() -> Vec<TermEntry> {
    use strum::IntoEnumIterator;
    Term::iter().map(Term::entry).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use strum::IntoEnumIterator;

    #[test]
    fn every_term_is_listed_and_explained() {
        for term in Term::iter() {
            let e = term.entry();
            assert_eq!(e.term, term);
            for text in [e.label, e.short, e.what_to_do] {
                assert!(!text.trim().is_empty(), "{term:?}");
                assert!(!text.contains("  "), "{term:?}: {text:?}");
            }
            // The fix is stated, never deferred to a link.
            assert!(!e.what_to_do.contains("http"), "{term:?}");
        }
    }

    #[test]
    fn glossary_is_deduplicated_and_ordered() {
        let g = for_terms([Term::OpenPart, Term::Invalid, Term::OpenPart]);
        let terms: Vec<Term> = g.iter().map(|e| e.term).collect();
        assert_eq!(terms, [Term::Invalid, Term::OpenPart]);
        let json = serde_json::to_value(g[0]).unwrap();
        assert_eq!(json["term"], "invalid");
        assert_eq!(json["label"], "INVALID");
    }
}
