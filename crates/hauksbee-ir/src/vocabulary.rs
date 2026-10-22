//! The one vocabulary every surface reads: verdict states, evidence grades,
//! the severity scale, the exit-code table and the check names, beside the
//! existing [`crate::glossary`].
//!
//! The CLI, `hauksbee-ci`, the app, `JUnit`, SARIF, the KiCad report and the
//! documents all render a run from the same [`crate::run_record::RunRecord`],
//! and they name what they show with the words defined here. A renderer that
//! wants a label, a one-line meaning, a CI alias, an exit code, a SARIF level
//! or a KiCad severity asks this module; it never spells its own.
//!
//! Every entry has an `id` (the wire word), a display `label`, a one-line
//! `meaning` and its `aliases` (older or tool-specific spellings, so a
//! renderer being migrated, or a reader of an old report, can map them).
//! [`vocabulary`] is the whole set as data with one serialization, committed
//! as `crates/hauksbee-ir/schemas/hauksbee-vocabulary.json` (drift-tested):
//! what `explain verdicts|severities|exit-codes` prints and what the app reads.

use serde::{Deserialize, Serialize};

use crate::evidence::EvidenceStatus;
use crate::glossary::TermEntry;

/// The exit-code contract the codes in [`ExitClass`] belong to. A record
/// states it next to its exit code so a consumer can tell which table applies.
pub const EXIT_CONTRACT: &str = "hauksbee/1";

/// How every human surface opens a partly judged verdict.
///
/// The reason sentence, the triage headline, the CI finding and the closing
/// `VERDICT:` line all start from it, so the words are spelled once.
pub const PARTLY_JUDGED_LEAD: &str = "PARTLY JUDGED (not a pass)";

/// How every human surface opens an inconclusive verdict: the same word the
/// closing `VERDICT: inconclusive` line uses, in capitals as
/// [`PARTLY_JUDGED_LEAD`] is.
pub const INCONCLUSIVE_LEAD: &str = "INCONCLUSIVE (not a pass)";

/// How every human surface opens the verdict on an input that is not a board.
///
/// A schematic cannot be ordered: the checks that apply to it ran, and nothing
/// about ordering was judged. The triage headline and the closing `VERDICT:`
/// line both start from it, in place of "OK TO ORDER" and "clean".
pub const NOT_ORDERABLE_LEAD: &str = "CHECKED, NOT ORDERABLE";

/// The closing `VERDICT:` line's tag for the same case, lower case as its
/// siblings ("clean", "failing") are.
pub const NOT_ORDERABLE_TAG: &str = "checked, not orderable";

/// What a surface calls the line that says which key parts a run did not judge.
///
/// A failing run says so once, on one line after its failures (the triage
/// block, the sections of the text report); the web card and the app list the
/// same facts under their own heading.
pub const NOT_JUDGED_LEAD: &str = "NOT JUDGED";

/// The step that judges what a run left unjudged for want of a model. The
/// scope sentence and the `NOT JUDGED` line both end with it.
pub const DRAFT_MODELS_NEXT_STEP: &str =
    "Draft the model(s) with `hauksbee models extract --unmodelled <board>` to judge the rest.";

/// [`DRAFT_MODELS_NEXT_STEP`] for the web app, which offers the step as a
/// button and so names no terminal command.
pub const DRAFT_MODELS_NEXT_STEP_WEB: &str = "Add a model for each open part to judge the rest.";

/// How a reader's reason opens when its input cannot hold a part's value.
///
/// Altium `.PcbDoc`, EasyEDA Pro `.epcb`, IPC-D-356, and ODB++ or IPC-2581
/// without a value property all use it. Lower case, matched against a
/// lower-cased reason: it tells "the input has no place for the value" from "the designer
/// left a placeholder", which only the first is a reduced scope and not a defect.
pub const VALUE_NOT_CARRIED_REASON_LEAD: &str = "no value in";

/// The step that supplies the values an input does not carry. The scope
/// sentence, the `NOT COVERED` line and the open-part remedy all end with it.
pub const VALUES_NOT_CARRIED_NEXT_STEP: &str =
    "Pass the BOM beside the board (`--bom <bom.csv>`) so the values are read, not guessed.";

/// [`VALUES_NOT_CARRIED_NEXT_STEP`] for the web app, where the BOM is uploaded
/// with the board and no flag is typed.
pub const VALUES_NOT_CARRIED_NEXT_STEP_WEB: &str =
    "Upload the BOM with the board so the values are read, not guessed.";

/// A terminal-worded sentence as the web app says it: each next step that
/// names a command swapped for its web wording, everything else unchanged.
///
/// The web text must hold no terminal command (the page has buttons and
/// uploads for these steps); the CLI keeps the originals. It is the one
/// place the pairs are listed.
#[must_use]
pub fn without_terminal_commands(text: &str) -> String {
    text.replace(DRAFT_MODELS_NEXT_STEP, DRAFT_MODELS_NEXT_STEP_WEB)
        .replace(
            VALUES_NOT_CARRIED_NEXT_STEP,
            VALUES_NOT_CARRIED_NEXT_STEP_WEB,
        )
}

/// The scope sentence for parts whose values the input does not carry.
///
/// `n` is how many and `subjects` names a few. They are not judged on anything
/// that depends on a value, and no defect is claimed: the values are unknown,
/// not unset.
#[must_use]
pub fn values_not_carried_scope(n: usize, subjects: &str) -> String {
    let lead = if n == 1 {
        format!("1 part carries no value in this input ({subjects}), so its value is unknown")
    } else {
        format!("{n} parts carry no value in this input ({subjects}), so their values are unknown")
    };
    format!(
        "{lead} and the checks and simulation that depend on a value are not judged for them. \
         {VALUES_NOT_CARRIED_NEXT_STEP}"
    )
}

/// The command that runs a check spec with no time limit. The browser stops a
/// run at a ceiling scaled to the board; the terminal never does.
pub const CHECK_RUN_CLI_COMMAND: &str = "hauksbee-ci run <spec.toml>";

/// The step after a check run in the browser was stopped at its ceiling.
pub const CHECK_RUN_STOPPED_NEXT_STEP: &str =
    "Download this spec from the page and run it in a terminal, which has no time limit: \
     `hauksbee-ci run <spec.toml>`.";

/// Why and where a browser check run was stopped at its ceiling.
///
/// `doing` is the stage's words as the progress bar showed them ("Simulating"),
/// `percent` how far the estimated work had got, `elapsed_s` how long it had
/// run. `None` for the stage when the run had not reported one.
#[must_use]
pub fn check_run_stopped(doing: Option<(&str, u32)>, elapsed_s: u64) -> String {
    let how_long = if elapsed_s >= 120 {
        format!("{} minutes", elapsed_s / 60)
    } else {
        format!("{elapsed_s} seconds")
    };
    let where_it_was = doing.map_or_else(String::new, |(label, percent)| {
        format!(" while \"{label}\" ({percent}% of the estimated work done)")
    });
    format!(
        "The check run was stopped after {how_long}{where_it_was} because it passed the \
         time the browser gives a board this size. {CHECK_RUN_STOPPED_NEXT_STEP}"
    )
}

/// What every surface says about a track or via of one net that lies inside
/// another net's stored zone fill.
///
/// The copper is not shorted: the fill was computed before the track was
/// routed or moved, and a refill carves the zone's clearance around it. `KiCad`
/// itself reports the same place as a clearance violation, never a short.
/// `copper` is the kind of copper ("track", "via" or "pad"); `B` is `KiCad`'s key for
/// "Fill all zones".
#[must_use]
pub fn stale_zone_fill(zone_net: &str, copper_net: &str, copper: &str) -> String {
    format!(
        "zone fill is out of date (a {copper_net} {copper} lies inside the stored {zone_net} fill); refill zones (B) in KiCad and re-run"
    )
}

/// How many part or net names a one-line message names before it counts the rest.
pub const NAMES_SHOWN: usize = 6;

/// The first few part or net names, then a count of the rest ("A, B and 12 more").
///
/// Every surface that puts a list of parts or nets into a sentence or a label
/// goes through it, so a board with thousands of nets never prints them in one
/// line. The full list stays wherever the structured record carries it (the
/// `--json` finding's `nets`/`refs`, a CI row's `all_subjects`).
#[must_use]
pub fn names_summary<S: AsRef<str>>(names: &[S]) -> String {
    list_summary(names, ", ")
}

/// [`names_summary`] for items a sentence separates with something other than
/// a comma (a "; "-joined list of per-part readings, say).
#[must_use]
pub fn list_summary<S: AsRef<str>>(items: &[S], separator: &str) -> String {
    let join = |items: &[S]| {
        items
            .iter()
            .map(AsRef::as_ref)
            .collect::<Vec<&str>>()
            .join(separator)
    };
    match items.split_at_checked(NAMES_SHOWN) {
        Some((head, rest)) if !rest.is_empty() => {
            format!("{} and {} more", join(head), rest.len())
        }
        _ => join(items),
    }
}

/// A sentence with its first letter in capitals, the rest untouched.
///
/// Conditions a pass rests on are listed one under another and joined into the
/// headline, so each one starts like a sentence. Their sources say so; this is
/// the guard at the one list every surface reads, for a source that forgets.
#[must_use]
pub fn sentence_case(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) if first.is_lowercase() => first.to_uppercase().chain(chars).collect(),
        _ => text.to_string(),
    }
}

/// What joins a failing verdict's own sentence to the sentence naming what it
/// did not judge, in `verdict_reason`, so a surface that prints the two apart
/// can split them where the engine joined them.
pub const NOT_JUDGED_JOINER: &str = " Also: ";

/// The assertion kind a CI run reports a static-check finding under.
///
/// A finding of the board's own static check (`hauksbee check`) is reported
/// under it, so a run never reads green over a board whose static verdict
/// fails. The JSON `kind`, the `JUnit` classname and the human row all carry
/// it.
pub const STATIC_CHECK_KIND: &str = "static_check";

/// How a CI run states a gating static-check finding: the run cannot pass
/// while `hauksbee check` fails the same board on it.
pub const STATIC_CHECK_FAILS_LEAD: &str =
    "FAIL: the board's own static check (hauksbee check) fails on this finding, so this run \
     cannot pass";

/// How a CI run states a gating static-check finding it cannot judge as built.
///
/// The finding is on a part or net that an as-built overlay entry reworks but
/// the static board could not carry, so it is the design's: not judged on the
/// reworked board, and the run cannot pass on it either.
pub const STATIC_CHECK_NOT_JUDGED_LEAD: &str =
    "NOT JUDGED on the as-built board: the board's own static check (hauksbee check) fails the \
     design on this finding, and the rework of these parts could not be applied to the static \
     board";

/// The order-triage bucket for findings a person must look at before ordering.
///
/// A run whose only items are in this bucket is a pass (nothing blocks), but
/// it is not clean: the closing `VERDICT:` line carries the same words, in
/// lower case, instead of "clean", so the two never disagree.
pub const INSPECT_BEFORE_ORDERING: &str = "INSPECT BEFORE ORDERING";

/// How the pass condition naming the modelling assumptions opens.
///
/// It lists the model-fidelity assumptions the checks were judged on (partly
/// modelled parts, stand-in and reduced-fidelity models, inferred pin roles,
/// default values), grouped by their glossary label. Checks that did not run,
/// parts with no model and reader gaps are not modelling assumptions and have
/// their own conditions.
pub const MODELLING_ASSUMPTIONS_LEAD: &str =
    "Judged on modelling assumptions, each listed in the evidence with how to lift it";

/// The pass condition that counts the abstentions a run does not name one by
/// one; `{n}` is how many and `{subjects}` the parts or nets they cover. Each
/// one stays among the findings with what it needs.
pub const MORE_ABSTENTIONS: &str =
    "{n} more checks could not judge the part or net they cover ({subjects}); each is listed \
     among the findings with what it needs.";

/// How the pass condition naming checks this run did not judge opens.
///
/// A check that could not run on these inputs, or a part of the board the
/// run never exercised, grouped by glossary label ("Not checked (drc)").
pub const NOT_JUDGED_BY_RUN_LEAD: &str =
    "Not judged by this run, so finding nothing there is no evidence that nothing is wrong";

/// How the pass condition naming inputs read past a known reader gap opens.
pub const READER_LIMITATION_LEAD: &str =
    "Could not be read with confidence, so findings there may be missing or wrong";

/// The pass condition for a run that treated every placed part as fitted;
/// `{subjects}` names the input that carries no BOM or populate flag.
pub const FITTED_BY_DEFAULT_CONDITION: &str =
    "Every placed part was judged as fitted, because {subjects} carries no BOM or populate flag: \
     a part the board leaves unfitted (DNP) was judged as present.";

/// What to do when a back-powering finding names a pin the datasheet documents
/// as tolerant of the higher rail.
///
/// The answer is the model-card row that records it, so the
/// warning becomes a verified exception. `card_file` is the user-supplied card
/// (a `--models-dir` or user-directory file) when the part's model came from
/// one, so the fix names the file to edit.
///
/// One definition: the plain report and the `--json` fix hint both read it.
#[must_use]
pub fn back_power_tolerance_fix(card_file: Option<&str>) -> String {
    let card = card_file.map_or_else(
        || "the part's model card".to_string(),
        |file| format!("the part's model card, `{file}`,"),
    );
    format!(
        "If the pin is documented as tolerant of the higher rail (some are), record that in \
         {card} so the warning becomes a verified exception: add an `input_tolerance` row to \
         its `[[models.envelope]]` (kind = \"input_tolerance\", pins = [the pin's role], max_v, \
         abs_max_v, basis = the datasheet table the figures come from)."
    )
}

/// The pass condition for parts with no model on a pass (a verdict-critical
/// one makes the run PARTLY JUDGED instead): `{n}` is how many and
/// `{subjects}` names them, a few by name and the rest counted.
#[must_use]
pub fn open_parts_condition(n: usize, subjects: &str) -> String {
    if n == 1 {
        format!(
            "1 part has no model and was not judged ({subjects}); it is listed in the evidence \
             with how to model it."
        )
    } else {
        format!(
            "{n} parts have no model and were not judged ({subjects}); each is listed in the \
             evidence with how to model it."
        )
    }
}

/// What a run concluded. Ordered from best to worst for the reader: a pass,
/// a pass with named conditions, a fail, and the two outcomes that vouch for
/// nothing (partly judged, invalid run).
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
)]
#[serde(rename_all = "snake_case")]
pub enum VerdictState {
    /// Every check in scope ran on trustworthy evidence and nothing blocks.
    Pass,
    /// Nothing blocks, but the pass rests on named conditions.
    PassConditional,
    /// At least one blocking finding or failed assertion.
    Fail,
    /// Nothing blocks among the checks that ran, but key parts were not judged.
    PartlyJudged,
    /// The run's own evidence is not trustworthy, so it vouches for nothing.
    InvalidRun,
}

impl VerdictState {
    /// Every state, in display order.
    pub const ALL: [Self; 5] = [
        Self::Pass,
        Self::PassConditional,
        Self::Fail,
        Self::PartlyJudged,
        Self::InvalidRun,
    ];

    /// The wire word, identical to what serde writes.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::PassConditional => "pass_conditional",
            Self::Fail => "fail",
            Self::PartlyJudged => "partly_judged",
            Self::InvalidRun => "invalid_run",
        }
    }

    /// The display label every human surface prints.
    ///
    /// A state that is neither a pass nor a fail reads "`<word>`, not a pass":
    /// the app's verdict card shows the word as its headline ("PARTLY
    /// JUDGED", "INCONCLUSIVE") and the rest as its qualifier, the words the
    /// CLI's [`PARTLY_JUDGED_LEAD`] and closing `VERDICT: inconclusive` use.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::PassConditional => "pass, conditionally",
            Self::Fail => "fail",
            Self::PartlyJudged => "partly judged, not a pass",
            Self::InvalidRun => "inconclusive, not a pass",
        }
    }

    /// One line saying what the state means, for `explain verdicts`, a
    /// document's verdict box and a tooltip.
    #[must_use]
    pub const fn meaning(self) -> &'static str {
        match self {
            Self::Pass => "Every check in scope ran on trustworthy evidence and nothing blocks.",
            Self::PassConditional => {
                "Nothing blocks, but the pass rests on named conditions (default rules, \
                 guessed or fallback models, partly modelled parts, inferred pin roles, parts \
                 with no model, checks that could not judge a part or did not run); each \
                 condition says how to lift it."
            }
            Self::Fail => "At least one blocking finding or failed assertion.",
            Self::PartlyJudged => {
                "Nothing blocks among the checks that ran, but key parts have no usable \
                 model, so the checks they could change were not judged."
            }
            Self::InvalidRun => {
                "The run's own evidence is not trustworthy (a refusal, an undermined claim, \
                 a failed simulation), so it vouches for nothing either way."
            }
        }
    }

    /// The CI word printed next to the canonical label, where CI has one.
    #[must_use]
    pub const fn ci_alias(self) -> Option<&'static str> {
        match self {
            Self::Pass => Some("GREEN"),
            Self::PassConditional => Some("GREEN, conditionally"),
            Self::Fail => Some("RED"),
            Self::PartlyJudged | Self::InvalidRun => None,
        }
    }

    /// The exit class this state ends a run with under [`EXIT_CONTRACT`].
    #[must_use]
    pub const fn exit_class(self) -> ExitClass {
        match self {
            Self::Pass | Self::PassConditional => ExitClass::Pass,
            Self::Fail => ExitClass::Fail,
            Self::PartlyJudged | Self::InvalidRun => ExitClass::InvalidRun,
        }
    }

    /// Whether the state is a pass (conditional or not). Partly judged and
    /// invalid runs are never a pass.
    #[must_use]
    pub const fn is_pass(self) -> bool {
        matches!(self, Self::Pass | Self::PassConditional)
    }

    /// The spellings this state had before the shared vocabulary, across the
    /// `run --json` v3 `verdict`, the terminal, `hauksbee-ci` and the app.
    #[must_use]
    pub const fn aliases(self) -> &'static [&'static str] {
        match self {
            Self::Pass => &["pass", "clean", "GREEN", "OK TO ORDER", "analysis clean"],
            Self::PassConditional => &[
                "clean, conditionally",
                "GREEN, conditionally",
                "OK TO ORDER, conditionally",
                "conditional_pass",
            ],
            Self::Fail => &["fail", "failing", "RED", "FAIL", "DO NOT ORDER"],
            Self::PartlyJudged => &["PARTLY JUDGED", "partly judged, not a pass"],
            Self::InvalidRun => &[
                "invalid",
                "inconclusive",
                "INCONCLUSIVE",
                "NOT CONCLUSIVE",
                "INVALID",
                "analysis undermined",
                "invalid for analysis",
            ],
        }
    }

    /// Map the `run --json` v3 verdict (`"pass"` / `"fail"` / `"invalid"`)
    /// and its two qualifiers onto a state. `partial` is the v3 PARTLY JUDGED
    /// flag; `conditional` is whether `verdict_conditions` is non-empty.
    /// Returns `None` for a word v3 never wrote.
    #[must_use]
    pub fn from_v3(verdict: &str, partial: bool, conditional: bool) -> Option<Self> {
        Some(match verdict {
            "pass" if conditional => Self::PassConditional,
            "pass" => Self::Pass,
            "fail" => Self::Fail,
            "invalid" if partial => Self::PartlyJudged,
            "invalid" => Self::InvalidRun,
            _ => return None,
        })
    }
}

impl std::fmt::Display for VerdictState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// The shared exit table: one meaning per number, in both binaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExitClass {
    /// 0: a pass or a conditional pass.
    Pass,
    /// 1: a fail.
    Fail,
    /// 2: a usage, spec or input error; the run did not start.
    Usage,
    /// 3: partly judged or invalid run; not a pass and not a fail.
    InvalidRun,
    /// 4: an internal error in hauksbee itself.
    ToolFault,
}

impl ExitClass {
    /// Every class, in code order.
    pub const ALL: [Self; 5] = [
        Self::Pass,
        Self::Fail,
        Self::Usage,
        Self::InvalidRun,
        Self::ToolFault,
    ];

    /// The process exit code.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Pass => 0,
            Self::Fail => 1,
            Self::Usage => 2,
            Self::InvalidRun => 3,
            Self::ToolFault => 4,
        }
    }

    /// The wire word, identical to what serde writes.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Usage => "usage",
            Self::InvalidRun => "invalid_run",
            Self::ToolFault => "tool_fault",
        }
    }

    /// The display label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Usage => "usage or input error",
            Self::InvalidRun => "not judged",
            Self::ToolFault => "tool fault",
        }
    }

    /// What each code meant before the shared table.
    #[must_use]
    pub const fn aliases(self) -> &'static [&'static str] {
        match self {
            Self::Pass => &["clean or report-only (hauksbee)", "GREEN (hauksbee-ci)"],
            Self::Fail => &["input error (hauksbee)", "RED (hauksbee-ci)"],
            Self::Usage => &[
                "strict findings or usage error (hauksbee)",
                "spec, usage or output error (hauksbee-ci)",
            ],
            Self::InvalidRun => &["invalid for analysis"],
            Self::ToolFault => &[],
        }
    }

    /// One line saying what the code means.
    #[must_use]
    pub const fn meaning(self) -> &'static str {
        match self {
            Self::Pass => "Pass or conditional pass.",
            Self::Fail => "Fail: a blocking finding or a failed assertion.",
            Self::Usage => "Usage, spec or input error: the run did not start.",
            Self::InvalidRun => "Partly judged or inconclusive: not a pass, and not a fail.",
            Self::ToolFault => "Internal error in hauksbee itself; please report it.",
        }
    }
}

/// How much a finding matters, on one ordered scale (`info` lowest,
/// `serious` highest).
///
/// Whether it fails the run is a separate flag,
/// [`BLOCKING`]: the gate decides that per check (a lint warning can block, a
/// DRC warning does not), and every renderer leads with it ("2 blocking,
/// 0 serious, 2 worth a look") so the headline and the verdict agree.
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
)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Context, not a problem.
    Info,
    /// Worth knowing; no action expected.
    Note,
    /// Worth a look before ordering.
    Warning,
    /// A real problem with the design.
    Serious,
}

impl Severity {
    /// Every grade, highest first.
    pub const ALL: [Self; 4] = [Self::Serious, Self::Warning, Self::Note, Self::Info];

    /// The wire word, identical to what serde writes.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Note => "note",
            Self::Warning => "warning",
            Self::Serious => "serious",
        }
    }

    /// The display label (the terminal already says "worth a look").
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Note => "note",
            Self::Warning => "worth a look",
            Self::Serious => "serious",
        }
    }

    /// SARIF 2.1.0 `result.level` of a finding with this grade. A blocking
    /// finding is `error` whatever its grade ([`sarif_level`]).
    #[must_use]
    pub const fn sarif_level(self) -> &'static str {
        match self {
            Self::Info | Self::Note => "note",
            Self::Warning => "warning",
            Self::Serious => "error",
        }
    }

    /// `severity` in the `kicad-cli pcb drc --format json` report shape. A
    /// blocking finding is `error` and a waived one `exclusion`
    /// ([`kicad_severity`]).
    #[must_use]
    pub const fn kicad_severity(self) -> &'static str {
        match self {
            Self::Info | Self::Note => "info",
            Self::Warning => "warning",
            Self::Serious => "error",
        }
    }

    /// Older or tool-specific spellings of this grade.
    #[must_use]
    pub const fn aliases(self) -> &'static [&'static str] {
        match self {
            Self::Info => &["Info"],
            Self::Note => &["Low", "[note]"],
            Self::Warning => &["Medium"],
            Self::Serious => &["High"],
        }
    }

    /// One line saying what the grade means.
    #[must_use]
    pub const fn meaning(self) -> &'static str {
        match self {
            Self::Info => "Context, not a problem.",
            Self::Note => "Worth knowing; no action expected.",
            Self::Warning => "Worth a look before ordering.",
            Self::Serious => "A real problem with the design.",
        }
    }

    /// Map a grade spelled by an existing producer: the engine's
    /// `serious`/`warning`/`note`/`info` and the checks crate's
    /// `High`/`Medium`/`Low`/`Info` (any case).
    #[must_use]
    pub fn from_legacy(word: &str) -> Option<Self> {
        Some(match word.to_ascii_lowercase().as_str() {
            "serious" | "high" | "error" => Self::Serious,
            "warning" | "medium" => Self::Warning,
            "note" | "low" => Self::Note,
            "info" => Self::Info,
            _ => return None,
        })
    }
}

/// The blocking flag: the gate fails the run on this finding unless a waiver
/// covers it. Independent of [`Severity`].
pub const BLOCKING: Entry = Entry {
    id: "blocking",
    label: "blocking",
    meaning: "The gate fails the run on this finding unless it is waived. Independent of \
              severity: a gating warning blocks.",
    aliases: &["gating", "blocking problem"],
};

/// SARIF `result.level` for a finding: `error` when it blocks, else its
/// grade's level.
#[must_use]
pub const fn sarif_level(severity: Severity, blocking: bool) -> &'static str {
    if blocking {
        "error"
    } else {
        severity.sarif_level()
    }
}

/// KiCad report `severity` for a finding: `exclusion` when waived, `error`
/// when it blocks, else its grade's severity.
#[must_use]
pub const fn kicad_severity(severity: Severity, blocking: bool, waived: bool) -> &'static str {
    if waived {
        "exclusion"
    } else if blocking {
        "error"
    } else {
        severity.kicad_severity()
    }
}

/// How much a conclusion's evidence can be trusted.
///
/// The record's spelling of [`EvidenceStatus`]: `full` replaces that type's `clean`, which collided
/// with the verdict word "clean"; `clean` stays the wire word in `run --json`
/// v3 and `hauksbee-ci --json` v2 until those documents become the record.
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
)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceGrade {
    /// No assumption on the causal path.
    Full,
    /// Assumptions on the path, none critical.
    Qualified,
    /// A critical assumption on the path: no verdict.
    Undermined,
}

impl EvidenceGrade {
    /// Every grade, best first.
    pub const ALL: [Self; 3] = [Self::Full, Self::Qualified, Self::Undermined];

    /// The wire word, identical to what serde writes.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Qualified => "qualified",
            Self::Undermined => "undermined",
        }
    }

    /// The display label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Full => "full evidence",
            Self::Qualified => "qualified evidence",
            Self::Undermined => "undermined evidence",
        }
    }

    /// One line saying what the grade means.
    #[must_use]
    pub const fn meaning(self) -> &'static str {
        match self {
            Self::Full => {
                "The conclusion stands on evidence alone: nothing on its causal path was assumed."
            }
            Self::Qualified => {
                "The conclusion holds with named caveats: assumptions sit on its path, none \
                 critical."
            }
            Self::Undermined => {
                "A critical assumption sits on the causal path, so the conclusion gets no verdict."
            }
        }
    }

    /// Older spellings.
    #[must_use]
    pub const fn aliases(self) -> &'static [&'static str] {
        match self {
            Self::Full => &["clean"],
            Self::Qualified => &["analysis qualified"],
            Self::Undermined => &["UNDERMINED", "analysis undermined"],
        }
    }
}

impl From<EvidenceStatus> for EvidenceGrade {
    fn from(status: EvidenceStatus) -> Self {
        match status {
            EvidenceStatus::Clean => Self::Full,
            EvidenceStatus::Qualified => Self::Qualified,
            EvidenceStatus::Undermined => Self::Undermined,
        }
    }
}

/// A check or analysis a run can perform: the `check` of a finding and the
/// names in `scope.checks_run`.
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
)]
#[serde(rename_all = "snake_case")]
pub enum Check {
    Drc,
    Lint,
    Si,
    Resources,
    UsbC,
    Thermal,
    Ac,
    Cosim,
    /// Spec assertions (`hauksbee-ci`).
    Spec,
    /// Run-level evidence findings (an undermined claim).
    Evidence,
}

impl Check {
    /// Every check, in report order.
    pub const ALL: [Self; 10] = [
        Self::Drc,
        Self::Lint,
        Self::Si,
        Self::Resources,
        Self::UsbC,
        Self::Thermal,
        Self::Ac,
        Self::Cosim,
        Self::Spec,
        Self::Evidence,
    ];

    /// The wire word, identical to what serde writes.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Drc => "drc",
            Self::Lint => "lint",
            Self::Si => "si",
            Self::Resources => "resources",
            Self::UsbC => "usb_c",
            Self::Thermal => "thermal",
            Self::Ac => "ac",
            Self::Cosim => "cosim",
            Self::Spec => "spec",
            Self::Evidence => "evidence",
        }
    }

    /// The display label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Drc => "Design rules (DRC)",
            Self::Lint => "Connectivity lint",
            Self::Si => "Signal integrity",
            Self::Resources => "MCU resources",
            Self::UsbC => "USB-C",
            Self::Thermal => "Thermal",
            Self::Ac => "AC response",
            Self::Cosim => "Firmware co-simulation",
            Self::Spec => "Spec assertions",
            Self::Evidence => "Evidence",
        }
    }

    /// One line saying what the check does.
    #[must_use]
    pub const fn meaning(self) -> &'static str {
        match self {
            Self::Drc => {
                "Copper geometry against the clearance and width rules: shorts, spacing, \
                 unrouted pads."
            }
            Self::Lint => {
                "The netlist the copper implements: floating inputs, unpowered parts, wrong \
                 values."
            }
            Self::Si => "Signal integrity of fast nets: impedance, length and coupling.",
            Self::Resources => {
                "MCU pins and peripherals the board uses, against what the part provides."
            }
            Self::UsbC => "USB-C configuration: CC resistors, VBUS and the receptacle wiring.",
            Self::Thermal => "Dissipation and temperature rise of each part against its ratings.",
            Self::Ac => "Small-signal frequency response of the solved circuit.",
            Self::Cosim => {
                "Compiled firmware run against the solved board, with ratings checked over time."
            }
            Self::Spec => "The assertions a hauksbee-ci spec states, each judged against the run.",
            Self::Evidence => "Claims whose own evidence is undermined, reported as findings.",
        }
    }

    /// Other spellings (command-line flags, older words).
    #[must_use]
    pub const fn aliases(self) -> &'static [&'static str] {
        match self {
            Self::Drc => &["--drc"],
            Self::Lint => &["--lint"],
            Self::Si => &["--si"],
            Self::Resources => &["--resources"],
            Self::UsbC => &["--usb-c", "usb_c_cc"],
            Self::Thermal => &["--thermal"],
            Self::Ac => &["--ac"],
            Self::Cosim => &["co-sim", "--headless"],
            Self::Spec => &["ci", "assertion"],
            Self::Evidence => &[],
        }
    }
}

/// One vocabulary entry: the shape every domain shares.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Entry {
    pub id: &'static str,
    pub label: &'static str,
    pub meaning: &'static str,
    pub aliases: &'static [&'static str],
}

/// A verdict state's entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VerdictEntry {
    #[serde(flatten)]
    pub entry: Entry,
    pub ci_alias: Option<&'static str>,
    pub exit_code: u8,
}

/// A severity grade's entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SeverityEntry {
    #[serde(flatten)]
    pub entry: Entry,
    pub sarif_level: &'static str,
    pub kicad_severity: &'static str,
}

/// An exit code's entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExitEntry {
    #[serde(flatten)]
    pub entry: Entry,
    pub code: u8,
}

/// The `schema` value of [`Vocabulary`].
pub const VOCABULARY_SCHEMA: &str = "hauksbee.vocabulary/1";

/// The whole vocabulary, as data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Vocabulary {
    /// Always [`VOCABULARY_SCHEMA`].
    pub schema: &'static str,
    pub exit_contract: &'static str,
    pub verdicts: Vec<VerdictEntry>,
    pub evidence_grades: Vec<Entry>,
    pub severities: Vec<SeverityEntry>,
    /// [`BLOCKING`].
    pub blocking: Entry,
    pub exit_codes: Vec<ExitEntry>,
    pub checks: Vec<Entry>,
    /// [`crate::glossary::all`], by reference: the glossary stays the one
    /// place its terms are explained.
    pub glossary: Vec<TermEntry>,
    /// [`crate::labels::label_table`]: plain-English words for the internal ids
    /// (capabilities, coverage stages, model tiers, emulator backends and the
    /// words above) that reach a human surface.
    pub labels: crate::labels::LabelTable,
    /// [`crate::observations::observation_kinds`]: the pin states, core states,
    /// bus kinds, timeline events and probe-seed reasons a co-sim run is
    /// described with.
    pub observations: Vec<crate::observations::ObservationEntry>,
}

/// `explain verdicts`: every state, in display order.
#[must_use]
pub fn verdicts() -> Vec<VerdictEntry> {
    VerdictState::ALL
        .iter()
        .map(|&s| VerdictEntry {
            entry: Entry {
                id: s.as_str(),
                label: s.label(),
                meaning: s.meaning(),
                aliases: s.aliases(),
            },
            ci_alias: s.ci_alias(),
            exit_code: s.exit_class().code(),
        })
        .collect()
}

/// `explain severities`: the scale, highest first, with its SARIF and KiCad
/// mappings.
#[must_use]
pub fn severities() -> Vec<SeverityEntry> {
    Severity::ALL
        .iter()
        .map(|&s| SeverityEntry {
            entry: Entry {
                id: s.as_str(),
                label: s.label(),
                meaning: s.meaning(),
                aliases: s.aliases(),
            },
            sarif_level: s.sarif_level(),
            kicad_severity: s.kicad_severity(),
        })
        .collect()
}

/// `explain exit-codes`: the shared table under [`EXIT_CONTRACT`].
#[must_use]
pub fn exit_codes() -> Vec<ExitEntry> {
    ExitClass::ALL
        .iter()
        .map(|&c| ExitEntry {
            entry: Entry {
                id: c.as_str(),
                label: c.label(),
                meaning: c.meaning(),
                aliases: c.aliases(),
            },
            code: c.code(),
        })
        .collect()
}

/// The evidence grades, best first.
#[must_use]
pub fn evidence_grades() -> Vec<Entry> {
    EvidenceGrade::ALL
        .iter()
        .map(|&g| Entry {
            id: g.as_str(),
            label: g.label(),
            meaning: g.meaning(),
            aliases: g.aliases(),
        })
        .collect()
}

/// The checks, in report order.
#[must_use]
pub fn checks() -> Vec<Entry> {
    Check::ALL
        .iter()
        .map(|&c| Entry {
            id: c.as_str(),
            label: c.label(),
            meaning: c.meaning(),
            aliases: c.aliases(),
        })
        .collect()
}

/// Everything above plus the glossary: the one serialization the app and
/// `explain` read.
#[must_use]
pub fn vocabulary() -> Vocabulary {
    Vocabulary {
        schema: VOCABULARY_SCHEMA,
        exit_contract: EXIT_CONTRACT,
        verdicts: verdicts(),
        evidence_grades: evidence_grades(),
        severities: severities(),
        blocking: BLOCKING,
        exit_codes: exit_codes(),
        checks: checks(),
        glossary: crate::glossary::all(),
        labels: crate::labels::label_table(),
        observations: crate::observations::observation_kinds(),
    }
}

/// Every display label defined here and in the glossary, for the test that
/// fails when a renderer spells one itself instead of reading it.
#[must_use]
pub fn labels() -> Vec<&'static str> {
    let v = vocabulary();
    let mut out: Vec<&'static str> = v
        .verdicts
        .iter()
        .map(|e| e.entry.label)
        .chain(v.severities.iter().map(|e| e.entry.label))
        .chain(v.exit_codes.iter().map(|e| e.entry.label))
        .chain(v.evidence_grades.iter().map(|e| e.label))
        .chain(v.checks.iter().map(|e| e.label))
        .chain([v.blocking.label])
        .chain(v.glossary.iter().map(|e| e.label))
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

#[cfg(test)]
mod names_summary_tests {
    use super::*;

    #[test]
    fn a_short_list_is_named_in_full() {
        assert_eq!(names_summary(&["U1", "GND"]), "U1, GND");
        assert_eq!(names_summary::<&str>(&[]), "");
        let six: Vec<String> = (1..=NAMES_SHOWN).map(|i| format!("N{i}")).collect();
        assert_eq!(names_summary(&six), "N1, N2, N3, N4, N5, N6");
    }

    #[test]
    fn a_long_list_names_the_first_few_and_counts_the_rest() {
        let many: Vec<String> = (1..=50).map(|i| format!("N{i}")).collect();
        assert_eq!(names_summary(&many), "N1, N2, N3, N4, N5, N6 and 44 more");
    }

    #[test]
    fn a_stale_fill_names_both_nets_and_the_refill_key() {
        let said = stale_zone_fill("GND", "A", "track");
        assert_eq!(
            said,
            "zone fill is out of date (a A track lies inside the stored GND fill); \
             refill zones (B) in KiCad and re-run"
        );
    }
}

#[cfg(test)]
mod check_run_stopped_tests {
    use super::*;

    #[test]
    fn a_stopped_run_names_the_stage_how_far_it_got_and_the_terminal_command() {
        let said = check_run_stopped(Some(("Simulating", 42)), 600);
        assert!(said.contains("stopped after 10 minutes"), "{said}");
        assert!(said.contains("\"Simulating\" (42%"), "{said}");
        assert!(said.ends_with(CHECK_RUN_STOPPED_NEXT_STEP), "{said}");
        assert!(CHECK_RUN_STOPPED_NEXT_STEP.contains(CHECK_RUN_CLI_COMMAND));
    }

    #[test]
    fn a_run_that_never_reported_a_stage_still_says_what_to_do() {
        let said = check_run_stopped(None, 90);
        assert!(said.starts_with("The check run was stopped after 90 seconds because"));
        assert!(said.contains("hauksbee-ci run"));
    }

    /// The page swaps the spec placeholder for the downloaded file's name. The
    /// placeholder lives here (the engine's words) and in the page's
    /// `SPEC_FILE_PLACEHOLDER`; this holds the two equal.
    #[test]
    fn the_page_replaces_the_same_spec_placeholder_the_terminal_command_names() {
        assert!(
            CHECK_RUN_CLI_COMMAND.ends_with("<spec.toml>"),
            "{CHECK_RUN_CLI_COMMAND}"
        );
        assert!(CHECK_RUN_STOPPED_NEXT_STEP.contains("<spec.toml>"));
        let page = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../frontend/src/lib/ci-workflow.ts"
        ))
        .expect("the page's ci-workflow.ts is readable from the crate");
        assert!(
            page.contains("'<spec.toml>'"),
            "frontend/src/lib/ci-workflow.ts must define SPEC_FILE_PLACEHOLDER as '<spec.toml>'"
        );
    }
}

#[cfg(test)]
mod value_scope_tests {
    use super::*;

    #[test]
    fn the_scope_sentence_agrees_in_number_and_names_the_fix() {
        let one = values_not_carried_scope(1, "C1");
        assert!(one.starts_with("1 part carries no value in this input (C1), so its value is"));
        let many = values_not_carried_scope(23, "C1, C2 and 21 more");
        assert!(many.starts_with("23 parts carry no value in this input"));
        for s in [&one, &many] {
            assert!(s.contains("not judged"), "{s}");
            assert!(s.ends_with(VALUES_NOT_CARRIED_NEXT_STEP), "{s}");
            assert!(s.contains("--bom"), "{s}");
        }
    }

    #[test]
    fn the_reason_lead_is_lower_case_so_it_matches_a_lower_cased_reason() {
        assert_eq!(
            VALUE_NOT_CARRIED_REASON_LEAD,
            VALUE_NOT_CARRIED_REASON_LEAD.to_lowercase()
        );
    }
}
