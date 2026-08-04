//! `hauksbee-ci check` end-to-end: the real binary, real boards, real exit
//! codes, and the `--json` diagnostics contract (E52/E53/E54).
//!
//! * a valid spec + board: exit 0, no simulation run;
//! * `--no-board`: structural validation only, a board that does not exist is
//!   fine;
//! * `--json`: an array of {line, col, code, message, fix}, with exact
//!   line/col for TOML parse errors (parser spans) and best-effort identifier
//!   resolution for validation errors; a valid spec prints `[]`;
//! * every INDEPENDENT error is reported in one invocation, on both the
//!   plain and `--json` paths (and through `run`'s spec-error path too).

use std::path::PathBuf;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_hauksbee-ci")
}

fn blinky_board() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/boards/blinky.kicad_pcb")
}

/// Write `body` to `<tag>.toml` in a fresh temp dir (with a copy of the
/// blinky board beside it) and return (dir, spec path).
fn spec_dir(tag: &str, body: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(blinky_board(), dir.path().join("blinky.kicad_pcb")).unwrap();
    let spec = dir.path().join(format!("{tag}.toml"));
    std::fs::write(&spec, body).unwrap();
    (dir, spec)
}

fn check(spec: &std::path::Path, extra: &[&str]) -> std::process::Output {
    Command::new(bin())
        .arg("check")
        .arg(spec)
        .args(extra)
        .output()
        .expect("binary runs")
}

fn json_diags(out: &std::process::Output) -> Vec<serde_json::Value> {
    let stdout = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str::<Vec<serde_json::Value>>(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout must be one JSON array ({e}): {stdout}"))
}

const VALID: &str = "name = \"check-valid\"\nboard = \"blinky.kicad_pcb\"\nduration_ms = 20\n\n\
                     [[supply]]\nnet = \"+5V\"\nkind = \"ideal\"\nvolts = 5.0\n\n\
                     [[assert]]\nkind = \"voltage\"\nnet = \"+5V\"\nmin = 1.0\n";

#[test]
fn check_on_a_valid_spec_loads_the_board_and_exits_zero() {
    let (_dir, spec) = spec_dir("valid", VALID);
    let out = check(&spec, &[]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "valid spec must exit 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("OK"), "plain output says OK: {stdout}");
    // And the JSON shape of the same result is the documented empty array.
    let out = check(&spec, &["--json"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(
        json_diags(&out).is_empty(),
        "a valid spec is an EMPTY diagnostics array"
    );
}

#[test]
fn no_board_skips_board_resolution_but_not_structure() {
    // The board file does not exist: --no-board must still pass a
    // structurally valid spec (the editor's board may not be checked out)...
    let (_dir, spec) = spec_dir(
        "noboard",
        "board = \"missing.kicad_pcb\"\nduration_ms = 20\n\n\
         [[assert]]\nkind = \"voltage\"\nnet = \"+5V\"\nmin = 1.0\n",
    );
    let out = check(&spec, &["--no-board"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "--no-board must not require the board; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // ...while the default (board-loading) mode reports it as board-load.
    let out = check(&spec, &["--json"]);
    assert_eq!(out.status.code(), Some(2));
    let diags = json_diags(&out);
    assert_eq!(diags.len(), 1, "one board-load diagnostic: {diags:?}");
    assert_eq!(diags[0]["code"], "board-load");
    // ...and --no-board still rejects a structural error.
    let (_dir2, bad) = spec_dir(
        "noboard-bad",
        "board = \"missing.kicad_pcb\"\nduration_ms = 20\n",
    );
    let out = check(&bad, &["--no-board"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "a spec with no [[assert]] blocks fails even with --no-board"
    );
}

/// `check` must not pass a spec `run` refuses at startup. A `firmware` path that
/// resolves to nothing is exactly that: `check` printed "OK" (exit 0) while
/// `run` on the same file exited 2 with "no firmware file at ...", so the
/// editor-facing validator and the runner disagreed about spec validity.
#[test]
fn a_firmware_path_that_does_not_exist_fails_check_as_it_fails_run() {
    let body = "name = \"check-firmware\"\nboard = \"blinky.kicad_pcb\"\n\
                firmware = \"firmware/nope.hex\"\nduration_ms = 20\n\n\
                [[supply]]\nnet = \"+5V\"\nkind = \"ideal\"\nvolts = 5.0\n\n\
                [[assert]]\nkind = \"voltage\"\nnet = \"+5V\"\nmin = 1.0\n";
    let (_dir, spec) = spec_dir("firmware-missing", body);

    let out = check(&spec, &[]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "a missing firmware must exit 2; stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    // The same sentence `run` prints, so a user who sees one recognises the other.
    assert!(
        stderr.contains("no firmware file at"),
        "check must name the missing image the way run does: {stderr}"
    );
    assert!(
        stderr.contains("firmware/nope.hex"),
        "and quote the spec's own `firmware` value: {stderr}"
    );

    // The machine surface carries its own code, and points at the firmware line
    // (line 3 of the body above).
    let out = check(&spec, &["--json"]);
    assert_eq!(out.status.code(), Some(2));
    let diags = json_diags(&out);
    assert_eq!(diags.len(), 1, "one firmware diagnostic: {diags:?}");
    assert_eq!(diags[0]["code"], "firmware-missing");
    assert_eq!(diags[0]["line"], 3, "points at the firmware line: {diags:?}");

    // --no-board is the documented opt-out for an editor loop where the firmware
    // is not built yet, and it covers the firmware as well as the board.
    let out = check(&spec, &["--no-board"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "--no-board skips the firmware artifact too; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // A spec whose firmware IS on disk still passes, so this is not a blanket
    // "any firmware key fails" regression.
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(blinky_board(), dir.path().join("blinky.kicad_pcb")).unwrap();
    std::fs::write(dir.path().join("real.hex"), ":00000001FF\n").unwrap();
    let ok_spec = dir.path().join("firmware-present.toml");
    std::fs::write(&ok_spec, body.replace("firmware/nope.hex", "real.hex")).unwrap();
    let out = check(&ok_spec, &[]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "a firmware that exists must still check clean; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn a_toml_parse_error_carries_the_exact_line_and_col() {
    // Line 3 redefines `name`: the parser's span points at the duplicate key,
    // line 3, column 1.
    let (_dir, spec) = spec_dir(
        "dupkey",
        "name = \"t\"\nboard = \"blinky.kicad_pcb\"\nname = \"u\"\nduration_ms = 20\n",
    );
    let out = check(&spec, &["--json", "--no-board"]);
    assert_eq!(out.status.code(), Some(2));
    let diags = json_diags(&out);
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0]["code"], "toml-parse");
    assert_eq!(diags[0]["line"], 3, "duplicate key is on line 3: {diags:?}");
    assert_eq!(diags[0]["col"], 1, "at column 1: {diags:?}");
}

#[test]
fn an_unknown_field_gets_its_own_code_and_a_did_you_mean_fix() {
    // `durration_ms` (typo) on line 2: serde's deny_unknown_fields error comes
    // through the TOML parser with a span, and the fix suggests the real field.
    let (_dir, spec) = spec_dir(
        "unkfield",
        "board = \"blinky.kicad_pcb\"\ndurration_ms = 20\n\n\
         [[assert]]\nkind = \"voltage\"\nnet = \"+5V\"\nmin = 1.0\n",
    );
    let out = check(&spec, &["--json", "--no-board"]);
    assert_eq!(out.status.code(), Some(2));
    let diags = json_diags(&out);
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0]["code"], "unknown-field");
    assert_eq!(diags[0]["line"], 2, "typo'd key is on line 2: {diags:?}");
    let fix = diags[0]["fix"].as_str().unwrap_or("");
    assert!(
        fix.contains("duration_ms"),
        "fix suggests the real field: {diags:?}"
    );
}

#[test]
fn a_validation_error_resolves_to_the_identifiers_line() {
    // The undeclared peripheral id "TYPO" appears exactly once, on line 9.
    let body = "board = \"blinky.kicad_pcb\"\n\
                duration_ms = 20\n\
                \n\
                [[peripheral]]\n\
                id = \"EE1\"\n\
                type = \"i2c_eeprom\"\n\
                \n\
                [[assert]]\n\
                id = \"TYPO\"\n\
                kind = \"peripheral\"\n\
                field = \"writes\"\n\
                min = 1\n";
    let (_dir, spec) = spec_dir("identline", body);
    let out = check(&spec, &["--json", "--no-board"]);
    assert_eq!(out.status.code(), Some(2));
    let diags = json_diags(&out);
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0]["code"], "unknown-id");
    assert_eq!(
        diags[0]["line"], 9,
        "the diagnostic points at the line carrying \"TYPO\": {diags:?}"
    );
    let msg = diags[0]["message"].as_str().unwrap();
    assert!(msg.contains("TYPO") && msg.contains("EE1"), "{msg}");
}

#[test]
fn an_unknown_net_is_a_diagnostic_with_a_did_you_mean_fix() {
    // "+5W" is not on the blinky board; "+5V" is. Board-aware path.
    let (_dir, spec) = spec_dir(
        "unknet",
        "board = \"blinky.kicad_pcb\"\nduration_ms = 20\n\n\
         [[assert]]\nkind = \"voltage\"\nnet = \"+5W\"\nmin = 1.0\n",
    );
    let out = check(&spec, &["--json"]);
    assert_eq!(out.status.code(), Some(2));
    let diags = json_diags(&out);
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0]["code"], "unknown-net");
    assert_eq!(diags[0]["line"], 6, "net is on line 6: {diags:?}");
    let fix = diags[0]["fix"].as_str().unwrap_or("");
    assert!(fix.contains("+5V"), "fix suggests the real net: {diags:?}");
}

#[test]
fn every_independent_error_is_reported_in_one_invocation() {
    // E54: three unrelated mistakes; one invocation reports all three, on the
    // plain path, the --json path, AND through `run`'s spec-error path.
    let body = "board = \"blinky.kicad_pcb\"\nduration_ms = 20\n\n\
                [[supply]]\nnet = \"+5V\"\nkind = \"benchh\"\nvolts = 5.0\n\n\
                [[tolerance]]\nref = \"R1\"\npercent = 150\n\n\
                [[assert]]\nkind = \"toggle\"\nnet = \"D1\"\nfreq_hz = 5\nmin_toggles = 1\n";
    let (_dir, spec) = spec_dir("multi", body);

    // Plain path: stderr carries all three findings.
    let out = check(&spec, &["--no-board"]);
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    for needle in ["benchh", "min_toggles", "percent"] {
        assert!(
            stderr.contains(needle),
            "plain check must report `{needle}`: {stderr}"
        );
    }

    // JSON path: three elements with the right codes.
    let out = check(&spec, &["--json", "--no-board"]);
    let diags = json_diags(&out);
    assert_eq!(diags.len(), 3, "three diagnostics: {diags:?}");
    let codes: Vec<&str> = diags.iter().filter_map(|d| d["code"].as_str()).collect();
    assert!(codes.contains(&"unknown-kind"), "{codes:?}");
    assert!(codes.contains(&"conflicting-fields"), "{codes:?}");
    assert!(codes.contains(&"bad-bound"), "{codes:?}");

    // The `run` path loads through the same validation, so its exit-2 error
    // message carries all three too.
    let out = Command::new(bin())
        .arg("run")
        .arg(&spec)
        .output()
        .expect("binary runs");
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    for needle in ["benchh", "min_toggles", "percent"] {
        assert!(
            stderr.contains(needle),
            "run must report `{needle}` in the same invocation: {stderr}"
        );
    }
}

#[test]
fn check_reports_all_unknown_component_refs_at_once() {
    // E54 on the board-aware side: two typo'd component refs surface together.
    let body = "board = \"blinky.kicad_pcb\"\nduration_ms = 20\n\n\
                [[override]]\nref = \"R_NOPE1\"\nvalue = \"10k\"\n\n\
                [[override]]\nref = \"R_NOPE2\"\nvalue = \"4k7\"\n\n\
                [[assert]]\nkind = \"voltage\"\nnet = \"+5V\"\nmin = 1.0\n";
    let (_dir, spec) = spec_dir("refs", body);
    let out = check(&spec, &["--json"]);
    assert_eq!(out.status.code(), Some(2));
    let diags = json_diags(&out);
    assert_eq!(diags.len(), 2, "both bad refs in one pass: {diags:?}");
    assert!(diags.iter().all(|d| d["code"] == "unknown-ref"), "{diags:?}");
    let msgs = diags
        .iter()
        .filter_map(|d| d["message"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(msgs.contains("R_NOPE1") && msgs.contains("R_NOPE2"), "{msgs}");
}
