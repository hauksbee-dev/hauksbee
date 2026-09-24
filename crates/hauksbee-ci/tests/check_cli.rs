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

use crate::support::{bin, blinky_board};
use std::path::PathBuf;
use std::process::Command;

/// Write `body` to `<tag>.toml` in a fresh temp dir (with a copy of the
/// blinky board beside it) and return (dir, spec path).
fn spec_dir(tag: &str, body: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(blinky_board(), dir.path().join("blinky.kicad_pcb")).unwrap();
    let spec = dir.path().join(format!("{tag}.toml"));
    std::fs::write(&spec, body).unwrap();
    (dir, spec)
}

fn cli(sub: &str, spec: &std::path::Path, extra: &[&str]) -> std::process::Output {
    let out = Command::new(bin()).arg(sub).arg(spec).args(extra).output();
    out.expect("binary runs")
}

fn check(spec: &std::path::Path, extra: &[&str]) -> std::process::Output {
    cli("check", spec, extra)
}

/// `refused` for a spec whose one impossible window must come back `bad-bound`
/// naming every `needles` word; `ok` is the near-miss twin that must pass.
fn refused_bad_bound(tag: &str, body: &str, needles: &[&str], ok: &str) {
    let diags = refused(tag, body, &[]);
    let names_all = |m: &str| needles.iter().all(|n| m.contains(n));
    assert!(
        diags
            .iter()
            .any(|d| d["code"] == "bad-bound" && d["message"].as_str().is_some_and(names_all)),
        "{diags:?}"
    );
    accepted(&format!("{tag}-ok"), ok);
}

fn json_diags(out: &std::process::Output) -> Vec<serde_json::Value> {
    let stdout = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str::<Vec<serde_json::Value>>(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout must be one JSON array ({e}): {stdout}"))
}

/// `check --json` over a fresh spec that must be REFUSED: asserts the
/// documented exit 2 and returns the diagnostics array. Every case below starts
/// this way, so the exit code is checked once rather than per case.
fn refused(tag: &str, body: &str, extra: &[&str]) -> Vec<serde_json::Value> {
    let (_dir, spec) = spec_dir(tag, body);
    let mut args = vec!["--json"];
    args.extend_from_slice(extra);
    let out = check(&spec, &args);
    assert_eq!(out.status.code(), Some(2), "{tag}: check must exit 2");
    json_diags(&out)
}

/// The ONE diagnostic a case expects, with its `code` already checked: a second
/// diagnostic would mean the run found something the case is not about.
fn one_refusal(tag: &str, body: &str, extra: &[&str], code: &str) -> serde_json::Value {
    let diags = refused(tag, body, extra);
    assert_eq!(diags.len(), 1, "{tag}: exactly one diagnostic: {diags:?}");
    assert_eq!(diags[0]["code"], code, "{diags:?}");
    diags[0].clone()
}

/// The near-miss twin of a refused case: the same spec, one field inside the
/// run, must still check clean.
fn accepted(tag: &str, body: &str) {
    let (_dir, spec) = spec_dir(tag, body);
    assert_eq!(check(&spec, &["--json"]).status.code(), Some(0), "{tag}");
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
    let body = "board = \"missing.kicad_pcb\"\nduration_ms = 20\n\n\
                [[assert]]\nkind = \"voltage\"\nnet = \"+5V\"\nmin = 1.0\n";
    let (_dir, spec) = spec_dir("noboard", body);
    let out = check(&spec, &["--no-board"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "--no-board must not require the board; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // ...while the default (board-loading) mode reports it as board-load.
    let diags = refused("noboard-load", body, &[]);
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
    let diag = one_refusal("firmware-json", body, &[], "firmware-missing");
    assert_eq!(diag["line"], 3, "points at the firmware line: {diag:?}");

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
    let (dir, ok_spec) = spec_dir(
        "firmware-present",
        &body.replace("firmware/nope.hex", "real.hex"),
    );
    std::fs::write(dir.path().join("real.hex"), ":00000001FF\n").unwrap();
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
    let diag = one_refusal(
        "dupkey",
        "name = \"t\"\nboard = \"blinky.kicad_pcb\"\nname = \"u\"\nduration_ms = 20\n",
        &["--no-board"],
        "toml-parse",
    );
    assert_eq!(diag["line"], 3, "duplicate key is on line 3: {diag:?}");
    assert_eq!(diag["col"], 1, "at column 1: {diag:?}");
}

#[test]
fn an_unknown_field_gets_its_own_code_and_a_did_you_mean_fix() {
    // `durration_ms` (typo) on line 2: serde's deny_unknown_fields error comes
    // through the TOML parser with a span, and the fix suggests the real field.
    let diag = one_refusal(
        "unkfield",
        "board = \"blinky.kicad_pcb\"\ndurration_ms = 20\n\n\
         [[assert]]\nkind = \"voltage\"\nnet = \"+5V\"\nmin = 1.0\n",
        &["--no-board"],
        "unknown-field",
    );
    assert_eq!(diag["line"], 2, "typo'd key is on line 2: {diag:?}");
    assert!(
        diag["fix"].as_str().unwrap_or("").contains("duration_ms"),
        "fix suggests the real field: {diag:?}"
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
    let diag = one_refusal("identline", body, &["--no-board"], "unknown-id");
    assert_eq!(
        diag["line"], 9,
        "the diagnostic points at the line carrying \"TYPO\": {diag:?}"
    );
    let msg = diag["message"].as_str().unwrap();
    assert!(msg.contains("TYPO") && msg.contains("EE1"), "{msg}");
}

#[test]
fn an_unknown_net_is_a_diagnostic_with_a_did_you_mean_fix() {
    // "+5W" is not on the blinky board; "+5V" is. Board-aware path.
    let diag = one_refusal(
        "unknet",
        "board = \"blinky.kicad_pcb\"\nduration_ms = 20\n\n\
         [[assert]]\nkind = \"voltage\"\nnet = \"+5W\"\nmin = 1.0\n",
        &[],
        "unknown-net",
    );
    assert_eq!(diag["line"], 6, "net is on line 6: {diag:?}");
    let fix = diag["fix"].as_str().unwrap_or("");
    assert!(fix.contains("+5V"), "fix suggests the real net: {diag:?}");
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
    let out = cli("run", &spec, &[]);
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
    let diags = refused("refs", body, &[]);
    assert_eq!(diags.len(), 2, "both bad refs in one pass: {diags:?}");
    assert!(
        diags.iter().all(|d| d["code"] == "unknown-ref"),
        "{diags:?}"
    );
    let msgs = diags
        .iter()
        .filter_map(|d| d["message"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        msgs.contains("R_NOPE1") && msgs.contains("R_NOPE2"),
        "{msgs}"
    );
}

// M2: a window that cannot overlap the run is a spec mistake `check` used to
// wave through, leaving the RUNTIME to report it as a degenerate failure after
// a full co-simulation. Every field below is individually in bounds; it is the
// relationship to `duration_ms` that is impossible.
#[test]
fn check_rejects_a_sample_window_that_starts_after_the_run_ends() {
    let body = "board = \"blinky.kicad_pcb\"\nduration_ms = 500\n\n\
                [[assert]]\nkind = \"voltage\"\nnet = \"+5V\"\nmin = 1.0\nafter_ms = 500\n";
    let diag = one_refusal("after", body, &[], "bad-bound");
    let msg = diag["message"].as_str().unwrap();
    for needle in ["after_ms", "duration_ms", "nothing would ever be measured"] {
        assert!(msg.contains(needle), "{msg}");
    }

    // One millisecond inside the run is legitimate and must stay accepted.
    accepted(
        "after-ok",
        &body.replace("after_ms = 500", "after_ms = 499"),
    );
}

#[test]
fn check_rejects_a_deadline_past_the_end_of_the_run() {
    let body = "board = \"blinky.kicad_pcb\"\nduration_ms = 100\n\n\
                [[assert]]\nkind = \"toggle\"\nnet = \"D13\"\nmin_toggles = 3\n\
                deadline_ms = 250\n";
    let ok = body.replace("deadline_ms = 250", "deadline_ms = 100");
    refused_bad_bound("deadline", body, &["deadline_ms", "duration_ms"], &ok);
}

#[test]
fn check_rejects_a_scenario_that_starts_after_the_run_ends() {
    let body = "board = \"blinky.kicad_pcb\"\nduration_ms = 50\n\n\
                [[scenario]]\nid = \"late\"\npart = \"R1\"\nprofile = \"short\"\n\
                start_ms = 50\n\n\
                [[assert]]\nkind = \"voltage\"\nnet = \"+5V\"\nmin = 1.0\n";
    let ok = body.replace("start_ms = 50", "start_ms = 10");
    refused_bad_bound("scenario", body, &["start_ms", "never fire"], &ok);
}
