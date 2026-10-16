//! CI-native artifacts for `hauksbee run`: `JUnit` XML and SARIF 2.1.0.
//!
//! `--junit <path>` (`JUnit` XML) and `--sarif <path>` (SARIF 2.1.0) are both
//! computed from the SAME selected run surface, so a pipeline consumes findings
//! and terminal refusals as test results / code-scanning alerts without parsing
//! the human report.
//! GitHub annotations for gate-grade findings ride the `--strict` gate
//! (see [`super::strict_gate_exit`]'s caller in `reports::mod`).

use hauksbee_ir::eprintln;
use std::path::Path;

use crate::result::{JsonFinding, Refusal};

/// Whether two paths resolve to the same underlying file.
///
/// Canonical paths catch spelling and symlink aliases. Existing hard links
/// need file identity as well: writing either name changes the same file.
/// Platforms without a stable file-id API fail closed when both paths already
/// exist, because artifact validation must never guess that an input is safe to
/// overwrite.
#[must_use]
pub fn paths_alias(left: &Path, right: &Path) -> bool {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let canonicalish = |path: &Path| {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        };
        std::fs::canonicalize(&absolute).unwrap_or_else(|_| {
            absolute
                .parent()
                .and_then(|parent| std::fs::canonicalize(parent).ok())
                .and_then(|parent| absolute.file_name().map(|name| parent.join(name)))
                .unwrap_or(absolute)
        })
    };
    if canonicalish(left) == canonicalish(right) {
        return true;
    }

    if !left.exists() || !right.exists() {
        return false;
    }

    // If the platform cannot prove that these two existing paths are
    // different, artifact safety fails closed rather than risking an input.
    same_file::is_same_file(left, right).unwrap_or(true)
}

#[derive(Clone)]
struct CurrentRunArtifacts {
    board_name: String,
    board_path: std::path::PathBuf,
    junit: Option<std::path::PathBuf>,
    sarif: Option<std::path::PathBuf>,
    findings: Vec<JsonFinding>,
    refusal: Option<Refusal>,
    selected_checks: Vec<String>,
    /// The run's own machine verdict, once the surface has computed it. The
    /// writers read it so an artifact is never green beside a verdict that is
    /// not a pass (see [`artifact_findings`]).
    verdict: Option<NotedVerdict>,
}

#[derive(Clone)]
struct NotedVerdict {
    verdict: String,
    partial: bool,
    reason: Option<String>,
}

thread_local! {
    static CURRENT_RUN: std::cell::RefCell<Option<CurrentRunArtifacts>> = const {
        std::cell::RefCell::new(None)
    };
}

#[derive(Clone, Copy)]
enum TerminalOutcomeKind {
    /// The run read the board and judged some families; the artifacts keep the
    /// families it judged clean.
    AnalysisRefusal,
    /// The whole run refused its claim ([`exit_with_refusal`]): no family has a
    /// verdict, so none gets a passing "no findings" case. SARIF reports it as
    /// the same `invalid-for-analysis` result as [`Self::AnalysisRefusal`].
    WholeRunRefusal,
    RunError,
}

fn write_current(
    state: &CurrentRunArtifacts,
    refusal: Option<&Refusal>,
    exit_code: i32,
    outcome_kind: TerminalOutcomeKind,
) -> anyhow::Result<()> {
    let refusal = refusal.or(state.refusal.as_ref());
    let findings = artifact_findings(&state.findings, refusal, state.verdict.as_ref());
    // A run that never read the board (a run error) or refused its whole claim
    // has judged no family, so it gets no passing "no findings" case for any of
    // them.
    let selected_checks: &[String] = if matches!(
        outcome_kind,
        TerminalOutcomeKind::RunError | TerminalOutcomeKind::WholeRunRefusal
    ) {
        &[]
    } else {
        &state.selected_checks
    };
    let mut errors = Vec::new();
    if let Some(path) = &state.junit {
        if let Err(error) = std::fs::write(
            path,
            junit_xml_for_checks(&state.board_name, &findings, refusal, selected_checks),
        ) {
            errors.push(format!("writing --junit '{}': {error}", path.display()));
        }
    }
    if let Some(path) = &state.sarif {
        if let Err(error) = std::fs::write(
            path,
            sarif_json_with_terminal_outcome(
                &state.board_path,
                &findings,
                refusal,
                exit_code,
                outcome_kind,
            ),
        ) {
            errors.push(format!("writing --sarif '{}': {error}", path.display()));
        }
    }
    if !errors.is_empty() {
        anyhow::bail!(errors.join("; "));
    }
    Ok(())
}

/// Start one artifact transaction.
///
/// The fail-closed placeholder is written before board/firmware parsing, so an
/// early return, panic, signal or legacy exit can never leave a previous run's
/// green file at the requested path.
///
/// # Errors
///
/// Returns an error if the fail-closed placeholder cannot be written to a
/// requested `--junit` or `--sarif` path.
pub fn begin_run(
    board_path: &Path,
    junit: Option<&Path>,
    sarif: Option<&Path>,
    selected_checks: Vec<String>,
) -> anyhow::Result<()> {
    if junit.is_none() && sarif.is_none() {
        return Ok(());
    }
    let state = CurrentRunArtifacts {
        board_name: board_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("board")
            .to_string(),
        board_path: board_path.to_path_buf(),
        junit: junit.map(Path::to_path_buf),
        sarif: sarif.map(Path::to_path_buf),
        findings: Vec::new(),
        refusal: None,
        selected_checks,
        verdict: None,
    };
    let pending = Refusal::new(
        "a completed Hauksbee run",
        "the run did not reach its final outcome",
        Vec::<String>::new(),
        "inspect the process error, fix it, and rerun the same command",
    );
    write_current(
        &state,
        Some(&pending),
        crate::result::EXIT_INVALID_FOR_ANALYSIS,
        TerminalOutcomeKind::RunError,
    )?;
    CURRENT_RUN.with(|slot| *slot.borrow_mut() = Some(state));
    Ok(())
}

thread_local! {
    /// The findings and verdict a run-manifest outcome records, kept apart from
    /// the artifact transaction so `--emit-manifest` without `--junit`/`--sarif`
    /// records the same gating set the artifacts would carry.
    static TRACKED: std::cell::RefCell<(Vec<JsonFinding>, Option<NotedVerdict>)> = const {
        std::cell::RefCell::new((Vec::new(), None))
    };
}

/// Record the gating finding ids the manifest carries: the findings, plus the
/// one gate-grade entry [`artifact_findings`] adds when the noted verdict is not
/// a pass, so the manifest's gating set is the artifacts' gating set.
fn note_tracked_gating() {
    if !crate::run_manifest::tracking_outcome() {
        return;
    }
    let refusal = CURRENT_RUN.with(|slot| slot.borrow().as_ref().and_then(|s| s.refusal.clone()));
    let all = TRACKED.with(|slot| {
        let tracked = slot.borrow();
        artifact_findings(&tracked.0, refusal.as_ref(), tracked.1.as_ref())
    });
    crate::run_manifest::note_gating_findings(
        "",
        all.iter()
            .filter(|finding| finding.gates())
            .map(JsonFinding::group_key),
    );
}

/// Replace the selected run's current finding set. Nothing is committed to the
/// requested paths until the one terminal finalizer below runs.
pub fn set_current_findings(findings: Vec<JsonFinding>) {
    if crate::run_manifest::tracking_outcome() {
        TRACKED.with(|slot| slot.borrow_mut().0.clone_from(&findings));
        note_tracked_gating();
    }
    CURRENT_RUN.with(|slot| {
        if let Some(state) = slot.borrow_mut().as_mut() {
            state.findings = findings;
        }
    });
}

/// Replace the synthetic pre-resolution name for an embedded example with the real path.
///
/// The transaction must start before example lookup, but successful artifacts
/// should still identify the materialized board path.
pub fn set_current_board_path(board_path: &Path) {
    CURRENT_RUN.with(|slot| {
        if let Some(state) = slot.borrow_mut().as_mut() {
            state.board_name = board_path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("board")
                .to_string();
            state.board_path = board_path.to_path_buf();
        }
    });
}

/// Record the run's machine verdict (`pass` | `fail` | `invalid`).
///
/// It is the one derivation `--json` prints, with its reason sentence and
/// whether it is a partly judged answer. [`crate::result::JsonReport`] is the
/// only caller (from `run_verdict` and `to_json`), so every surface that builds
/// its machine document tells the artifacts the same verdict. The artifact
/// writers then guarantee that `JUnit` and SARIF are red whenever it is not a
/// pass, even when the findings they were handed are all non-gating (an
/// abstained analysis, a DC operating point that did not solve).
///
/// A no-op when no artifact run or manifest outcome is active. A non-pass
/// verdict is never replaced by a later pass: a run is only as green as its
/// worst machine document.
pub fn note_verdict(verdict: &str, partial: bool, reason: Option<&str>) {
    let noted = NotedVerdict {
        verdict: verdict.to_string(),
        partial,
        reason: reason.map(str::to_string),
    };
    let is_pass = noted.verdict == "pass";
    let keep = move |current: &Option<NotedVerdict>| {
        is_pass && current.as_ref().is_some_and(|c| c.verdict != "pass")
    };
    CURRENT_RUN.with(|slot| {
        if let Some(state) = slot.borrow_mut().as_mut() {
            if !keep(&state.verdict) {
                state.verdict = Some(noted.clone());
            }
        }
    });
    if crate::run_manifest::tracking_outcome() {
        TRACKED.with(|slot| {
            let mut tracked = slot.borrow_mut();
            if !keep(&tracked.1) {
                tracked.1 = Some(noted);
            }
        });
        note_tracked_gating();
    }
}

/// The findings the writers render: the run's own, plus one gate-grade entry
/// when the run's verdict is not a pass and nothing else in the document is red
/// (no failing finding, no refusal). The invariant: `JUnit` and SARIF are never
/// green beside a `--json` verdict that is not `pass`.
fn artifact_findings(
    findings: &[JsonFinding],
    refusal: Option<&Refusal>,
    verdict: Option<&NotedVerdict>,
) -> Vec<JsonFinding> {
    let mut out = findings.to_vec();
    let Some(v) = verdict.filter(|v| v.verdict != "pass") else {
        return out;
    };
    if refusal.is_some() || findings.iter().any(is_failure) {
        return out;
    }
    let reason = v.reason.clone().unwrap_or_else(|| {
        format!(
            "the run's verdict is `{}`, which is not a pass; see the --json verdict_reason",
            v.verdict
        )
    });
    // The one place the not-a-pass entry is worded: the verdict's own reason
    // sentence, labelled in the evidence vocabulary when the run could not be
    // judged at all (a partly judged answer already leads with its own label).
    let (kind, message) = match (v.verdict.as_str(), v.partial) {
        ("invalid", true) => ("partly_judged", reason),
        ("invalid", false) => ("undermined", format!("INVALID evidence: {reason}")),
        _ => ("verdict_fail", reason),
    };
    out.push(JsonFinding {
        check: "evidence".into(),
        kind: kind.into(),
        severity: "serious".into(),
        nets: Vec::new(),
        location_mm: None,
        layer: None,
        refs: Vec::new(),
        actionable: true,
        plain: message.clone(),
        message,
        fix: Some("resolve what the verdict reason names, then re-run".into()),
    });
    out
}

pub fn set_current_refusal(refusal: Refusal) {
    CURRENT_RUN.with(|slot| {
        if let Some(state) = slot.borrow_mut().as_mut() {
            state.refusal = Some(refusal);
        }
    });
}

fn take_current() -> Option<CurrentRunArtifacts> {
    CURRENT_RUN.with(|slot| slot.borrow_mut().take())
}

/// Commit the current run's artifacts as a successful outcome.
///
/// # Errors
///
/// Returns an error if a requested `--junit` or `--sarif` file cannot be
/// written; a fail-closed refusal is then written in its place where possible.
pub fn finish_success() -> anyhow::Result<()> {
    if let Some(state) = take_current() {
        if let Err(error) = write_current(&state, None, 0, TerminalOutcomeKind::AnalysisRefusal) {
            let refusal = Refusal::new(
                "a complete set of requested CI artifacts",
                error.to_string(),
                Vec::<String>::new(),
                "fix the artifact output path or permissions, then rerun",
            );
            let _ = write_current(&state, Some(&refusal), 1, TerminalOutcomeKind::RunError);
            return Err(error);
        }
    }
    Ok(())
}

pub fn finish_error(error: &anyhow::Error, exit_code: i32) {
    let Some(state) = take_current() else {
        return;
    };
    let refusal = Refusal::new(
        "a completed Hauksbee run",
        error.to_string(),
        Vec::<String>::new(),
        "fix the reported input or invocation error, then rerun the same command",
    );
    if let Err(write_error) = write_current(
        &state,
        Some(&refusal),
        exit_code,
        TerminalOutcomeKind::RunError,
    ) {
        eprintln!("error: could not finalize requested CI artifact: {write_error}");
    }
    github_refusal_annotation(&refusal);
}

pub fn exit_with_refusal(code: i32, refusal: &Refusal) -> ! {
    if let Some(state) = take_current() {
        if let Err(error) = write_current(
            &state,
            Some(refusal),
            code,
            TerminalOutcomeKind::WholeRunRefusal,
        ) {
            eprintln!("error: could not finalize requested CI artifact: {error}");
        }
    }
    github_refusal_annotation(refusal);
    crate::run_manifest::finish_outcome("hauksbee", code);
    std::process::exit(code)
}

pub fn exit_with_findings(code: i32, annotation_items: &[String]) -> ! {
    let mut retained_refusal = None;
    if let Some(state) = take_current() {
        retained_refusal.clone_from(&state.refusal);
        if let Err(error) = write_current(&state, None, code, TerminalOutcomeKind::AnalysisRefusal)
        {
            eprintln!("error: could not finalize requested CI artifact: {error}");
        }
    }
    if let Some(refusal) = retained_refusal.as_ref() {
        github_refusal_annotation(refusal);
    }
    github_annotations(annotation_items);
    crate::run_manifest::finish_outcome("hauksbee", code);
    std::process::exit(code)
}

/// Convert non-clean evidence statuses into the finding vocabulary CI writers consume.
///
/// Undermined RUN-LEVEL evidence is gate-grade; qualified evidence remains
/// visible without becoming green-by-omission.
#[must_use]
pub fn evidence_findings(maps: &[hauksbee_ir::evidence::EvidenceMap]) -> Vec<JsonFinding> {
    evidence_findings_with_gate(maps, |_| true)
}

/// As [`evidence_findings`], with the same run-level split the JSON verdict makes.
///
/// An undermined map whose assertion `gates` says is NOT run-level (it backs an
/// individual finding) is demoted to a warning badge instead of a gate-grade
/// failure, so JUnit/SARIF can never fail a run whose JSON verdict says pass, or
/// vice versa.
pub fn evidence_findings_with_gate(
    maps: &[hauksbee_ir::evidence::EvidenceMap],
    gates: impl Fn(&hauksbee_ir::evidence::EvidenceMap) -> bool,
) -> Vec<JsonFinding> {
    use hauksbee_ir::evidence::EvidenceStatus;
    maps.iter()
        .filter_map(|map| {
            let (severity, prefix) = match map.status() {
                EvidenceStatus::Clean => return None,
                EvidenceStatus::Qualified => ("warning", "QUALIFIED evidence"),
                EvidenceStatus::Undermined if gates(map) => ("serious", "INVALID evidence"),
                EvidenceStatus::Undermined => ("warning", "UNDERMINED evidence"),
            };
            Some(JsonFinding {
                check: "evidence".into(),
                kind: map.status().as_str().into(),
                severity: severity.into(),
                nets: Vec::new(),
                location_mm: None,
                layer: None,
                refs: Vec::new(),
                actionable: true,
                message: format!("{prefix}: {}", map.assertion()),
                plain: format!("{prefix}: {}", map.assertion()),
                fix: Some("resolve the cited assumptions, then re-run the assertion".into()),
            })
        })
        .collect()
}

/// Whether this process has already announced its non-pass verdict on GitHub.
///
/// One annotation per process: several layers can converge on the same exit
/// (the verdict, then [`super::exit_invalid_for_analysis`]), and the job should
/// name the cause once.
static VERDICT_ANNOUNCED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn announce_verdict_once(title: &str, text: &str) {
    if std::env::var_os("GITHUB_ACTIONS").is_none() {
        return;
    }
    if VERDICT_ANNOUNCED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    eprintln!(
        "::error title={title}::{}",
        text.replace('%', "%25")
            .replace('\r', "%0D")
            .replace('\n', "%0A")
    );
}

/// GitHub error annotation for unbound verdict-critical parts.
///
/// The annotation surface agrees with the gate-grade JUnit/SARIF entry the same
/// blockers produce. No-op outside GitHub Actions, and for no blockers (see
/// [`github_verdict_annotation`] for a verdict that has none to name).
///
/// At most once per process, shared with [`github_verdict_annotation`].
pub fn github_blocker_annotation(blockers: &[String]) {
    if blockers.is_empty() {
        return;
    }
    announce_verdict_once(
        "hauksbee evidence undermined",
        &crate::result::inconclusive_verdict(blockers),
    );
}

/// GitHub error annotation for a strict run whose verdict is not a pass.
///
/// For a verdict no gate-grade finding explains: partly judged, or invalid with
/// no blocker list (an undermined run-level evidence map). `reason` is the run
/// verdict's own sentence, the one `--json` carries as `verdict_reason`. No-op
/// outside GitHub Actions; at most once per process.
pub fn github_verdict_annotation(reason: &str) {
    let title = if reason.starts_with(hauksbee_ir::vocabulary::PARTLY_JUDGED_LEAD) {
        "hauksbee PARTLY JUDGED"
    } else {
        "hauksbee verdict is not a pass"
    };
    announce_verdict_once(title, reason);
}

/// GitHub error annotation for a whole-run refusal.
///
/// The refusal is the exit-3 document the `JUnit` `<error>` and the SARIF
/// `hauksbee/invalid-for-analysis` result carry, so the annotation surface cannot
/// stay silent on a run the other two artifacts show red. It fires on the
/// refusal, not on the exit code, so a run that refused AND raised faults
/// annotates both this and the `--strict` gate while exiting 2: the faults
/// outrank the refusal for the exit code without making the refusal untrue. No-op
/// outside GitHub Actions.
pub fn github_refusal_annotation(refusal: &Refusal) {
    if std::env::var_os("GITHUB_ACTIONS").is_none() {
        return;
    }
    eprintln!(
        "::error title=hauksbee invalid for analysis::{}",
        // `%` first, then the newline: a workflow command decodes the escapes,
        // so escaping the newline first would send its `%0A` back through the
        // percent pass and arrive as a literal `%250A`.
        refusal
            .render_text()
            .replace('%', "%25")
            .replace('\r', "%0D")
            .replace('\n', "%0A")
    );
}

/// GitHub annotations for evidence that is not entitled to a clean result,
/// with the run-level split.
///
/// An undermined finding-backed map annotates as a warning, not an `::error`,
/// so the annotation level can never contradict the JUnit/SARIF entry or the
/// JSON verdict built from the same maps.
pub fn github_evidence_annotations_with_gate(
    maps: &[hauksbee_ir::evidence::EvidenceMap],
    gates: impl Fn(&hauksbee_ir::evidence::EvidenceMap) -> bool,
) {
    if std::env::var_os("GITHUB_ACTIONS").is_none() {
        return;
    }
    for finding in evidence_findings_with_gate(maps, gates) {
        // The same predicate JUnit and SARIF grade on, so the three artifact
        // surfaces cannot split over one map.
        let level = if is_failure(&finding) {
            "error"
        } else {
            "warning"
        };
        eprintln!(
            "::{level} title=hauksbee evidence {}::{}",
            finding.kind, finding.message
        );
    }
}

/// The one SARIF schema URL this writer targets.
///
/// Pinned so a consumer (GitHub code scanning validates against it) never sees
/// a drifting reference. A unit test asserts the emitted document carries
/// exactly this URL.
pub const SARIF_SCHEMA_URL: &str =
    "https://raw.githubusercontent.com/oasis-tcs/sarif-spec/master/Schemata/sarif-schema-2.1.0.json";
/// SARIF version matching [`SARIF_SCHEMA_URL`].
pub const SARIF_VERSION: &str = "2.1.0";

/// Whether a finding fails the `JUnit` testcase / escalates the SARIF level: the
/// finding's own [`JsonFinding::gates`] answer
/// from the predicate its surface's gate asks.
///
/// NOT the severity word. The check families gate wider than the shared
/// `serious` grade (a medium lint finding, any real SI finding, every co-sim
/// fault), so a run could exit 2 with a red `verdict` and still archive
/// `failures="0"`: a dashboard reading the artifact was told the build was fine.
/// The precedence between the grades is untouched, because it lives elsewhere:
/// a refusal is an `<error>`, not a `<failure>`, and the `invalid` route's
/// blockers keep their "INVALID evidence" text.
fn is_failure(f: &JsonFinding) -> bool {
    f.gates()
}

/// The `<check> <net/ref>` subject line shared with the strict-gate output.
fn subject(f: &JsonFinding) -> String {
    f.nets
        .first()
        .or_else(|| f.refs.first())
        .cloned()
        .unwrap_or_default()
}

/// Escape the five XML special characters for element text and attributes.
#[must_use]
pub fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Render the findings as one `JUnit` XML document.
///
/// One `<testsuite>` per check family, one `<testcase>` per finding (gating
/// findings carry a `<failure>`), and a synthetic passing "no findings" case for
/// a clean check so an empty suite is distinguishable from a suite that never
/// ran.
#[must_use]
pub fn junit_xml(board_name: &str, findings: &[JsonFinding]) -> String {
    junit_xml_with_refusal(board_name, findings, None)
}

/// `JUnit` with an optional exit-3 testcase. A refusal is `<error>`, never
/// `<failure>`: the analysis did not reach a hardware verdict.
#[must_use]
pub fn junit_xml_with_refusal(
    board_name: &str,
    findings: &[JsonFinding],
    refusal: Option<&Refusal>,
) -> String {
    junit_xml_for_checks(
        board_name,
        findings,
        refusal,
        &["drc".into(), "lint".into(), "si".into(), "usb_c".into()],
    )
}

fn junit_xml_for_checks(
    board_name: &str,
    findings: &[JsonFinding],
    refusal: Option<&Refusal>,
    selected_checks: &[String],
) -> String {
    use std::fmt::Write;
    // Group by check family, preserving first-seen order (deterministic:
    // findings arrive in report order).
    let mut order: Vec<&str> = Vec::new();
    for f in findings {
        if !order.contains(&f.check.as_str()) {
            order.push(&f.check);
        }
    }
    // The selected surface always records its families even when clean.
    for known in selected_checks {
        if !order.contains(&known.as_str()) {
            order.push(known);
        }
    }
    let mut s = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    let total: usize = order
        .iter()
        .map(|check| {
            findings
                .iter()
                .filter(|f| f.check == **check)
                .count()
                .max(1)
        })
        .sum::<usize>()
        + usize::from(refusal.is_some());
    let failures = findings.iter().filter(|f| is_failure(f)).count();
    let _ = writeln!(
        s,
        "<testsuites name=\"hauksbee {}\" tests=\"{}\" failures=\"{}\" errors=\"{}\">",
        xml_escape(board_name),
        total,
        failures,
        usize::from(refusal.is_some()),
    );
    for check in order {
        let of_check: Vec<&JsonFinding> = findings.iter().filter(|f| f.check == check).collect();
        let fail_n = of_check.iter().filter(|f| is_failure(f)).count();
        let _ = writeln!(
            s,
            "  <testsuite name=\"{}\" tests=\"{}\" failures=\"{}\">",
            xml_escape(check),
            of_check.len().max(1),
            fail_n
        );
        if of_check.is_empty() {
            let _ = writeln!(
                s,
                "    <testcase name=\"no findings\" classname=\"{}\"/>",
                xml_escape(check)
            );
        }
        for f in of_check {
            let name = if subject(f).is_empty() {
                f.kind.clone()
            } else {
                format!("{} {}", f.kind, subject(f))
            };
            if is_failure(f) {
                let _ = writeln!(
                    s,
                    "    <testcase name=\"{}\" classname=\"{}\">\n      <failure message=\"{}\"/>\n    </testcase>",
                    xml_escape(&name),
                    xml_escape(check),
                    xml_escape(&f.message)
                );
            } else {
                // Non-gating finding: a passing case that still carries the
                // text, so the CI UI shows it without failing the job.
                let _ = writeln!(
                    s,
                    "    <testcase name=\"{}\" classname=\"{}\">\n      <system-out>{}</system-out>\n    </testcase>",
                    xml_escape(&name),
                    xml_escape(check),
                    xml_escape(&f.message)
                );
            }
        }
        let _ = writeln!(s, "  </testsuite>");
    }
    if let Some(refusal) = refusal {
        let body = refusal.render_text();
        let _ = writeln!(
            s,
            "  <testsuite name=\"refusal\" tests=\"1\" failures=\"0\" errors=\"1\">\n    <testcase name=\"requested claim is answerable\" classname=\"hauksbee\">\n      <error message=\"{}\">{}</error>\n    </testcase>\n  </testsuite>",
            xml_escape(&refusal.missing_prerequisite),
            xml_escape(&body),
        );
    }
    s.push_str("</testsuites>\n");
    s
}

/// Render the findings as one SARIF 2.1.0 document.
///
/// Rules are deduplicated by `check/kind`; each finding becomes a result whose
/// level maps gating -> error, non-gating -> warning, located at the board file
/// (hauksbee findings are electrical, not line-addressed).
#[must_use]
pub fn sarif_json(board_path: &Path, findings: &[JsonFinding]) -> String {
    sarif_json_with_refusal(board_path, findings, None)
}

/// SARIF with a structured exit-3 result. The properties retain the complete
/// refusal object so code-scanning consumers need not parse prose.
#[must_use]
pub fn sarif_json_with_refusal(
    board_path: &Path,
    findings: &[JsonFinding],
    refusal: Option<&Refusal>,
) -> String {
    sarif_json_with_terminal_outcome(
        board_path,
        findings,
        refusal,
        crate::result::EXIT_INVALID_FOR_ANALYSIS,
        TerminalOutcomeKind::AnalysisRefusal,
    )
}

fn sarif_json_with_terminal_outcome(
    board_path: &Path,
    findings: &[JsonFinding],
    refusal: Option<&Refusal>,
    exit_code: i32,
    outcome_kind: TerminalOutcomeKind,
) -> String {
    let mut rule_ids: Vec<String> = Vec::new();
    for f in findings {
        let id = format!("{}/{}", f.check, f.kind);
        if !rule_ids.contains(&id) {
            rule_ids.push(id);
        }
    }
    let mut rules: Vec<serde_json::Value> = rule_ids
        .iter()
        .map(|id| serde_json::json!({ "id": id }))
        .collect();
    let uri = board_path.to_string_lossy();
    let mut results: Vec<serde_json::Value> = findings
        .iter()
        .map(|f| {
            let mut message = f.message.clone();
            let subj = subject(f);
            if !subj.is_empty() && !message.contains(&subj) {
                message = format!("{subj}: {message}");
            }
            serde_json::json!({
                "ruleId": format!("{}/{}", f.check, f.kind),
                "level": if is_failure(f) { "error" } else { "warning" },
                "message": { "text": message },
                "locations": [{
                    "physicalLocation": {
                        "artifactLocation": { "uri": uri }
                    }
                }]
            })
        })
        .collect();
    if let Some(refusal) = refusal {
        let (rule_id, status) = match outcome_kind {
            TerminalOutcomeKind::AnalysisRefusal | TerminalOutcomeKind::WholeRunRefusal => {
                ("hauksbee/invalid-for-analysis", "invalid_for_analysis")
            }
            TerminalOutcomeKind::RunError => ("hauksbee/run-error", "run_error"),
        };
        rules.push(serde_json::json!({ "id": rule_id }));
        results.push(serde_json::json!({
            "ruleId": rule_id,
            "level": "error",
            "message": { "text": refusal.render_text() },
            "locations": [{
                "physicalLocation": { "artifactLocation": { "uri": uri } }
            }],
            "properties": {
                "status": status,
                "exit_code": exit_code,
                "refusal": refusal,
            }
        }));
    }
    let doc = serde_json::json!({
        "$schema": SARIF_SCHEMA_URL,
        "version": SARIF_VERSION,
        "runs": [{
            "tool": {
                "driver": {
                    "name": "hauksbee",
                    "version": env!("CARGO_PKG_VERSION"),
                    "rules": rules
                }
            },
            "results": results
        }]
    });
    serde_json::to_string_pretty(&doc).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
}

/// GitHub Actions workflow annotations for gate-grade findings.
///
/// Printed only under `GITHUB_ACTIONS` and only when `--strict` is gating (the
/// same items the `FAILED under --strict` line names). One `::error` per
/// finding.
///
/// stderr, like every other annotation here: a workflow command is not report
/// content, and on stdout it appended non-JSON lines after the `--json`
/// document, so a consumer parsing a gating run's output failed on trailing
/// data instead of reading the verdict it was gating on.
pub fn github_annotations(items: &[String]) {
    if std::env::var_os("GITHUB_ACTIONS").is_none() {
        return;
    }
    for item in items {
        eprintln!("::error title=hauksbee --strict gate::{item}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A finding whose gate grade follows its severity, the shape the DRC and
    /// evidence families produce. The gate-wider families have their own tests
    /// in `tests/verdict_contract.rs`, end to end through the CLI.
    fn finding(check: &str, kind: &str, severity: &str, net: &str, msg: &str) -> JsonFinding {
        JsonFinding {
            check: check.to_string(),
            kind: kind.to_string(),
            severity: severity.to_string(),
            nets: vec![net.to_string()],
            location_mm: None,
            layer: None,
            refs: Vec::new(),
            actionable: true,
            message: msg.to_string(),
            plain: msg.to_string(),
            fix: None,
        }
    }

    #[test]
    fn sarif_pins_the_exact_schema_url_and_version() {
        let doc = sarif_json(
            Path::new("b.kicad_pcb"),
            &[finding(
                "drc",
                "short",
                "serious",
                "GND",
                "GND shorted to +5V",
            )],
        );
        let v: serde_json::Value = serde_json::from_str(&doc).expect("sarif parses");
        // The pinned URL, byte for byte: consumers validate against it.
        assert_eq!(
            v["$schema"],
            "https://raw.githubusercontent.com/oasis-tcs/sarif-spec/master/Schemata/sarif-schema-2.1.0.json"
        );
        assert_eq!(v["version"], "2.1.0");
        assert_eq!(v["runs"][0]["results"][0]["level"], "error");
        assert_eq!(v["runs"][0]["tool"]["driver"]["name"], "hauksbee");
    }

    #[test]
    fn junit_maps_a_gating_finding_to_failure_and_keeps_clean_suites_nonempty() {
        let out = junit_xml(
            "b",
            &[
                finding("drc", "short", "serious", "GND", "GND shorted to +5V"),
                finding("drc", "clearance", "warning", "USB_DP", "pair mismatched"),
            ],
        );
        assert!(out.contains("<failure message=\"GND shorted to +5V\"/>"));
        // Non-serious finding is a passing case with the text preserved.
        assert!(out.contains("<system-out>pair mismatched</system-out>"));
        // Families with no findings still appear with a synthetic pass.
        assert!(out.contains("<testsuite name=\"lint\" tests=\"1\" failures=\"0\">"));
        assert!(out.contains("<testcase name=\"no findings\" classname=\"lint\"/>"));
        // Escaping holds.
        let esc = junit_xml("b", &[finding("lint", "x<y", "serious", "A&B", "m\"q\"")]);
        assert!(
            esc.contains("x&lt;y") && esc.contains("A&amp;B") && esc.contains("m&quot;q&quot;")
        );
    }

    #[test]
    fn the_gate_grade_is_the_gating_flag_and_not_the_severity_word() {
        // Both directions, on the two shapes that really occur: a medium lint
        // finding that gates (`warning`, gating) and a possibly-phantom copper
        // short that does not (`warning`, not gating). Grading on the severity
        // word made the first one green.
        let gating = finding("lint", "placeholder_value", "warning", "+5V", "no value");
        let phantom = finding("drc", "short", "warning", "GND", "may be phantom");
        let out = junit_xml("b", &[gating.clone(), phantom.clone()]);
        assert!(out.contains("failures=\"1\""), "{out}");
        assert!(out.contains("<failure message=\"no value\"/>"), "{out}");
        assert!(
            out.contains("<system-out>may be phantom</system-out>"),
            "{out}"
        );

        let sarif = sarif_json(Path::new("b.kicad_pcb"), &[gating, phantom]);
        let v: serde_json::Value = serde_json::from_str(&sarif).expect("sarif parses");
        assert_eq!(v["runs"][0]["results"][0]["level"], "error");
        assert_eq!(v["runs"][0]["results"][1]["level"], "warning");
    }

    #[test]
    fn undermined_evidence_is_a_failure_on_junit_and_sarif() {
        use hauksbee_ir::evidence::{
            Assumption, CausalPathIndex, EvidenceMap, EvidenceRegistry, NetScope, RunDate,
        };
        let registry = EvidenceRegistry::new(vec![Assumption::open_part(
            "U9",
            "regulator",
            "no model matched",
        )])
        .unwrap();
        let graph = CausalPathIndex::from_net_parts([("VBUS", ["U9"].as_slice())]).unwrap();
        let traversal = graph
            .traverse(&NetScope::new(["VBUS"], None).unwrap(), &registry)
            .unwrap();
        let map = EvidenceMap::from_traversal(
            "VBUS peak stays below 5.5 V",
            traversal,
            &registry,
            RunDate::from_epoch_days(20_666),
        )
        .unwrap();
        let evidence = evidence_findings(&[map]).pop().unwrap();
        assert_eq!(evidence.severity, "serious");
        assert_eq!(evidence.kind, "undermined");
        let junit = junit_xml("b", std::slice::from_ref(&evidence));
        assert!(junit.contains("<failure"), "{junit}");
        assert!(junit.contains("INVALID evidence"), "{junit}");
        let sarif = sarif_json(Path::new("b.kicad_pcb"), &[evidence]);
        let value: serde_json::Value = serde_json::from_str(&sarif).unwrap();
        assert_eq!(value["runs"][0]["results"][0]["level"], "error");
        assert!(value["runs"][0]["results"][0]["message"]["text"]
            .as_str()
            .unwrap()
            .contains("INVALID evidence"));
    }

    fn noted(verdict: &str, partial: bool) -> NotedVerdict {
        NotedVerdict {
            verdict: verdict.into(),
            partial,
            reason: Some(format!("PARTLY JUDGED (not a pass): the {verdict} reason")),
        }
    }

    /// The invariant: `JUnit` and SARIF are never green beside a verdict that is
    /// not a pass, whatever findings they were handed. Faulty case: a partly
    /// judged and an invalid verdict over warnings only. Controls: a pass stays
    /// green, and a verdict already red through a failing finding or a refusal
    /// is not doubled.
    #[test]
    fn a_verdict_that_is_not_a_pass_is_never_a_green_artifact() {
        let warning = finding("drc", "clearance", "warning", "VCC", "ratings abstained");
        let warning = JsonFinding {
            actionable: false,
            ..warning
        };
        for (verdict, partial, kind) in [
            ("invalid", true, "partly_judged"),
            ("invalid", false, "undermined"),
            ("fail", false, "verdict_fail"),
        ] {
            let v = noted(verdict, partial);
            let all = artifact_findings(std::slice::from_ref(&warning), None, Some(&v));
            assert_eq!(all.len(), 2, "{verdict}: one gate-grade entry added");
            assert_eq!(all[1].kind, kind);
            let junit = junit_xml("b", &all);
            assert!(junit.contains("failures=\"1\""), "{verdict}: {junit}");
            assert!(junit.contains(&format!("the {verdict} reason")), "{junit}");
            let sarif: serde_json::Value =
                serde_json::from_str(&sarif_json(Path::new("b.kicad_pcb"), &all)).unwrap();
            assert!(
                sarif["runs"][0]["results"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|r| r["level"] == "error"),
                "{verdict}: SARIF needs an error"
            );
        }
        // Legal controls.
        let pass = noted("pass", false);
        assert_eq!(
            artifact_findings(std::slice::from_ref(&warning), None, Some(&pass)).len(),
            1,
            "a pass adds nothing"
        );
        assert_eq!(
            artifact_findings(std::slice::from_ref(&warning), None, None).len(),
            1,
            "no noted verdict adds nothing"
        );
        let red = finding("drc", "short", "serious", "GND", "GND shorted to +5V");
        let invalid = noted("invalid", false);
        assert_eq!(
            artifact_findings(std::slice::from_ref(&red), None, Some(&invalid)).len(),
            1,
            "an already failing finding is not doubled"
        );
        let refusal =
            crate::result::Refusal::new("a claim", "a prerequisite", Vec::<String>::new(), "fix");
        assert_eq!(
            artifact_findings(&[], Some(&refusal), Some(&invalid)).len(),
            0,
            "a refusal already makes the artifacts red"
        );
    }

    /// A run that never read the board (a run error) or refused its whole claim
    /// writes no passing "no findings" case; a run that read the board and ended
    /// with findings (even beside a retained refusal) keeps the families it did
    /// judge clean.
    #[test]
    fn an_unread_board_gets_no_passing_cases() {
        let dir =
            std::env::temp_dir().join(format!("hauksbee-ci-artifacts-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let junit = dir.join("j.xml");
        let sarif = dir.join("s.sarif");
        let state = CurrentRunArtifacts {
            board_name: "b.brd".into(),
            board_path: "b.brd".into(),
            junit: Some(junit.clone()),
            sarif: Some(sarif),
            findings: Vec::new(),
            refusal: None,
            selected_checks: vec!["drc".into(), "lint".into(), "si".into(), "usb_c".into()],
            verdict: None,
        };
        let refusal =
            crate::result::Refusal::new("a claim", "unreadable", Vec::<String>::new(), "fix");
        write_current(&state, Some(&refusal), 1, TerminalOutcomeKind::RunError).unwrap();
        let unread = std::fs::read_to_string(&junit).unwrap();
        assert!(!unread.contains("no findings"), "{unread}");
        assert!(unread.contains("errors=\"1\""), "{unread}");
        write_current(
            &state,
            Some(&refusal),
            3,
            TerminalOutcomeKind::AnalysisRefusal,
        )
        .unwrap();
        let read = std::fs::read_to_string(&junit).unwrap();
        assert!(read.contains("no findings"), "{read}");
        // A whole-run refusal (`exit_with_refusal`) judged no family either.
        write_current(
            &state,
            Some(&refusal),
            3,
            TerminalOutcomeKind::WholeRunRefusal,
        )
        .unwrap();
        let refused = std::fs::read_to_string(&junit).unwrap();
        assert!(!refused.contains("no findings"), "{refused}");
        assert!(refused.contains("errors=\"1\""), "{refused}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_for_analysis_is_an_error_in_junit_and_a_refusal_result_in_sarif() {
        let refusal = crate::result::Refusal::new(
            "strict co-sim verdict",
            "a converged analog solve",
            vec!["static copper findings remain valid"],
            "fix the named floating net, then rerun",
        );
        let junit = junit_xml_with_refusal("b", &[], Some(&refusal));
        assert!(
            junit.contains("tests=\"5\""),
            "root count includes four clean families plus refusal: {junit}"
        );
        assert!(junit.contains("errors=\"1\""), "{junit}");
        for value in [
            &refusal.claim,
            &refusal.missing_prerequisite,
            &refusal.valid_partial_conclusions[0],
            &refusal.next_action,
        ] {
            assert!(junit.contains(value), "JUnit lost {value:?}: {junit}");
        }

        let sarif = sarif_json_with_refusal(Path::new("b.kicad_pcb"), &[], Some(&refusal));
        let value: serde_json::Value = serde_json::from_str(&sarif).expect("SARIF JSON");
        let result = &value["runs"][0]["results"][0];
        assert_eq!(result["ruleId"], "hauksbee/invalid-for-analysis");
        assert_eq!(result["properties"]["exit_code"], 3);
        assert_eq!(result["properties"]["refusal"]["claim"], refusal.claim);
    }
}
