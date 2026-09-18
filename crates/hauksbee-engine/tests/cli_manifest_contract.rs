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
