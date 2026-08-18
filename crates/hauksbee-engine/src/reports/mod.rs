//! One module per report family. Each builds the **structured** finding type (the
//! honesty layer in [`crate::result`]) and renders it in exactly one of the three
//! output surfaces. The per-flag `if json / else if plain / else` triplication
//! collapses into a single `match` on [`OutputMode`] per report, which keeps
//! `cmd_run` a thin dispatcher: pick the mode once, call the right report's
//! `emit`.
//!
//! Rendering itself lives elsewhere: each `emit` delegates to the shared
//! renderers (`DrcStructured::render`, `plain_*`, `JsonReport`, the extract-crate
//! text renderers), so every surface emits byte-identical output for the same
//! report.

pub mod ac;
pub mod ampacity;
pub mod bind;
pub mod check;
pub mod ci_artifacts;
pub mod cosim;
pub mod coverage;
pub mod drc;
pub mod lint;
pub mod si;
pub mod thermal;
pub mod usb_c;

use std::path::Path;

use hauksbee_extract::ExtractedBoard;

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
    /// (a machine consumer never wants prose), matching the historical precedence.
    pub fn from_flags(json: bool, plain: bool) -> Self {
        if json {
            OutputMode::Json
        } else if plain {
            OutputMode::Plain
        } else {
            OutputMode::Text
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

/// The one prominent note printed by `--check`/`--drc` when a KiCad layout
/// carries no routed copper at all (D2): the spacing check then had only pads
/// to compare, and a clean result must not read as "the routing is clean" on
/// a board that has no routing yet.
pub(crate) const UNROUTED_COPPER_NOTE: &str =
    "note: this board has no routed copper (no track segments): the spacing check \
     had only pads to compare, so a clean result here says nothing about routing \
     that does not exist yet.";

/// True when a KiCad layout text carries no routed copper: no track segments
/// and no zones (a filled zone is copper too, so its presence disables the
/// caveat rather than risk crying wolf). Only meaningful for `.kicad_pcb`
/// text; other formats return false and print nothing.
pub(crate) fn unrouted_kicad_layout(text: &str) -> bool {
    text.contains("(kicad_pcb") && !text.contains("(segment") && !text.contains("(zone")
}

/// Read project-file netclass clearances and resolve them to this board's
/// concrete net names. KiCad 10 stores this in the sibling `.kicad_pro` rather
/// than the `.kicad_pcb`; missing/malformed project files simply leave DRC on
/// the board/default rules. Shared by the `--drc`, `--check` and combined-`--json`
/// reports.
pub fn kicad_pro_clearance_rules(
    board_path: &Path,
    board: &ExtractedBoard,
) -> Option<hauksbee_extract::ClearanceRules> {
    let text = std::fs::read_to_string(board_path.with_extension("kicad_pro")).ok()?;
    hauksbee_extract::clearance_rules_from_kicad_pro(
        &text,
        board.nets.iter().map(|n| n.name.as_str()),
    )
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
        .any(|finding| finding.gates())
}

/// Strict-mode predicate for the SI report: any real finding (high/medium/low,
/// but not the informational computed-value notes) fails the gate. Asked of the
/// machine findings for the same reason as [`lint_fails`].
pub fn si_fails(report: &hauksbee_extract::SiReport) -> bool {
    crate::result::si_findings_json(report)
        .iter()
        .any(|finding| finding.gates())
}

/// How many gate-grade subjects the `--strict` failure line names before it
/// truncates to ", ...". The line is a summary, not the report; the findings
/// themselves were already printed above it.
const STRICT_LINE_SUBJECTS: usize = 8;

/// One `<check> <net/ref>` label per gating lint finding (high/medium, the
/// same predicate as [`lint_fails`]), for the `--strict` failure line.
pub fn lint_gate_items(report: &hauksbee_extract::NetLintReport) -> Vec<String> {
    crate::result::lint_findings_json(report)
        .iter()
        .filter(|f| f.gates())
        .map(|f| gate_item(&f.kind, &f.nets, &f.refs))
        .collect()
}

/// One label per gating SI finding (every real finding gates, matching
/// [`si_fails`]). The informational computed-value notes are filtered out: they
/// never gate, and naming one on the failure line, or spending a GitHub
/// annotation on it, says a note is why the run failed.
pub fn si_gate_items(report: &hauksbee_extract::SiReport) -> Vec<String> {
    crate::result::si_findings_json(report)
        .iter()
        .filter(|f| f.gates())
        .map(|f| gate_item(&f.kind, &f.nets, &f.refs))
        .collect()
}

/// One label per GATING copper short: clearance-only findings do not gate, and
/// neither does a contact backed by board-local physical authority. Schematic
/// net names alone do not locate an intended join and therefore stay gating.
pub fn drc_gate_items(report: &hauksbee_extract::DrcReport) -> Vec<String> {
    drc_gate_items_with_ties(report, None)
}

pub fn drc_gate_items_with_ties(
    report: &hauksbee_extract::DrcReport,
    qualification: Option<&hauksbee_extract::DrcTieQualification>,
) -> Vec<String> {
    report
        .shorts()
        .filter(|finding| qualification.is_none_or(|ties| ties.tie_for(finding).is_none()))
        .map(|f| format!("drc-short {}/{}", f.net_a_name, f.net_b_name))
        .collect()
}

/// One label per short whose final structured severity is serious. Unlike the
/// raw-report helper, this sees pair-specific KiCad-oracle promotion on an
/// otherwise unvalidated board format.
pub fn drc_structured_gate_items(drc: &crate::result::DrcStructured) -> Vec<String> {
    drc.shorts
        .iter()
        .filter(|short| short.severity == "serious")
        .map(|short| format!("drc-short {}/{}", short.net_a, short.net_b))
        .collect()
}

/// `<check> <first net, else first ref>`; bare check id when neither exists.
fn gate_item(check: &str, nets: &[String], refs: &[String]) -> String {
    match nets.first().or_else(|| refs.first()) {
        Some(subject) => format!("{check} {subject}"),
        None => check.to_string(),
    }
}

/// Exit 3 for a bind-blocked run, naming the blockers on the GitHub checks tab
/// on the way out whether or not `--junit`/`--sarif` were asked for. The current
/// artifact transaction already contains the same blockers as gate-grade
/// evidence and is finalized by this exit.
///
/// Narrower than [`strict_gate_exit`], deliberately: it prints no failure line
/// (the surfaces that exit 3 already printed their INCONCLUSIVE verdict) and it
/// is not the chokepoint for typed whole-run refusals: those use
/// `ci_artifacts::exit_with_refusal` so JUnit, SARIF and GitHub receive the
/// refusal itself. No-op outside GitHub Actions.
pub fn exit_invalid_for_analysis(blockers: &[String]) -> ! {
    ci_artifacts::github_blocker_annotation(blockers);
    ci_artifacts::exit_with_findings(crate::result::EXIT_INVALID_FOR_ANALYSIS, &[])
}

/// The mandatory last word of the eight report gate sites that route through
/// here: `--lint`, `--resources`, `--check`, bare `--json`, `--drc`, `--si`,
/// `--usb-c`, and the co-sim fault gate in `commands::run`. Name WHY the process
/// is about to exit 2, then exit. Exit 2 with no line saying why reads as a tool
/// crash, and `--plain --strict` used to print a "not a failure" verdict while
/// failing.
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
    // one moment it matters most (L4). Text/JSON keep the exact ids (they are
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

/// A gate item ("drc-short GND/+5V", "crystal_load_cap XTAL1") in words a
/// non-engineer can read, for the `--plain` strict failure line (L4). The id
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

/// Discoverability for the exit-code contract: a report command without
/// `--strict` exits 0 even when it just printed a gate-grade finding, and the
/// proof campaign showed people read that 0 as a clean bill in CI. When the
/// findings WOULD have gated, say so once on stderr (stdout stays a clean
/// report / a single JSON document). Every strict-gated report calls this at
/// its gate site so the hint and the gate can never disagree.
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

#[cfg(test)]
mod tests {
    use hauksbee_extract::{
        LintCheck, LintFinding, NetLintReport, Severity, SiCheck, SiFinding, SiReport, SiSeverity,
    };

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
                .map(|finding| finding.gates())
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
                .map(|finding| finding.gates())
                .collect::<Vec<_>>(),
            [false, true]
        );
        // Info only: no gate, no items, and the note is still reported.
        report.findings.remove(1);
        assert!(!super::si_fails(&report));
        assert!(super::si_gate_items(&report).is_empty());
        assert_eq!(crate::result::si_findings_json(&report).len(), 1);
    }
}
