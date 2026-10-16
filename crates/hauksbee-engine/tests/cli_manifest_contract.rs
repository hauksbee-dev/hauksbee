use crate::support::{assert_ok, board, run, stderr, stdout};

#[test]
fn run_help_publishes_manifest_emission_and_root_help_publishes_replay() {
    let run_out = run(&["run", "--help"]);
    assert!(run_out.status.success());
    let run_help = stdout(&run_out);
    assert!(run_help.contains("--emit-manifest <FILE>"), "{run_help}");
    assert!(run_help.contains("immutable"), "{run_help}");

    let root = run(&["--help"]);
    assert!(root.status.success());
    let root_help = stdout(&root);
    assert!(root_help.contains("reproduce"), "{root_help}");
}

#[test]
fn report_emits_verifiable_no_clobber_manifest_and_replays_it() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("report.manifest.json");
    let manifest_str = manifest.to_str().unwrap();
    let board_path = board("../../testdata/boards/button_pullup.kicad_pcb");
    let board_str = board_path.to_str().unwrap();

    let first = run(&[
        "run",
        board_str,
        "--report",
        "--json",
        "--emit-manifest",
        manifest_str,
    ]);
    assert_ok(&first);
    serde_json::from_slice::<serde_json::Value>(&first.stdout).expect("report stdout remains JSON");
    assert!(stderr(&first).contains("immutable run manifest"));

    let doc = hauksbee_engine::run_manifest::RunManifest::read_verified(&manifest).unwrap();
    doc.verify_inputs().unwrap();
    assert_eq!(doc.tool.name, "hauksbee");
    assert_eq!(doc.invocation.argv[0], "hauksbee");
    assert!(!doc
        .invocation
        .argv
        .iter()
        .any(|arg| arg.contains("emit-manifest")));

    let second = run(&[
        "run",
        board_str,
        "--report",
        "--emit-manifest",
        manifest_str,
    ]);
    assert!(!second.status.success());
    assert!(stderr(&second).contains("refusing to overwrite"));

    let replay = run(&["reproduce", manifest_str]);
    assert_ok(&replay);
}

#[test]
fn embedded_example_manifest_relies_on_the_pinned_tool_not_ephemeral_temp_files() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("example.manifest.json");
    let out = run(&[
        "run",
        "--example",
        "blinky",
        "--report",
        "--emit-manifest",
        manifest.to_str().unwrap(),
    ]);
    assert_ok(&out);
    let doc = hauksbee_engine::run_manifest::RunManifest::read_verified(&manifest).unwrap();
    assert!(doc
        .inputs
        .iter()
        .all(|input| !input.path.contains("hauksbee-example")));
    assert_eq!(doc.invocation.options["example"], "blinky");
}

#[test]
fn invalid_explicit_schematic_aborts_before_manifest_creation() {
    let dir = tempfile::tempdir().unwrap();
    let board_path = dir.path().join("design.brd");
    let schematic = dir.path().join("design.sch");
    let manifest = dir.path().join("run.manifest.json");
    std::fs::write(
        &board_path,
        include_bytes!("../../hauksbee-extract/tests/fixtures/eagle_ties/declared.brd"),
    )
    .unwrap();
    std::fs::write(&schematic, b"<eagle><schematic>").unwrap();

    let output = run(&[
        "run",
        board_path.to_str().unwrap(),
        "--drc",
        "--schematic",
        schematic.to_str().unwrap(),
        "--emit-manifest",
        manifest.to_str().unwrap(),
    ]);
    assert!(!output.status.success());
    assert!(
        !manifest.exists(),
        "invalid companion must not produce replay evidence"
    );
    let err = stderr(&output);
    assert!(err.contains("not an Eagle .sch"), "{err}");
}

/// Run the compiled binary from `dir`, so relative paths resolve against it.
fn run_in(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new(crate::support::bin())
        .args(args)
        .current_dir(dir)
        .output()
        .expect("hauksbee binary runs")
}

fn copy_board_into(dir: &std::path::Path) {
    std::fs::copy(
        board("../../testdata/boards/button_pullup.kicad_pcb"),
        dir.join("board.kicad_pcb"),
    )
    .unwrap();
}

/// Every string under `value` that is an absolute path, or `--flag=` followed by one.
fn collect_absolute_strings(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(text) => {
            let path = text.split_once('=').map_or(text.as_str(), |(_, v)| v);
            if std::path::Path::new(path).is_absolute() {
                out.push(text.clone());
            }
        }
        serde_json::Value::Array(items) => {
            for v in items {
                collect_absolute_strings(v, out);
            }
        }
        serde_json::Value::Object(map) => {
            for v in map.values() {
                collect_absolute_strings(v, out);
            }
        }
        _ => {}
    }
}

#[test]
fn manifest_paths_are_project_relative_and_replay_from_a_copy_elsewhere() {
    let original = tempfile::tempdir().unwrap();
    copy_board_into(original.path());
    let emitted = run_in(
        original.path(),
        &[
            "run",
            "board.kicad_pcb",
            "--report",
            "--json",
            "--emit-manifest",
            "m.json",
        ],
    );
    assert_ok(&emitted);

    let text = std::fs::read_to_string(original.path().join("m.json")).unwrap();
    let doc: serde_json::Value = serde_json::from_str(&text).unwrap();
    let original_root = original.path().canonicalize().unwrap();
    assert!(
        !text.contains(original_root.to_str().unwrap()),
        "the capturing directory must not be in the manifest: {text}"
    );
    assert_eq!(doc["inputs"][0]["path"], "board.kicad_pcb");
    // No recorded argument or option may name an absolute path, however the
    // platform spells the capturing directory (`\\?\` prefix, 8.3 short names).
    let mut absolute = Vec::new();
    collect_absolute_strings(&doc["invocation"], &mut absolute);
    assert!(
        absolute.is_empty(),
        "invocation holds absolute paths {absolute:?}: {text}"
    );
    assert!(doc["root_hint"].as_str().is_some(), "{text}");
    let outcome = &doc["outcome"];
    assert!(
        outcome["verdict"]
            .as_str()
            .is_some_and(|v| v == "pass" || v == "fail"),
        "{text}"
    );
    assert_eq!(outcome["exit_code"], 0);
    assert!(
        outcome
            .get("record_sha256")
            .is_some_and(serde_json::Value::is_null),
        "record_sha256 is null until the run record lands: {text}"
    );

    // A second directory with the same bytes; the first is gone by then.
    let copy = tempfile::tempdir().unwrap();
    copy_board_into(copy.path());
    std::fs::copy(original.path().join("m.json"), copy.path().join("m.json")).unwrap();
    drop(original);
    let replay = run_in(copy.path(), &["reproduce", "m.json"]);
    assert_ok(&replay);
    let err = stderr(&replay);
    assert!(err.contains("MATCH"), "{err}");
    assert!(!err.contains("DIFF"), "{err}");
}

#[test]
fn reproduce_reports_diff_and_exits_nonzero_when_an_input_byte_changes() {
    let dir = tempfile::tempdir().unwrap();
    copy_board_into(dir.path());
    assert_ok(&run_in(
        dir.path(),
        &[
            "run",
            "board.kicad_pcb",
            "--report",
            "--emit-manifest",
            "m.json",
        ],
    ));

    let board_path = dir.path().join("board.kicad_pcb");
    let mut bytes = std::fs::read(&board_path).unwrap();
    bytes.push(b'\n');
    std::fs::write(&board_path, bytes).unwrap();

    let replay = run_in(dir.path(), &["reproduce", "m.json"]);
    assert_eq!(replay.status.code(), Some(1), "{}", stderr(&replay));
    let err = stderr(&replay);
    assert!(err.contains("DIFF"), "{err}");
    assert!(err.contains("digest mismatch"), "{err}");
    assert!(err.contains("board.kicad_pcb"), "{err}");
}

#[test]
fn reproduce_reports_the_verdict_exit_code_and_findings_that_differ() {
    let dir = tempfile::tempdir().unwrap();
    copy_board_into(dir.path());
    assert_ok(&run_in(
        dir.path(),
        &[
            "run",
            "board.kicad_pcb",
            "--report",
            "--emit-manifest",
            "m.json",
        ],
    ));

    // The outcome is not part of the manifest id, so it can be edited without
    // invalidating the manifest: pretend the original run had failed.
    let path = dir.path().join("m.json");
    let mut doc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let recorded_verdict = doc["outcome"]["verdict"].as_str().unwrap().to_string();
    doc["outcome"]["verdict"] = "invalid".into();
    doc["outcome"]["exit_code"] = 3.into();
    doc["outcome"]["gating_findings"] =
        serde_json::json!(["lint/unrouted_pad/serious/111111111111"]);
    std::fs::write(&path, serde_json::to_vec_pretty(&doc).unwrap()).unwrap();

    let replay = run_in(dir.path(), &["reproduce", "m.json"]);
    assert_eq!(replay.status.code(), Some(1), "{}", stderr(&replay));
    let err = stderr(&replay);
    assert!(err.contains("DIFF"), "{err}");
    assert!(
        err.contains(&format!("verdict: invalid -> {recorded_verdict}")),
        "{err}"
    );
    assert!(err.contains("exit code: 3 -> 0"), "{err}");
    assert!(
        err.contains("gating finding gone: lint/unrouted_pad/serious/111111111111"),
        "{err}"
    );
}
