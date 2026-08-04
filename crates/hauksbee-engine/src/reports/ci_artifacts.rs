//! CI-native artifacts for `hauksbee run`: `--junit <path>` (JUnit XML) and
//! `--sarif <path>` (SARIF 2.1.0), both computed from the SAME full static
//! suite the `--check` report renders, so a pipeline consumes findings as
//! test results / code-scanning alerts without parsing the human report.
//! GitHub annotations for gate-grade findings ride the `--strict` gate
//! (see [`super::strict_gate_exit`]'s caller in `reports::mod`).

use std::path::Path;

use crate::result::JsonFinding;

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
    let total: usize = findings.len().max(1);
    let failures = findings.iter().filter(|f| is_failure(f)).count();
    let _ = writeln!(
        s,
        "<testsuites name=\"hauksbee {}\" tests=\"{}\" failures=\"{}\">",
        xml_escape(board_name),
        total,
        failures
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
    s.push_str("</testsuites>\n");
    s
}

/// Render the findings as one SARIF 2.1.0 document. Rules are deduplicated by
/// `check/kind`; each finding becomes a result whose level maps serious ->
/// error, everything else -> warning, located at the board file (hauksbee
/// findings are electrical, not line-addressed).
pub fn sarif_json(board_path: &Path, findings: &[JsonFinding]) -> String {
    let mut rule_ids: Vec<String> = Vec::new();
    for f in findings {
        let id = format!("{}/{}", f.check, f.kind);
        if !rule_ids.contains(&id) {
            rule_ids.push(id);
        }
    }
    let rules: Vec<serde_json::Value> = rule_ids
        .iter()
        .map(|id| serde_json::json!({ "id": id }))
        .collect();
    let uri = board_path.to_string_lossy();
    let results: Vec<serde_json::Value> = findings
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
pub fn github_annotations(items: &[String]) {
    if std::env::var_os("GITHUB_ACTIONS").is_none() {
        return;
    }
    for item in items {
        println!("::error title=hauksbee --strict gate::{item}");
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
}
