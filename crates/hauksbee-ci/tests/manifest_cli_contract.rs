use std::process::Command;

#[test]
fn run_help_publishes_immutable_manifest_emission() {
    let out = Command::new(env!("CARGO_BIN_EXE_hauksbee-ci"))
        .args(["run", "--help"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(help.contains("--emit-manifest <FILE>"), "{help}");
    assert!(help.contains("immutable"), "{help}");
}

#[test]
fn ci_run_emits_spec_and_transitive_input_hashes_without_polluting_json() {
    let dir = tempfile::tempdir().unwrap();
    let board = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/boards/button_pullup.kicad_pcb");
    let spec = dir.path().join("check.toml");
    let manifest = dir.path().join("check.manifest.json");
    std::fs::write(
        &spec,
        format!(
            "name = 'manifest smoke'\nboard = {:?}\nduration_ms = 0.01\n\n[[assert]]\nkind = 'no_faults'\n",
            board.display().to_string()
        ),
    )
    .unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_hauksbee-ci"))
        .args(["run", spec.to_str().unwrap(), "--json", "--emit-manifest"])
        .arg(&manifest)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice::<serde_json::Value>(&out.stdout).expect("CI stdout remains JSON");
    assert!(String::from_utf8_lossy(&out.stderr).contains("immutable run manifest"));

    let doc = hauksbee_engine::run_manifest::RunManifest::read_verified(&manifest).unwrap();
    doc.verify_inputs().unwrap();
    assert_eq!(doc.tool.name, "hauksbee-ci");
    assert!(doc.inputs.iter().any(|input| input.role == "spec[0]"));
    assert!(doc.inputs.iter().any(|input| input.role == "board[0]"));
    assert!(!doc
        .invocation
        .argv
        .iter()
        .any(|arg| arg.contains("emit-manifest")));
    assert_eq!(doc.invocation.options["seed"], serde_json::Value::Null);
}

#[test]
fn ci_embedded_example_manifest_has_no_ephemeral_input_dependency() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("example.manifest.json");
    let out = Command::new(env!("CARGO_BIN_EXE_hauksbee-ci"))
        .args(["run", "--example", "blinky", "--quiet", "--emit-manifest"])
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
        .all(|input| !input.path.contains("hauksbee-ci-example")));
    assert_eq!(doc.invocation.options["example"], "blinky");
}
