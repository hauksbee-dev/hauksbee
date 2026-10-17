//! EasyEDA boards through the real front doors: `hauksbee run <board> --check
//! --json` on the compiled binary and the web `analyze` entry. The fixtures are
//! the public MIT boards in the extract crate (see its
//! `tests/fixtures/easyeda/README.md` for sources and licences).

use crate::support::{board, run};
use hauksbee_engine::frontdoor::analyze;
use serde_json::Value;
use std::io::Write;
use std::path::PathBuf;

const NOFILTER: &str = "rpitx-coax-nofilter.json";
const GP1212: &str = "rpitx-coax-gp1212-gp731.json";
const NANO: &str = "nano-hub-pcb1.epcb";

fn fixture(name: &str) -> PathBuf {
    board("../hauksbee-extract/tests/fixtures/easyeda").join(name)
}

fn scratch(tag: &str) -> PathBuf {
    hauksbee_testkit::fresh_scratch_dir("easyeda", tag)
}

/// `hauksbee run <path> --check --json`, parsed.
fn check_json(path: &std::path::Path) -> (std::process::Output, Value) {
    let out = run(&["run", path.to_str().unwrap(), "--check", "--json"]);
    let json: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "--json stdout must be JSON ({e}); stderr:\n{}",
            String::from_utf8_lossy(&out.stderr)
        )
    });
    (out, json)
}

/// Where a refusal lands: `--json` puts it on stdout as `{"error": ...}`, the
/// plain form on stderr.
fn refusal_text(out: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn assumption_ids(json: &Value) -> Vec<String> {
    json["assumptions"]
        .as_array()
        .expect("assumptions array")
        .iter()
        .map(|a| a["id"].as_str().unwrap_or_default().to_string())
        .collect()
}

#[test]
fn the_check_json_reads_each_fixture_as_an_easyeda_board() {
    // (fixture, components, nets named by pads or copper)
    for (name, parts) in [(NOFILTER, 3_u64), (GP1212, 4), (NANO, 23)] {
        let path = fixture(name);
        let (out, json) = check_json(&path);
        assert!(
            matches!(out.status.code(), Some(0 | 3)),
            "{name}: a read board exits 0 (or 3 under --strict), got {:?}:\n{}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        );
        let input = &json["inputs"][0];
        assert_eq!(input["format"], "easyeda", "{name}: {input}");
        assert_eq!(input["kind"], "board");
        let contributed = input["contributed"][0].as_str().unwrap_or_default();
        assert!(
            contributed.starts_with(&format!("{parts} components and ")),
            "{name}: {contributed}"
        );
        // The board is named for its file, not for a placeholder.
        let stem = name.rsplit_once('.').map_or(name, |(s, _)| s);
        assert_eq!(json["board"], stem, "{name}");

        // Copper was counted, not measured: the two copper checks are recorded
        // as not run rather than silently absent or reported clean.
        let ids = assumption_ids(&json);
        for want in ["not-checked:drc", "not-checked:si"] {
            assert!(ids.iter().any(|i| i == want), "{name}: {want} in {ids:?}");
        }
        // And the reader's own accounting reaches the report.
        let ignored = input["ignored"].to_string();
        for phrase in ["not reverse-engineered from copper", "Read past"] {
            assert!(ignored.contains(phrase), "{name}: {phrase} in {ignored}");
        }
        // A format that could not check copper must not come back as a pass.
        assert_ne!(json["verdict"], "pass", "{name}: {}", json["verdict"]);
    }
}

#[test]
fn a_part_with_no_value_is_an_open_part_not_a_guess() {
    let (_, json) = check_json(&fixture(GP1212));
    let ids = assumption_ids(&json);
    assert!(
        ids.iter().any(|i| i == "open-part:U1"),
        "U1 has no value in this file and no model, so it is open: {ids:?}"
    );
    let ignored = json["inputs"][0]["ignored"].to_string();
    assert!(ignored.contains("carry no value"), "{ignored}");
}

#[test]
fn what_was_not_read_is_named_in_the_report() {
    let (_, json) = check_json(&fixture(GP1212));
    let ignored = json["inputs"][0]["ignored"].to_string();
    assert!(ignored.contains("NOT read"), "{ignored}");
    assert!(ignored.contains("TEXT on a copper layer"), "{ignored}");
    // Positive accounting is provenance, not a fidelity alarm: the "read past"
    // note must not become a reduced-fidelity assumption of its own.
    let alarms: Vec<String> = json["assumptions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|a| a["kind"] == "reduced_fidelity")
        .map(|a| a["because"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(
        alarms.iter().all(|b| !b.contains("Read past")),
        "read-past accounting raised a fidelity alarm: {alarms:?}"
    );
}

#[test]
fn a_renamed_easyeda_file_reads_by_content() {
    let dir = scratch("renamed");
    let path = dir.join("my-board.dat");
    std::fs::copy(fixture(GP1212), &path).unwrap();
    let (_, json) = check_json(&path);
    assert_eq!(json["inputs"][0]["format"], "easyeda");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_web_entry_reads_an_easyeda_board_too() {
    for (name, parts) in [(GP1212, 4_usize), (NANO, 23)] {
        let bytes = std::fs::read(fixture(name)).unwrap();
        let report = analyze(name, &bytes);
        assert!(report.ok, "{name}: {:?}", report.error);
        assert_eq!(report.num_components, parts, "{name}");
        let diagnostics = report.import_diagnostics.expect("import diagnostics");
        assert_eq!(diagnostics.format, "EasyEDA");
        assert_eq!(diagnostics.objects.len(), parts);
        // The reader's limits are surfaced to the browser, not just the CLI.
        assert!(
            diagnostics.missing_or_refused >= 1,
            "{name}: the 'were not run' / 'NOT read' notes are counted"
        );
        // Copper was not measured, so the spacing section says so.
        let text = serde_json::to_string(&report.sections)
            .unwrap()
            .to_ascii_lowercase();
        assert!(
            text.contains("not checked: clearance drc"),
            "{name}: {text}"
        );
    }
}

#[test]
fn an_unrecognized_file_lists_easyeda_among_the_accepted_formats() {
    let report = analyze("notes.txt", b"this is not a board file at all");
    assert!(!report.ok);
    let error = report.error.expect("an error");
    assert!(error.contains("EasyEDA"), "{error}");

    let dir = scratch("unrecognized");
    let path = dir.join("nothing.board");
    std::fs::write(&path, "board version 1\n").unwrap();
    let out = run(&["run", path.to_str().unwrap(), "--report"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("EasyEDA"), "{stderr}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_truncated_export_is_refused_with_what_to_do() {
    let bytes = std::fs::read(fixture(NOFILTER)).unwrap();
    let dir = scratch("truncated");
    let path = dir.join("cut.json");
    std::fs::write(&path, &bytes[..bytes.len() / 2]).unwrap();
    let out = run(&["run", path.to_str().unwrap(), "--check", "--json"]);
    assert_eq!(out.status.code(), Some(1), "an unreadable board exits 1");
    let stderr = refusal_text(&out);
    for phrase in ["truncated", "Re-export"] {
        assert!(stderr.contains(phrase), "{phrase} in:\n{stderr}");
    }
    assert!(!stderr.contains("panicked"), "{stderr}");
    let _ = std::fs::remove_dir_all(&dir);

    let report = analyze("cut.json", &bytes[..bytes.len() / 2]);
    assert!(!report.ok);
    assert!(report.error.unwrap().contains("truncated"));
}

#[test]
fn a_schematic_or_pro_v3_file_is_refused_by_name() {
    let dir = scratch("named_refusals");
    let sch = dir.join("sheet.json");
    std::fs::write(
        &sch,
        r#"{"head":{"docType":"1"},"canvas":"CA~1","shape":[]}"#,
    )
    .unwrap();
    let out = run(&["run", sch.to_str().unwrap(), "--check", "--json"]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = refusal_text(&out);
    assert!(
        stderr.contains("schematic") && stderr.contains("PCB editor"),
        "{stderr}"
    );

    let v3 = dir.join("v3.epcb");
    std::fs::write(
        &v3,
        "{\"type\":\"DOCHEAD\",\"version\":\"3\"}||{\"docType\":\"PCB\"}\n",
    )
    .unwrap();
    let out = run(&["run", v3.to_str().unwrap(), "--check", "--json"]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = refusal_text(&out);
    assert!(
        stderr.contains("v3") && stderr.contains("Gerber"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("unrecognized board format"),
        "named, not generic: {stderr}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_epro_project_archive_reads_through_the_cli() {
    let dir = scratch("epro");
    let path = dir.join("Nano.epro");
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    w.start_file("project.json", opts).unwrap();
    w.write_all(br#"{"pcbs":{}}"#).unwrap();
    w.start_file("PCB/Nano.epcb", opts).unwrap();
    w.write_all(&std::fs::read(fixture(NANO)).unwrap()).unwrap();
    std::fs::write(&path, w.finish().unwrap().into_inner()).unwrap();

    let (_, json) = check_json(&path);
    assert_eq!(json["inputs"][0]["format"], "easyeda");
    assert_eq!(json["board"], "Nano");
    let contributed = json["inputs"][0]["contributed"][0].as_str().unwrap();
    assert!(contributed.starts_with("23 components"), "{contributed}");
    let _ = std::fs::remove_dir_all(&dir);
}
