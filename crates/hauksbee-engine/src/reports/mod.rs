//! One module per report family.
//!
//! Each builds the **structured** finding type (the honesty layer in
//! [`crate::result`]) and renders it in exactly one of the three output
//! surfaces. The per-flag `if json / else if plain / else` triplication
//! collapses into a single `match` on [`OutputMode`] per report, which keeps
//! `cmd_run` a thin dispatcher: pick the mode once, call the right report's
//! `emit`.
//!
//! Rendering itself lives elsewhere: each `emit` delegates to the shared
//! renderers (`DrcStructured::render_with_clearance_rule_provenance`, `plain_*`,
//! `JsonReport`, the extract-crate text renderers), so every surface emits
//! byte-identical output for the same report. The per-surface map and the
//! register of deliberate divergences are in [`crate::result`]'s module doc.

pub mod ac;
pub mod ampacity;
pub mod bind;
pub mod check;
pub mod ci_artifacts;
pub mod cosim;
pub mod coverage;
pub mod drc;
pub(crate) mod firmware_intent;
pub mod lint;
pub mod si;
pub mod thermal;
pub mod usb_c;

use hauksbee_ir::{eprintln, println};
use std::path::Path;

use anyhow::Context;
use hauksbee_extract::ExtractedBoard;

/// Truncate to at most `max` chars.
pub(crate) fn truncate(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// The output surface a report renders into, resolved once from the CLI flags so
/// each report matches it a single time instead of re-checking `--json`/`--plain`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OutputMode {
    /// The default box-drawing / expert text tables.
    Text,
    /// Plain-language prose (`--plain` / `--explain`).
    Plain,
    /// Machine-readable JSON (`--json`).
    Json,
}

impl OutputMode {
    /// Resolve the surface from the two CLI flags. `--json` wins over `--plain`
    /// (a machine consumer never wants prose).
    #[must_use]
    pub const fn from_flags(json: bool, plain: bool) -> Self {
        if json {
            Self::Json
        } else if plain {
            Self::Plain
        } else {
            Self::Text
        }
    }
}

/// Human evidence appendix policy shared by report surfaces. Quiet mode keeps
/// the verdict, findings, and coverage/open-part summaries emitted by each
/// report, but replaces the potentially hundreds-of-lines per-item appendix
/// with one recovery instruction.
pub(crate) fn render_evidence_appendix(
    evidence: &crate::evidence::BoardEvidence,
    quiet: bool,
    verbose: bool,
) -> String {
    let rendered = if verbose {
        evidence.render_plain()
    } else {
        evidence.render_plain_compact()
    };
    if rendered.is_empty() || !quiet {
        rendered
    } else {
        "\nEvidence appendix hidden by --quiet; rerun without --quiet to show the compact evidence appendix.\n".to_string()
    }
}

/// The one prominent note printed by `--check`/`--drc` when a `KiCad` layout
/// carries no routed copper at all: the spacing check then had only pads
/// to compare, and a clean result must not read as "the routing is clean" on
/// a board that has no routing yet.
pub(crate) const UNROUTED_COPPER_NOTE: &str =
    "note: this board has no routed copper (no track segments): the spacing check \
     had only pads to compare, so a clean result here says nothing about routing \
     that does not exist yet.";

/// True when a `KiCad` layout text carries no routed copper: no track segments
/// and no zones (a filled zone is copper too, so its presence disables the
/// caveat rather than risk crying wolf). Only meaningful for `.kicad_pcb`
/// text; other formats return false and print nothing.
pub(crate) fn unrouted_kicad_layout(text: &str) -> bool {
    text.contains("(kicad_pcb") && !text.contains("(segment") && !text.contains("(zone")
}

/// Read project-file netclass clearances and resolve them to this board's net names.
///
/// `KiCad` 10 stores this in the sibling `.kicad_pro` rather than the
/// `.kicad_pcb`; missing/malformed project files simply leave DRC on the
/// board/default rules. Shared by the `--drc`, `--check` and combined-`--json`
/// reports.
pub fn kicad_pro_clearance_rules(
    board_path: &Path,
    board: &ExtractedBoard,
) -> Option<hauksbee_extract::ClearanceRules> {
    let text = std::fs::read_to_string(board_path.with_extension("kicad_pro")).ok()?;
    // A syntactically valid project with no usable class still leaves the DRC
    // on the extractor fallback. Do not let `Some(defaults)` promote that to
    // project-backed evidence.
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let has_usable_rule = value
        .pointer("/net_settings/classes")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|classes| {
            classes.iter().any(|class| {
                class
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .is_some()
                    && (class
                        .get("clearance")
                        .and_then(serde_json::Value::as_f64)
                        .is_some_and(|value| value > 0.0)
                        || class
                            .get("diff_pair_gap")
                            .and_then(serde_json::Value::as_f64)
                            .is_some_and(|value| value > 0.0))
            })
        });
    if !has_usable_rule {
        return None;
    }
    hauksbee_extract::clearance_rules_from_kicad_pro(
        &text,
        board.nets.iter().map(|n| n.name.as_str()),
    )
}

/// Preserve the provenance that [`kicad_pro_clearance_rules`] alone cannot express.
///
/// `rules_read=false` is always defaulted, whether the sibling is absent or
/// present-but-unusable; the latter distinction is retained only so the human
/// instruction does not falsely say the file was missing.
#[must_use]
pub fn kicad_pro_clearance_rule_provenance(
    board_path: &Path,
    layout_text: &str,
    rules_read: bool,
    value_mm: f64,
) -> crate::result::ClearanceRuleProvenance {
    if rules_read {
        crate::result::ClearanceRuleProvenance::project_file(value_mm)
    } else if !layout_text.contains("(kicad_pcb") {
        crate::result::ClearanceRuleProvenance::board_file(value_mm)
    } else {
        crate::result::ClearanceRuleProvenance::defaulted(
            value_mm,
            board_path.with_extension("kicad_pro").is_file(),
        )
    }
}

#[derive(Debug, Clone)]
pub struct KicadClearanceInput {
    pub rules: Option<hauksbee_extract::ClearanceRules>,
    project_rules_read: bool,
    project_value_mm: Option<f64>,
    custom_global_mm: Option<f64>,
    custom_coverage: Option<crate::result::CustomRulesCoverage>,
}

impl KicadClearanceInput {
    /// Load the sibling `.kicad_pro` netclass rules and `.kicad_dru` custom rules.
    ///
    /// # Errors
    ///
    /// Returns an error if a sibling `.kicad_dru` exists but cannot be read from
    /// disk. A `.kicad_dru` (or one of its rules) the reader cannot interpret is
    /// not an error: it is disclosed as not checked. A missing or unusable
    /// `.kicad_pro` is not an error either.
    pub fn load(board_path: &Path, board: &ExtractedBoard) -> anyhow::Result<Self> {
        let mut rules = kicad_pro_clearance_rules(board_path, board);
        let project_rules_read = rules.is_some();
        let project_value_mm = rules.as_ref().map(|rules| rules.default_clearance_mm);
        let custom_path = board_path.with_extension("kicad_dru");
        let (custom_global_mm, custom_coverage) = if custom_path.is_file() {
            let text = std::fs::read_to_string(&custom_path).with_context(|| {
                format!("failed to read custom rules file {}", custom_path.display())
            })?;
            let file_name = custom_path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("sibling .kicad_dru")
                .to_string();
            // A file or rule this reader cannot interpret never stops the run:
            // it is disclosed as a rule that was not checked.
            match hauksbee_extract::parse_kicad_dru(&text) {
                Ok(parsed) => {
                    let coverage =
                        crate::result::CustomRulesCoverage::from_parsed(file_name, &parsed);
                    let global = parsed.global_clearance_mm();
                    // Every clearance rule the file sets governs the pairs its
                    // condition and layer select, last matching rule first;
                    // the unconditional one is also the report-wide value.
                    let custom = parsed.clearance_rules();
                    if !custom.is_empty() {
                        rules
                            .get_or_insert_with(hauksbee_extract::ClearanceRules::default)
                            .set_custom_rules(custom);
                    }
                    if let Some(clearance_mm) = global {
                        rules
                            .get_or_insert_with(hauksbee_extract::ClearanceRules::default)
                            .apply_global_clearance_override(clearance_mm);
                    }
                    (global, Some(coverage))
                }
                Err(error) => (
                    None,
                    Some(crate::result::CustomRulesCoverage::unreadable(
                        file_name,
                        error.to_string(),
                    )),
                ),
            }
        } else {
            (None, None)
        };
        Ok(Self {
            rules,
            project_rules_read,
            project_value_mm,
            custom_global_mm,
            custom_coverage,
        })
    }

    /// The clearance rule's provenance for this board's DRC.
    ///
    /// # Panics
    ///
    /// Does not panic in practice: a custom global clearance is only ever set by
    /// [`Self::load`] together with the parsed custom-rule coverage it `expect`s.
    #[must_use]
    pub fn provenance(
        &self,
        board_path: &Path,
        layout_text: &str,
        value_mm: f64,
    ) -> crate::result::ClearanceRuleProvenance {
        if self.custom_global_mm.is_some() {
            return crate::result::ClearanceRuleProvenance::custom_rules_file(
                value_mm,
                self.project_value_mm,
                self.custom_coverage
                    .clone()
                    .expect("a custom global rule came from parsed custom-rule coverage"),
            );
        }
        let provenance = kicad_pro_clearance_rule_provenance(
            board_path,
            layout_text,
            self.project_rules_read,
            value_mm,
        );
        match &self.custom_coverage {
            Some(coverage) => provenance.with_custom_rules(coverage.clone()),
            None => provenance,
        }
    }

    #[must_use]
    pub const fn custom_coverage(&self) -> Option<&crate::result::CustomRulesCoverage> {
        self.custom_coverage.as_ref()
    }
}

/// The normalized board inputs every static report reads, bundled once so each
/// surface takes one argument instead of eight.
#[derive(Clone, Copy)]
pub(crate) struct StaticInputs<'a> {
    pub board_path: &'a Path,
    pub board: &'a ExtractedBoard,
    pub text: &'a str,
    pub raw: &'a [u8],
    pub input_kind: crate::board_input::InputKind,
    pub altium_present: bool,
    pub lib: &'a hauksbee_models::ModelLibrary,
    pub reader_notes: &'a [String],
    pub inputs: &'a [crate::result::JsonInputEvidence],
    /// The `--firmware` image, which the static suite reads for what the program
    /// means to use (see [`firmware_intent`]).
    pub firmware: Option<&'a Path>,
}

impl StaticInputs<'_> {
    /// The evidence spine every bound static surface attaches: the board's bind
    /// report dated by the run clock, carrying the input artifact it came from.
    pub(crate) fn evidence(
        &self,
        report: &crate::reports::bind::BindReport,
    ) -> Result<crate::evidence::BoardEvidence, hauksbee_ir::evidence::EvidenceError> {
        crate::evidence::board_evidence(
            self.board,
            report,
            self.reader_notes,
            self.board_path,
            self.raw,
            self.input_kind,
        )
    }

    /// [`run_drc`] over these inputs.
    fn drc(&self) -> anyhow::Result<(Option<KicadClearanceInput>, hauksbee_extract::DrcReport)> {
        run_drc(
            self.board_path,
            self.board,
            self.text,
            self.raw,
            self.altium_present,
        )
    }

    /// The layout text the geometry-bearing SI checks read; Altium geometry is
    /// not yet threaded into them, so `None` there.
    fn geo_text(&self) -> Option<&str> {
        (!self.altium_present).then_some(self.text)
    }
}

/// Run the geometric DRC: Altium's own rules, else `KiCad`'s with the sibling
/// project/custom-rule clearances (returned too, for provenance and coverage;
/// `None` exactly on Altium). Shared by `--drc`, `--check` and the CI finding
/// gatherer so they can never run different DRCs over one board.
pub(crate) fn run_drc(
    board_path: &Path,
    board: &ExtractedBoard,
    text: &str,
    raw: &[u8],
    altium_present: bool,
) -> anyhow::Result<(Option<KicadClearanceInput>, hauksbee_extract::DrcReport)> {
    let clearance_input = (!altium_present)
        .then(|| KicadClearanceInput::load(board_path, board))
        .transpose()?;
    let mut report = if altium_present {
        ExtractedBoard::altium_drc(raw)?
    } else {
        // KiCad 10 keeps class clearances in the sibling .kicad_pro. Resolve
        // concrete net names here (the CLI has both the board path and the
        // extracted netlist), then hand the DRC a pairwise clearance resolver.
        ExtractedBoard::drc_with_clearance_rules(
            text,
            clearance_input
                .as_ref()
                .and_then(|input| input.rules.clone()),
        )?
    };
    if !altium_present {
        let pro = std::fs::read_to_string(board_path.with_extension("kicad_pro")).ok();
        let exclusions: Vec<String> = pro
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .and_then(|value| {
                value
                    .pointer("/board/design_settings/drc_exclusions")
                    .and_then(serde_json::Value::as_array)
                    .map(|list| {
                        list.iter()
                            .filter_map(|e| e.as_str().map(String::from))
                            .collect()
                    })
            })
            .unwrap_or_default();
        report.apply_kicad_exclusions(exclusions.iter().map(String::as_str));
    }
    Ok((clearance_input, report))
}

/// Where the DRC's clearance rule came from: the tool default on Altium, else
/// the `KiCad` inputs [`run_drc`] loaded.
pub(crate) fn clearance_provenance(
    input: Option<&KicadClearanceInput>,
    board_path: &Path,
    text: &str,
    clearance_mm: f64,
) -> crate::result::ClearanceRuleProvenance {
    input.map_or_else(
        || crate::result::ClearanceRuleProvenance::tool_default(clearance_mm),
        |input| input.provenance(board_path, text, clearance_mm),
    )
}

/// The schematic contributed to the verdict, so it enters the inventory with
/// its own hash and what it did. A reader who sees a contact reported as a
/// declared tie must be able to find the file that declared it.
pub(crate) fn with_schematic_evidence(
    evidence: crate::evidence::BoardEvidence,
    ties: Option<&crate::schematic_ties::SchematicTies>,
    qualification: Option<&hauksbee_extract::DrcTieQualification>,
) -> Result<crate::evidence::BoardEvidence, hauksbee_ir::evidence::EvidenceError> {
    match (ties, qualification) {
        (Some(ties), Some(qualification)) => evidence.with_schematic_artifact(
            &ties.path,
            &ties.raw,
            ties.contribution(qualification),
        ),
        _ => Ok(evidence),
    }
}

/// Strict-mode predicate for the connectivity/resource lint: any high/medium
/// finding fails the gate.
///
/// Asked of the machine findings, not of the severity words: the CI artifacts
/// mark a testcase failed from [`crate::result::JsonFinding::gates`], and a
/// gate that re-derived the rule here could grade the same run differently
/// from the file the pipeline archives.
pub fn lint_fails(report: &hauksbee_extract::NetLintReport) -> bool {
    crate::result::lint_findings_json(report)
        .iter()
        .any(crate::result::JsonFinding::gates)
}

/// Strict-mode predicate for the SI report.
///
/// Any real finding (high/medium/low, but not the informational computed-value
/// notes) fails the gate. Asked of the machine findings for the same reason as
/// [`lint_fails`].
pub fn si_fails(report: &hauksbee_extract::SiReport) -> bool {
    crate::result::si_findings_json(report)
        .iter()
        .any(crate::result::JsonFinding::gates)
}

/// How many gate-grade subjects the `--strict` failure line names before it
/// truncates to ", ...". The line is a summary, not the report; the findings
/// themselves were already printed above it.
const STRICT_LINE_SUBJECTS: usize = 8;

/// One `<check> <net/ref>` label per gating lint finding (high/medium, the
/// same predicate as [`lint_fails`]), for the `--strict` failure line.
#[must_use]
pub fn lint_gate_items(report: &hauksbee_extract::NetLintReport) -> Vec<String> {
    crate::result::lint_findings_json(report)
        .iter()
        .filter(|f| f.gates())
        .map(|f| gate_item(&f.kind, &f.nets, &f.refs))
        .collect()
}

/// One label per gating SI finding.
///
/// Every real finding gates, matching [`si_fails`]. The informational
/// computed-value notes are filtered out: they never gate, and naming one on the
/// failure line, or spending a GitHub annotation on it, says a note is why the
/// run failed.
#[must_use]
pub fn si_gate_items(report: &hauksbee_extract::SiReport) -> Vec<String> {
    crate::result::si_findings_json(report)
        .iter()
        .filter(|f| f.gates())
        .map(|f| gate_item(&f.kind, &f.nets, &f.refs))
        .collect()
}

/// One label per short whose final structured severity is serious. Unlike the
/// raw-report helper, this sees pair-specific KiCad-oracle promotion on an
/// otherwise unvalidated board format.
#[must_use]
pub fn drc_structured_gate_items(drc: &crate::result::DrcStructured) -> Vec<String> {
    drc.shorts
        .iter()
        .filter(|short| short.severity == "serious")
        .map(|short| format!("drc-short {}/{}", short.net_a, short.net_b))
        .collect()
}

/// `<check> <first net, else first ref>`; bare check id when neither exists.
fn gate_item(check: &str, nets: &[String], refs: &[String]) -> String {
    nets.first()
        .or_else(|| refs.first())
        .map_or_else(|| check.to_string(), |subject| format!("{check} {subject}"))
}

/// Exit 3 for a bind-blocked run, naming the blockers on the GitHub checks tab.
///
/// The blockers are named on the way out whether or not `--junit`/`--sarif`
/// were asked for. The current artifact transaction already contains the same
/// blockers as gate-grade evidence and is finalized by this exit.
///
/// Narrower than [`strict_gate_exit`], deliberately: it prints no failure line
/// (the surfaces that exit 3 already printed their INCONCLUSIVE verdict) and it
/// is not the chokepoint for typed whole-run refusals: those use
/// `ci_artifacts::exit_with_refusal` so `JUnit`, SARIF and GitHub receive the
/// refusal itself. No-op outside GitHub Actions.
pub fn exit_invalid_for_analysis(blockers: &[String]) -> ! {
    ci_artifacts::github_blocker_annotation(blockers);
    // An invalid verdict with no blocker to name (an undermined run-level
    // evidence map) still fails the job, so the annotation surface says so too.
    // A caller that holds the verdict's own reason annotates it first, and this
    // one is then a no-op.
    ci_artifacts::github_verdict_annotation(
        "the run's verdict is invalid: run-level evidence is undermined, so a clean result \
         would not be trustworthy (see the Evidence section of the report)",
    );
    ci_artifacts::exit_with_findings(crate::result::EXIT_INVALID_FOR_ANALYSIS, &[])
}

/// Name WHY the process is about to exit 2, then exit.
///
/// The mandatory last word of the eight report gate sites that route through
/// here: `--lint`, `--resources`, `--check`, bare `--json`, `--drc`, `--si`,
/// `--usb-c`, and the co-sim fault gate in `commands::run`. Exit 2 with no line
/// saying why reads as a tool crash, and `--plain --strict` must not print a
/// "not a failure" verdict while failing.
///
/// Not every exit 2 in the binary comes through here, so do not read this as the
/// exit-2 chokepoint. Three other paths exit 2 with a message of their own:
/// `--strict-boot` in `commands::run` prints a `BOOT HAZARD` line per held-high
/// net (its subjects are nets, not the `<check> <subject>` items this helper
/// formats), `hauksbee models lint` exits 2 on its own finding count, and
/// `hauksbee sim` uses `EXIT_MALFORMED_DECK`. Two more reach exit 2 without a
/// message of their own: the usage guards in `main`, which are not gates, and
/// `hauksbee reproduce`, which forwards a replayed run's code verbatim. The
/// exit-code contract itself is in docs/ci/CI.md.
///
/// Stream per house style: the line is part of the report on the text/plain
/// surfaces (stdout); under `--json` stdout must stay one JSON document, so it
/// goes to stderr.
pub fn strict_gate_exit(mode: OutputMode, items: &[String]) -> ! {
    // --plain promised prose a non-engineer can read; a failure line full of
    // rule ids ("drc-short", "crystal_load_cap") breaks that promise at the
    // one moment it matters most. Text/JSON keep the exact ids (they are
    // the grep/waiver keys).
    let humanized: Vec<String>;
    let mut shown: Vec<&str> = if mode == OutputMode::Plain {
        humanized = items.iter().map(|i| plain_gate_item(i)).collect();
        humanized
            .iter()
            .take(STRICT_LINE_SUBJECTS)
            .map(String::as_str)
            .collect()
    } else {
        items
            .iter()
            .take(STRICT_LINE_SUBJECTS)
            .map(String::as_str)
            .collect()
    };
    if items.len() > STRICT_LINE_SUBJECTS {
        shown.push("...");
    }
    let line = format!(
        "FAILED under --strict: {} gate-grade finding(s): {}",
        items.len(),
        shown.join(", ")
    );
    match mode {
        OutputMode::Json => eprintln!("{line}"),
        OutputMode::Text | OutputMode::Plain => println!("{line}"),
    }
    // Under GitHub Actions the same gate-grade findings become workflow
    // annotations, so the failing job names them in the PR UI.
    ci_artifacts::exit_with_findings(2, items);
}

/// A gate item ("drc-short GND/+5V", "`crystal_load_cap` XTAL1") in words a
/// non-engineer can read, for the `--plain` strict failure line. The id
/// prefix maps to its check family; underscores become spaces.
fn plain_gate_item(item: &str) -> String {
    let (id, subject) = match item.split_once(' ') {
        Some((id, s)) => (id, Some(s)),
        None => (item, None),
    };
    let words = if id == "drc-short" {
        "copper short between".to_string()
    } else if let Some(rest) = id.strip_prefix("cosim-") {
        format!("co-sim {} on", rest.replace('_', " "))
    } else if id == "usb_c_cc" {
        "USB-C compliance:".to_string()
    } else {
        format!("{} on", id.replace('_', " "))
    };
    match subject {
        Some(s) => format!("{words} {s}"),
        None => words,
    }
}

/// Discoverability for the exit-code contract.
///
/// A report command without `--strict` exits 0 even when it just printed a
/// gate-grade finding, and the proof campaign showed people read that 0 as a
/// clean bill in CI. When the findings WOULD have gated, say so once on stderr
/// (stdout stays a clean report / a single JSON document). Every strict-gated
/// report calls this at its gate site so the hint and the gate can never
/// disagree.
pub fn note_ungated_findings(strict: bool, would_gate: bool) {
    if !strict && would_gate {
        eprintln!(
            "note: gate-grade finding(s) above, but this is a report command so the exit code is 0. \
             Add --strict to exit 2 on them (exit contract: 0 = clean or report-only, 1 = input \
             error such as a missing or unreadable file, 2 = findings under --strict, 3 = invalid \
             for analysis), or gate CI with hauksbee-ci."
        );
    }
}

/// The exit-3 counterpart of [`note_ungated_findings`].
///
/// A run the verdict cannot vouch for (partly judged, or inconclusive over
/// unmodelled key parts) still exits 0 without `--strict`, and a CI log reader
/// must not read that 0 as a pass. Said once on stderr, so stdout stays the
/// report.
pub fn note_unvouched_exit(strict: bool, unvouched: bool) {
    if !strict && unvouched {
        eprintln!(
            "note: exit 0 only because --strict is off: this run is partly judged or \
             inconclusive, not a pass. Add --strict to exit 3 on it (3 = invalid for analysis)."
        );
    }
}

#[cfg(test)]
mod tests {
    use hauksbee_extract::{
        LintCheck, LintFinding, NetLintReport, Severity, SiCheck, SiFinding, SiReport, SiSeverity,
    };

    #[test]
    fn missing_project_defaults_and_present_project_rules_are_distinguished() {
        let dir = tempfile::tempdir().unwrap();
        let board_path = dir.path().join("board.kicad_pcb");
        let board = hauksbee_extract::ExtractedBoard::from_kicad_pcb(include_str!(
            "../../../hauksbee-ci/examples/boards/blinky.kicad_pcb"
        ))
        .unwrap();

        let missing = super::kicad_pro_clearance_rules(&board_path, &board);
        assert!(missing.is_none());
        let missing_source = super::kicad_pro_clearance_rule_provenance(
            &board_path,
            "(kicad_pcb)",
            missing.is_some(),
            0.2,
        );
        assert_eq!(
            missing_source.source,
            crate::result::ClearanceRuleSource::Defaulted
        );

        std::fs::write(
            board_path.with_extension("kicad_pro"),
            r#"{"net_settings":{"classes":[]}}"#,
        )
        .unwrap();
        let unreadable = super::kicad_pro_clearance_rules(&board_path, &board);
        assert!(unreadable.is_none());
        let unreadable_source = super::kicad_pro_clearance_rule_provenance(
            &board_path,
            "(kicad_pcb)",
            unreadable.is_some(),
            0.2,
        );
        assert!(unreadable_source.project_file_found);
        assert!(unreadable_source.cli_notice().contains("Repair or replace"));
        assert!(!unreadable_source
            .cli_notice()
            .contains("Place the matching"));

        std::fs::write(
            board_path.with_extension("kicad_pro"),
            r#"{"net_settings":{"classes":[{"name":"Default","clearance":0.1}]}}"#,
        )
        .unwrap();
        let found = super::kicad_pro_clearance_rules(&board_path, &board);
        assert!(found.is_some());
        let found_source = super::kicad_pro_clearance_rule_provenance(
            &board_path,
            "(kicad_pcb)",
            found.is_some(),
            0.1,
        );
        assert_eq!(
            found_source.source,
            crate::result::ClearanceRuleSource::ProjectFileFound
        );

        std::fs::write(
            board_path.with_extension("kicad_pro"),
            r#"{"net_settings":{"classes":[]}}"#,
        )
        .unwrap();
        assert!(
            super::kicad_pro_clearance_rules(&board_path, &board).is_none(),
            "an empty project must not be promoted above the fallback"
        );
    }

    #[test]
    fn custom_rules_load_degrades_and_override_project_in_both_directions() {
        let dir = tempfile::tempdir().unwrap();
        let board_path = dir.path().join("board.kicad_pcb");
        let board = hauksbee_extract::ExtractedBoard::from_kicad_pcb(include_str!(
            "../../../hauksbee-ci/examples/boards/blinky.kicad_pcb"
        ))
        .unwrap();
        std::fs::write(
            board_path.with_extension("kicad_pro"),
            r#"{"net_settings":{"classes":[{"name":"Default","clearance":0.2}]}}"#,
        )
        .unwrap();
        std::fs::write(
            board_path.with_extension("kicad_dru"),
            "(version 1) (rule \"broken\"",
        )
        .unwrap();
        let clearance_bits = |input: &super::KicadClearanceInput| {
            let rules = input.rules.as_ref().unwrap();
            rules.effective_clearance("A", "B").to_bits()
        };
        // A file that cannot be read as a whole no longer stops the run: the
        // project's netclass rule stays in force and the report says the
        // custom rules were not checked.
        let unreadable = super::KicadClearanceInput::load(&board_path, &board)
            .expect("a custom rules file the reader cannot parse must not fail the import");
        assert_eq!(clearance_bits(&unreadable), 0.2_f64.to_bits());
        let notice = unreadable
            .custom_coverage()
            .and_then(crate::result::CustomRulesCoverage::unevaluated_notice)
            .expect("the unreadable file is disclosed");
        assert!(notice.contains("board.kicad_dru"), "{notice}");
        assert!(
            notice.contains("none of its rules were checked"),
            "{notice}"
        );

        std::fs::write(
            board_path.with_extension("kicad_dru"),
            "(version 1) (rule \"relax\" (constraint clearance (min 0.127mm)))",
        )
        .unwrap();
        let relaxed = super::KicadClearanceInput::load(&board_path, &board).unwrap();
        assert_eq!(clearance_bits(&relaxed), 0.127_f64.to_bits());
        assert_eq!(
            relaxed.provenance(&board_path, "(kicad_pcb)", 0.127).source,
            crate::result::ClearanceRuleSource::CustomRulesFile
        );

        std::fs::write(
            board_path.with_extension("kicad_dru"),
            "(version 1) (rule \"tighten\" (constraint clearance (min 0.25mm)))",
        )
        .unwrap();
        let tightened = super::KicadClearanceInput::load(&board_path, &board).unwrap();
        assert_eq!(clearance_bits(&tightened), 0.25_f64.to_bits());

        std::fs::write(
            board_path.with_extension("kicad_dru"),
            "(version 1)\n(rule \"bare\" (constraint clearance (min 0.3)))\n(rule \"explicit\" (constraint clearance (min 0.25mm)))",
        )
        .unwrap();
        let inactive = super::KicadClearanceInput::load(&board_path, &board).unwrap();
        assert_eq!(clearance_bits(&inactive), 0.2_f64.to_bits());
        let provenance = inactive.provenance(&board_path, "(kicad_pcb)", 0.2);
        assert_eq!(
            provenance.source,
            crate::result::ClearanceRuleSource::ProjectFileFound
        );
        assert!(
            provenance
                .custom_rules
                .as_ref()
                .unwrap()
                .file_inactive_due_to_bare_values
        );
    }

    /// The gate predicates and the CI artifacts read one rule. These pin the two
    /// families whose gate is wider than the `serious` severity, from both
    /// sides: the gate's answer, the subjects it names, and the `gating` flags
    /// the artifacts grade on, on the same report.
    #[test]
    fn the_lint_gate_is_exactly_the_gating_findings_it_names() {
        let mut report = NetLintReport {
            findings: vec![
                LintFinding {
                    check: LintCheck::PlaceholderValue,
                    severity: Severity::Low,
                    message: "a low one".into(),
                    refs: vec!["R9".into()],
                    nets: Vec::new(),
                },
                LintFinding {
                    check: LintCheck::PlaceholderValue,
                    severity: Severity::Medium,
                    message: "a medium one".into(),
                    refs: vec!["R1".into()],
                    nets: Vec::new(),
                },
            ],
        };
        // Medium gates and low does not, so the gate fires and names one item.
        assert!(super::lint_fails(&report));
        assert_eq!(super::lint_gate_items(&report), ["placeholder_value R1"]);
        // The findings the artifacts grade carry the same split.
        let findings = crate::result::lint_findings_json(&report);
        assert_eq!(
            findings
                .iter()
                .map(crate::result::JsonFinding::gates)
                .collect::<Vec<_>>(),
            [false, true]
        );
        // Low only: nothing gates and nothing is named.
        report.findings.pop();
        assert!(!super::lint_fails(&report));
        assert!(super::lint_gate_items(&report).is_empty());
    }

    #[test]
    fn an_si_info_note_is_never_named_as_the_reason_a_run_failed() {
        let mut report = SiReport {
            findings: vec![
                SiFinding {
                    check: SiCheck::ControlledImpedance,
                    severity: SiSeverity::Info,
                    message: "a computed value".into(),
                    refs: Vec::new(),
                    nets: vec!["USB_DP".into()],
                },
                SiFinding {
                    check: SiCheck::CrystalLoadCap,
                    severity: SiSeverity::Medium,
                    message: "load caps look wrong".into(),
                    refs: Vec::new(),
                    nets: vec!["XTAL1".into()],
                },
            ],
        };
        // The info note is on the report and in the JSON, but it is not why the
        // run failed, so it is neither gating nor named on the failure line (the
        // same list the GitHub annotations are printed from).
        assert!(super::si_fails(&report));
        assert_eq!(super::si_gate_items(&report), ["crystal_load_cap XTAL1"]);
        let findings = crate::result::si_findings_json(&report);
        assert_eq!(findings.len(), 2);
        assert_eq!(
            findings
                .iter()
                .map(crate::result::JsonFinding::gates)
                .collect::<Vec<_>>(),
            [false, true]
        );
        // Info only: no gate, no items, and the note is still reported.
        report.findings.remove(1);
        assert!(!super::si_fails(&report));
        assert!(super::si_gate_items(&report).is_empty());
        assert_eq!(crate::result::si_findings_json(&report).len(), 1);
    }

    const CROSSING_BOARD: &str = r#"(kicad_pcb (version 20221018) (generator pcbnew)
  (layers (0 "F.Cu" signal) (31 "B.Cu" signal))
  (net 0 "") (net 1 "A") (net 2 "B")
  (segment (start 0 0) (end 10 0) (width 0.5) (layer "F.Cu") (net 1) (uuid "aaaaaaaa-0000-4000-8000-000000000001"))
  (segment (start 5 -5) (end 5 5) (width 0.5) (layer "F.Cu") (net 2) (uuid "bbbbbbbb-0000-4000-8000-000000000002"))
)
"#;

    #[test]
    fn valid_rules_without_a_check_load_and_are_listed_as_recognised_not_checked() {
        let dir = tempfile::tempdir().unwrap();
        let board_path = dir.path().join("board.kicad_pcb");
        let board = hauksbee_extract::ExtractedBoard::from_kicad_pcb(CROSSING_BOARD).unwrap();
        std::fs::write(
            board_path.with_extension("kicad_dru"),
            "(version 1)\n(rule \"holes\" (condition \"A.Net != B.Net\") (constraint hole_to_hole (min 0.5mm)))",
        )
        .unwrap();
        let input = super::KicadClearanceInput::load(&board_path, &board).unwrap();
        let notice = input
            .custom_coverage()
            .unwrap()
            .unevaluated_notice()
            .unwrap();
        assert!(notice.contains("recognised, not checked"), "{notice}");
        assert!(notice.contains("holes"), "{notice}");

        std::fs::write(
            board_path.with_extension("kicad_dru"),
            "(version 1)\n(rule \"ok\" (constraint clearance (min 0.1mm)))\n(rule \"bad\" (constraint copper_magic (min 1mm)))",
        )
        .unwrap();
        // A rule the reader cannot understand degrades to a note naming the rule
        // and line; the readable rule before it still applies.
        let input = super::KicadClearanceInput::load(&board_path, &board)
            .expect("an unsupported rule must not fail the import");
        let rules = input.rules.as_ref().expect("the readable rule applies");
        assert!((rules.effective_clearance("A", "B") - 0.1).abs() < 1e-12);
        let notice = input
            .custom_coverage()
            .and_then(crate::result::CustomRulesCoverage::unevaluated_notice)
            .expect("the unreadable rule is disclosed");
        assert!(notice.contains("\"bad\""), "{notice}");
        assert!(notice.contains("line 3"), "{notice}");
        assert!(
            notice.contains("custom DRC rule not understood; that rule was not checked"),
            "{notice}"
        );
        assert!(notice.contains("copper_magic"), "{notice}");
        assert!(input
            .custom_coverage()
            .unwrap()
            .qualifies_clearance_findings());
    }

    #[test]
    fn project_drc_exclusion_is_waived_but_listed_and_does_not_fail() {
        let dir = tempfile::tempdir().unwrap();
        let board_path = dir.path().join("board.kicad_pcb");
        let board = hauksbee_extract::ExtractedBoard::from_kicad_pcb(CROSSING_BOARD).unwrap();
        let run = |path: &std::path::Path| {
            super::run_drc(
                path,
                &board,
                CROSSING_BOARD,
                CROSSING_BOARD.as_bytes(),
                false,
            )
            .unwrap()
            .1
        };
        assert_eq!(run(&board_path).short_count(), 1);

        std::fs::write(
            board_path.with_extension("kicad_pro"),
            r#"{"board":{"design_settings":{"drc_exclusions":[
              "shorting_items|5000000|0|aaaaaaaa-0000-4000-8000-000000000001|bbbbbbbb-0000-4000-8000-000000000002"]}}}"#,
        )
        .unwrap();
        let mut report = run(&board_path);
        assert!(report.is_clean());
        let mut waivers = crate::waiver::WaiverSet::default();
        let waived = super::check::waive_drc(&mut waivers, &mut report);
        assert_eq!(waived.len(), 1);
        assert!(waived[0].reason.contains("design's own DRC exclusions"));
    }
}
