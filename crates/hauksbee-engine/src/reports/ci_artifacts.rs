//! CI-native artifacts for `hauksbee run`: `--junit <path>` (JUnit XML) and
//! `--sarif <path>` (SARIF 2.1.0), both computed from the SAME full static
//! suite the `--check` report renders, so a pipeline consumes findings as
//! test results / code-scanning alerts without parsing the human report.
//! GitHub annotations for gate-grade findings ride the `--strict` gate
//! (see [`super::strict_gate_exit`]'s caller in `reports::mod`).

use std::path::Path;

use crate::result::{JsonFinding, Refusal};

/// Convert non-clean evidence statuses into the same finding vocabulary CI
/// writers already consume. Undermined RUN-LEVEL evidence is gate-grade;
/// qualified evidence remains visible without becoming green-by-omission.
pub fn evidence_findings(maps: &[hauksbee_ir::evidence::EvidenceMap]) -> Vec<JsonFinding> {
    evidence_findings_with_gate(maps, |_| true)
}

/// As [`evidence_findings`], with the same run-level split the JSON verdict
/// makes: an undermined map whose assertion `gates` says is NOT run-level
/// (it backs an individual finding) is demoted to a warning badge instead of
/// a gate-grade failure, so JUnit/SARIF can never fail a run whose JSON
/// verdict says pass, or vice versa.
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

/// GitHub error annotation for unbound verdict-critical parts, so the
/// annotation surface agrees with the gate-grade JUnit/SARIF entry the same
/// blockers produce. No-op outside GitHub Actions.
///
/// At most once per process. Two call sites reach it for the same run: the
/// artifact writer (which must annotate a NON-gating run, the only surface that
/// would otherwise say nothing) and the invalid-for-analysis exit (which must
/// annotate a gating run whether or not artifacts were asked for). A run that
/// passes through both would otherwise spend two of GitHub's ten
/// annotations-per-type-per-step on the same refusal. The artifact writer runs
/// first and names the whole run's blockers, so the surviving annotation is the
/// widest one: on `--usb-c`, whose exit site names only the CC-scoped subset,
/// the kept line is the superset rather than that surface's own list.
pub fn github_blocker_annotation(blockers: &[String]) {
    static ANNOUNCED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if blockers.is_empty() || std::env::var_os("GITHUB_ACTIONS").is_none() {
        return;
    }
    if ANNOUNCED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    eprintln!(
        "::error title=hauksbee evidence undermined::{}",
        crate::result::inconclusive_verdict(blockers)
    );
}

/// GitHub error annotation for a whole-run refusal (the exit-3 documents the
/// JUnit `<error>` and the SARIF `hauksbee/invalid-for-analysis` result carry),
/// so the annotation surface cannot stay silent on a run the other two
/// artifacts show red. No-op outside GitHub Actions.
pub fn github_refusal_annotation(refusal: &Refusal) {
    if std::env::var_os("GITHUB_ACTIONS").is_none() {
        return;
    }
    eprintln!(
        "::error title=hauksbee invalid for analysis::{}",
        // `%` first, then the newline: a workflow command decodes the
        // escapes, so escaping the newline before the percent would turn the
        // refusal's own text into a mangled `%0A`.
        refusal
            .render_text()
            .replace('%', "%25")
            .replace('\n', "%0A")
    );
}

/// GitHub annotations for evidence that is not entitled to a clean result.
pub fn github_evidence_annotations(maps: &[hauksbee_ir::evidence::EvidenceMap]) {
    github_evidence_annotations_with_gate(maps, |_| true)
}

/// As [`github_evidence_annotations`], with the run-level split: an
/// undermined finding-backed map annotates as a warning, not an `::error`,
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
        let level = if finding.severity == "serious" {
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

/// The one SARIF schema URL this writer targets, pinned so a consumer (GitHub
/// code scanning validates against it) never sees a drifting reference. A unit
/// test asserts the emitted document carries exactly this URL.
pub const SARIF_SCHEMA_URL: &str =
    "https://raw.githubusercontent.com/oasis-tcs/sarif-spec/master/Schemata/sarif-schema-2.1.0.json";
/// SARIF version matching [`SARIF_SCHEMA_URL`].
pub const SARIF_VERSION: &str = "2.1.0";

/// Whether a finding fails the JUnit testcase / escalates the SARIF level.
/// "serious" is the shared gate grade across the report surfaces.
fn is_failure(f: &JsonFinding) -> bool {
    f.severity == "serious"
}

/// The `<check> <net/ref>` subject line shared with the strict-gate output.
fn subject(f: &JsonFinding) -> String {
    f.nets
        .first()
        .or_else(|| f.refs.first())
        .cloned()
        .unwrap_or_default()
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Render the findings as one JUnit XML document: one `<testsuite>` per check
/// family, one `<testcase>` per finding (serious findings carry a
/// `<failure>`), and a synthetic passing "no findings" case for a clean check
/// so an empty suite is distinguishable from a suite that never ran.
pub fn junit_xml(board_name: &str, findings: &[JsonFinding]) -> String {
    junit_xml_with_refusal(board_name, findings, None)
}

/// JUnit with an optional exit-3 testcase. A refusal is `<error>`, never
/// `<failure>`: the analysis did not reach a hardware verdict.
pub fn junit_xml_with_refusal(
    board_name: &str,
    findings: &[JsonFinding],
    refusal: Option<&Refusal>,
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
    // The full suite always covers these families even when clean.
    for known in ["drc", "lint", "si", "usb_c"] {
        if !order.contains(&known) {
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

/// Render the findings as one SARIF 2.1.0 document. Rules are deduplicated by
/// `check/kind`; each finding becomes a result whose level maps serious ->
/// error, everything else -> warning, located at the board file (hauksbee
/// findings are electrical, not line-addressed).
pub fn sarif_json(board_path: &Path, findings: &[JsonFinding]) -> String {
    sarif_json_with_refusal(board_path, findings, None)
}

/// SARIF with a structured exit-3 result. The properties retain the complete
/// refusal object so code-scanning consumers need not parse prose.
pub fn sarif_json_with_refusal(
    board_path: &Path,
    findings: &[JsonFinding],
    refusal: Option<&Refusal>,
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
        rules.push(serde_json::json!({ "id": "hauksbee/invalid-for-analysis" }));
        results.push(serde_json::json!({
            "ruleId": "hauksbee/invalid-for-analysis",
            "level": "error",
            "message": { "text": refusal.render_text() },
            "locations": [{
                "physicalLocation": { "artifactLocation": { "uri": uri } }
            }],
            "properties": {
                "status": "invalid_for_analysis",
                "exit_code": 3,
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

/// GitHub Actions workflow annotations for gate-grade findings, printed only
/// under `GITHUB_ACTIONS` and only when `--strict` is gating (the same items
/// the `FAILED under --strict` line names). One `::error` per finding.
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
    fn junit_maps_serious_to_failure_and_keeps_clean_suites_nonempty() {
        let out = junit_xml(
            "b",
            &[
                finding("drc", "short", "serious", "GND", "GND shorted to +5V"),
                finding(
                    "si",
                    "usb_diff_pair",
                    "warning",
                    "USB_DP",
                    "pair mismatched",
                ),
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
        let junit = junit_xml("b", &[evidence.clone()]);
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
