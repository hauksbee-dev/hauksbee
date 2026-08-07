use std::process::Command;

#[test]
fn run_help_publishes_manifest_emission_and_root_help_publishes_replay() {
    let bin = env!("CARGO_BIN_EXE_hauksbee");
    let run = Command::new(bin).args(["run", "--help"]).output().unwrap();
    assert!(run.status.success());
    let run_help = String::from_utf8_lossy(&run.stdout);
    assert!(run_help.contains("--emit-manifest <FILE>"), "{run_help}");
    assert!(run_help.contains("immutable"), "{run_help}");

    let root = Command::new(bin).arg("--help").output().unwrap();
    assert!(root.status.success());
    let root_help = String::from_utf8_lossy(&root.stdout);
    assert!(root_help.contains("reproduce"), "{root_help}");
}

#[test]
fn report_emits_verifiable_no_clobber_manifest_and_replays_it() {
    let bin = env!("CARGO_BIN_EXE_hauksbee");
    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("report.manifest.json");
    let board = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/boards/button_pullup.kicad_pcb");

    let first = Command::new(bin)
        .args([
            "run",
            board.to_str().unwrap(),
            "--report",
            "--json",
            "--emit-manifest",
        ])
        .arg(&manifest)
        .output()
        .unwrap();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    serde_json::from_slice::<serde_json::Value>(&first.stdout).expect("report stdout remains JSON");
    assert!(String::from_utf8_lossy(&first.stderr).contains("immutable run manifest"));

    let doc = hauksbee_engine::run_manifest::RunManifest::read_verified(&manifest).unwrap();
    doc.verify_inputs().unwrap();
    assert_eq!(doc.tool.name, "hauksbee");
    assert_eq!(doc.invocation.argv[0], "hauksbee");
    assert!(!doc
        .invocation
        .argv
        .iter()
        .any(|arg| arg.contains("emit-manifest")));

    let second = Command::new(bin)
        .args([
            "run",
            board.to_str().unwrap(),
            "--report",
            "--emit-manifest",
        ])
        .arg(&manifest)
        .output()
        .unwrap();
    assert!(!second.status.success());
    assert!(String::from_utf8_lossy(&second.stderr).contains("refusing to overwrite"));

    let replay = Command::new(bin)
        .args(["reproduce", manifest.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        replay.status.success(),
        "{}",
        String::from_utf8_lossy(&replay.stderr)
    );
}

#[test]
fn embedded_example_manifest_relies_on_the_pinned_tool_not_ephemeral_temp_files() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("example.manifest.json");
    let out = Command::new(env!("CARGO_BIN_EXE_hauksbee"))
        .args(["run", "--example", "blinky", "--report", "--emit-manifest"])
        .arg(&manifest)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let doc = hauksbee_engine::run_manifest::RunManifest::read_verified(&manifest).unwrap();
    assert!(doc
        .inputs
        .iter()
        .all(|input| !input.path.contains("hauksbee-example")));
    assert_eq!(doc.invocation.options["example"], "blinky");
}
