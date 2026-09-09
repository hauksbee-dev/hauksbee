use std::collections::BTreeMap;
use std::fs;

use hauksbee_engine::run_manifest::{
    absolutize_argv_paths, board_sidecar_inputs, ManifestInput, ManifestRequest, RunManifest,
    ToolIdentity, MANIFEST_SCHEMA_VERSION,
};

#[test]
fn retained_file_bytes_are_hashed_instead_of_a_second_filesystem_read() {
    let dir = tempfile::tempdir().unwrap();
    let schematic = dir.path().join("design.sch");
    std::fs::write(&schematic, b"first bytes").unwrap();
    let retained = ManifestInput::retained_file("schematic", &schematic, b"first bytes".to_vec());
    std::fs::write(&schematic, b"changed after resolution").unwrap();

    let models = dir.path().join("models");
    std::fs::create_dir(&models).unwrap();
    let mut request = request(&schematic, &models);
    request.inputs = vec![retained];
    let manifest = RunManifest::capture(request).unwrap();
    assert_eq!(manifest.inputs[0].size_bytes, b"first bytes".len() as u64);
    assert!(
        manifest.verify_inputs().is_err(),
        "verification must compare retained capture bytes with the current file"
    );
}

fn request(board: &std::path::Path, models: &std::path::Path) -> ManifestRequest {
    ManifestRequest {
        tool: ToolIdentity::new("hauksbee", "0.1.0", Some("0123456789ab")),
        command: vec![
            "hauksbee".into(),
            "run".into(),
            board.display().to_string(),
            "--report".into(),
        ],
        options: BTreeMap::from([
            ("mode".into(), serde_json::json!("report")),
            ("strict".into(), serde_json::json!(false)),
        ]),
        inputs: vec![
            ManifestInput::new("model_directory", models),
            ManifestInput::new("board", board),
        ],
        feature_flags: vec!["qemu".into(), "renode".into()],
    }
}

#[test]
fn relative_path_arguments_are_made_independent_of_the_replay_cwd() {
    let base = std::path::Path::new("/project");
    let argv = vec![
        "hauksbee".into(),
        "run".into(),
        "boards/main.kicad_pcb".into(),
        "--models-dir=models".into(),
        "--report".into(),
    ];
    let paths = [
        std::path::PathBuf::from("boards/main.kicad_pcb"),
        std::path::PathBuf::from("models"),
    ];
    let got = absolutize_argv_paths(argv, base, &paths);
    // The joined form carries the host separator, so the expectation is
    // built the same way rather than spelled as a POSIX literal.
    let board = base.join("boards/main.kicad_pcb").display().to_string();
    let models = base.join("models").display().to_string();
    assert_eq!(got[2], board);
    assert_eq!(got[3], format!("--models-dir={models}"));
    assert_eq!(got[4], "--report");
}

#[test]
fn board_sidecar_discovery_includes_only_existing_semantic_inputs() {
    let dir = tempfile::tempdir().unwrap();
    let board = dir.path().join("controller.kicad_pcb");
    fs::write(&board, "board\n").unwrap();
    fs::write(dir.path().join("controller.kicad_pro"), "{}\n").unwrap();
    fs::write(dir.path().join("hauksbee-waivers.toml"), "[[waive]]\n").unwrap();
    fs::write(dir.path().join("unrelated.txt"), "not consumed\n").unwrap();

    let sidecars = board_sidecar_inputs(&board, "board");
    let manifest = RunManifest::capture(ManifestRequest {
        tool: ToolIdentity::new("hauksbee", "0.1.0", Some("0123456789ab")),
        command: vec!["hauksbee".into(), "run".into(), board.display().to_string()],
        options: BTreeMap::new(),
        inputs: sidecars,
        feature_flags: Vec::new(),
    })
    .unwrap();
    let roles = manifest
        .inputs
        .iter()
        .map(|input| input.role.as_str())
        .collect::<Vec<_>>();
    assert_eq!(roles, ["board.kicad_project", "board.waivers"]);
}

#[test]
fn canonical_manifest_records_the_complete_reproduction_contract() {
    let dir = tempfile::tempdir().unwrap();
    let board = dir.path().join("board.kicad_pcb");
    let models = dir.path().join("models");
    fs::write(&board, "board bytes\n").unwrap();
    fs::create_dir(&models).unwrap();
    fs::write(models.join("z.toml"), "z = 1\n").unwrap();
    fs::write(models.join("a.toml"), "a = 1\n").unwrap();

    let manifest = RunManifest::capture(request(&board, &models)).unwrap();
    let json = manifest.canonical_json().unwrap();
    let decoded: serde_json::Value = serde_json::from_str(&json).unwrap();

    assert_eq!(decoded["schema_version"], MANIFEST_SCHEMA_VERSION);
    assert_eq!(decoded["tool"]["name"], "hauksbee");
    assert_eq!(decoded["tool"]["git_revision"], "0123456789ab");
    assert_eq!(decoded["components"]["solver"], env!("CARGO_PKG_VERSION"));
    assert_eq!(decoded["components"]["models"], env!("CARGO_PKG_VERSION"));
    assert!(decoded["plugins"].is_array());
    assert_eq!(decoded["build"]["target_os"], std::env::consts::OS);
    assert_eq!(decoded["build"]["target_arch"], std::env::consts::ARCH);
    assert_eq!(
        decoded["build"]["features"],
        serde_json::json!(["qemu", "renode"])
    );
    assert_eq!(decoded["invocation"]["argv"][1], "run");
    assert_eq!(decoded["invocation"]["options"]["mode"], "report");
    assert_eq!(decoded["reproduce"], "hauksbee reproduce <manifest.json>");
    assert_eq!(decoded["environment"]["value_policy"], "sha256_only");
    assert!(decoded["environment"]["selectors"].is_array());
    assert!(manifest.manifest_id.starts_with("sha256:"));
    assert_eq!(manifest.manifest_id.len(), "sha256:".len() + 64);

    let inputs = decoded["inputs"].as_array().unwrap();
    assert_eq!(inputs.len(), 2);
    assert_eq!(inputs[0]["role"], "board", "inputs are canonically sorted");
    for input in inputs {
        assert_eq!(input["digest"]["algorithm"], "sha256");
        assert_eq!(input["digest"]["value"].as_str().unwrap().len(), 64);
        assert!(input["size_bytes"].as_u64().unwrap() > 0);
    }

    // No timestamps, usernames, cwd, host names, PATH, API keys, or other
    // ambient process noise belong in a reproducibility identity.
    for forbidden in [
        "created_at",
        "timestamp",
        "hostname",
        "username",
        "cwd",
        "PATH",
        "API_KEY",
        "TOKEN",
        "SECRET",
    ] {
        assert!(
            !json.contains(forbidden),
            "ambient/private field leaked: {forbidden}"
        );
    }
}

#[test]
fn serialization_and_directory_hashing_are_stable() {
    let dir = tempfile::tempdir().unwrap();
    let board = dir.path().join("board.kicad_pcb");
    let models = dir.path().join("models");
    fs::write(&board, "same\n").unwrap();
    fs::create_dir(&models).unwrap();
    fs::write(models.join("b.toml"), "b = 2\n").unwrap();
    fs::write(models.join("a.toml"), "a = 1\n").unwrap();

    let first = RunManifest::capture(request(&board, &models)).unwrap();
    let second = RunManifest::capture(request(&board, &models)).unwrap();
    assert_eq!(first.manifest_id, second.manifest_id);
    assert_eq!(
        first.canonical_json().unwrap(),
        second.canonical_json().unwrap()
    );

    fs::write(models.join("a.toml"), "a = 3\n").unwrap();
    let changed = RunManifest::capture(request(&board, &models)).unwrap();
    assert_ne!(first.manifest_id, changed.manifest_id);
}

#[test]
fn emission_is_no_clobber_and_verification_detects_tampering() {
    let dir = tempfile::tempdir().unwrap();
    let board = dir.path().join("board.kicad_pcb");
    let models = dir.path().join("models");
    let output = dir.path().join("run.manifest.json");
    fs::write(&board, "original\n").unwrap();
    fs::create_dir(&models).unwrap();
    fs::write(models.join("model.toml"), "kind = 'r'\n").unwrap();

    let manifest = RunManifest::capture(request(&board, &models)).unwrap();
    manifest.write_new(&output).unwrap();
    let original = fs::read(&output).unwrap();
    assert!(manifest
        .write_new(&output)
        .unwrap_err()
        .to_string()
        .contains("already exists"));
    assert_eq!(
        fs::read(&output).unwrap(),
        original,
        "existing evidence was not changed"
    );

    let loaded = RunManifest::read_verified(&output).unwrap();
    loaded.verify_inputs().unwrap();

    fs::write(&board, "changed\n").unwrap();
    let error = loaded.verify_inputs().unwrap_err().to_string();
    assert!(error.contains("board"));
    assert!(error.contains("digest mismatch"));

    let mut value: serde_json::Value = serde_json::from_slice(&original).unwrap();
    value["invocation"]["options"]["strict"] = serde_json::json!(true);
    fs::write(&output, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    assert!(RunManifest::read_verified(&output)
        .unwrap_err()
        .to_string()
        .contains("manifest_id mismatch"));
}

#[test]
fn manifest_cannot_be_written_inside_a_hashed_input_directory() {
    let dir = tempfile::tempdir().unwrap();
    let board = dir.path().join("board.kicad_pcb");
    let models = dir.path().join("models");
    fs::write(&board, "board\n").unwrap();
    fs::create_dir(&models).unwrap();
    fs::write(models.join("model.toml"), "kind = 'r'\n").unwrap();
    let manifest = RunManifest::capture(request(&board, &models)).unwrap();

    let error = manifest
        .write_new(&models.join("run.manifest.json"))
        .unwrap_err()
        .to_string();
    assert!(error.contains("inside hashed input directory"), "{error}");
    assert!(!models.join("run.manifest.json").exists());
}
