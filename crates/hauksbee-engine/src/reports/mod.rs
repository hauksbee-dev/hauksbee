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
pub fn lint_fails(report: &hauksbee_extract::NetLintReport) -> bool {
    use hauksbee_extract::Severity;
    report
        .findings
        .iter()
        .any(|f| matches!(f.severity, Severity::High | Severity::Medium))
}

/// Strict-mode predicate for the SI report: any real finding (high/medium/low,
/// but not the informational computed-value notes) fails the gate.
pub fn si_fails(report: &hauksbee_extract::SiReport) -> bool {
    report.finding_count() > 0
}

/// How many gate-grade subjects the `--strict` failure line names before it
/// truncates to ", ...". The line is a summary, not the report; the findings
/// themselves were already printed above it.
const STRICT_LINE_SUBJECTS: usize = 8;

/// One `<check> <net/ref>` label per gating lint finding (high/medium, the
/// same predicate as [`lint_fails`]), for the `--strict` failure line.
pub fn lint_gate_items(report: &hauksbee_extract::NetLintReport) -> Vec<String> {
    use hauksbee_extract::Severity;
    report
        .findings
        .iter()
        .filter(|f| matches!(f.severity, Severity::High | Severity::Medium))
        .map(|f| gate_item(f.check.as_str(), &f.nets, &f.refs))
        .collect()
}

/// One label per gating SI finding (every real finding gates, matching
/// [`si_fails`]).
pub fn si_gate_items(report: &hauksbee_extract::SiReport) -> Vec<String> {
    report
        .findings
        .iter()
        .map(|f| gate_item(f.check.as_str(), &f.nets, &f.refs))
        .collect()
}

/// One label per true copper short (clearance-only findings do not gate).
pub fn drc_gate_items(report: &hauksbee_extract::DrcReport) -> Vec<String> {
    report
        .findings
        .iter()
        .filter(|f| matches!(f.kind, hauksbee_extract::ViolationKind::Short))
        .map(|f| format!("drc-short {}/{}", f.net_a_name, f.net_b_name))
        .collect()
}

/// `<check> <first net, else first ref>`; bare check id when neither exists.
fn gate_item(check: &str, nets: &[String], refs: &[String]) -> String {
    match nets.first().or_else(|| refs.first()) {
        Some(subject) => format!("{check} {subject}"),
        None => check.to_string(),
    }
}

/// The exit-3 twin of [`strict_gate_exit`]: a run that could not be judged
/// annotates the GitHub checks tab with the reason before exiting, whether or
/// not `--junit`/`--sarif` were asked for. Without this the annotation surface
/// was the only silent one on a refused run, because the blocker annotation
/// rode inside the artifact-writing branch; a reviewer then saw a clean checks
/// tab beside an exit 3. No-op outside GitHub Actions.
pub fn exit_invalid_for_analysis(blockers: &[String]) -> ! {
    ci_artifacts::github_blocker_annotation(blockers);
    std::process::exit(crate::result::EXIT_INVALID_FOR_ANALYSIS)
}

/// The mandatory last word of every `--strict` gate: name WHY the process is
/// about to exit 2, then exit. Exit 2 with no line saying why reads as a tool
/// crash, and `--plain --strict` used to print a "not a failure" verdict while
/// failing. Stream per house style: the line is part of the report on the
/// text/plain surfaces (stdout); under `--json` stdout must stay one JSON
/// document, so it goes to stderr.
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
    ci_artifacts::github_annotations(items);
    std::process::exit(2);
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
