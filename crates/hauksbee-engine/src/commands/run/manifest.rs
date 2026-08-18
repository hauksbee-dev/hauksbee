//! Builds the immutable reproduction manifest for a run invocation. It records
//! explicit and discovered inputs, normalized command arguments, environment
//! identity, and retained schematic bytes so later verification can reproduce
//! the exact input contract without rereading mutable sources.

use super::RunConfig;

pub(crate) fn capture_manifest(
    cfg: &RunConfig,
    firmware_source: Option<&std::path::Path>,
    explicit_schematic: Option<&std::path::Path>,
    schematic_ties: Option<&crate::schematic_ties::SchematicTies>,
) -> anyhow::Result<crate::run_manifest::RunManifest> {
    use std::collections::BTreeMap;

    use crate::run_manifest::{
        absolutize_argv_paths, board_sidecar_inputs, implicit_model_inputs, ManifestInput,
        ManifestRequest, ToolIdentity,
    };

    let mut inputs = Vec::new();
    if cfg.example.is_none() {
        inputs.push(ManifestInput::new("board", &cfg.board));
        inputs.extend(board_sidecar_inputs(&cfg.board, "board"));
    }
    for (role, path) in [
        ("bom", cfg.bom.as_deref()),
        ("placement", cfg.placement.as_deref()),
        ("asbuilt", cfg.asbuilt.as_deref()),
        ("models_dir", cfg.models_dir.as_deref()),
    ] {
        if let Some(path) = path {
            inputs.push(ManifestInput::new(role, path));
        }
    }
    // The RESOLVED schematic, not just the explicit CLI option: an auto-discovered
    // sibling contributes exactly as much context as a named one, so a manifest
    // that omitted it would not replay the report it describes. This also keeps the
    // manifest agreeing with the evidence inventory, which hashes the same file.
    //
    if let Some(ties) = schematic_ties {
        inputs.push(ManifestInput::retained_file(
            "schematic",
            &ties.path,
            ties.raw.clone(),
        ));
    }
    if let Some(path) = firmware_source {
        inputs.push(ManifestInput::new("firmware_source", path));
    }
    if let Some(path) = cfg.firmware.as_deref() {
        if firmware_source != Some(path) {
            inputs.push(ManifestInput::new("firmware_resolved", path));
        }
    }
    inputs.extend(implicit_model_inputs());

    let dnp_policy = match cfg.dnp_policy {
        hauksbee_extract::dnp::DnpPolicy::FitExceptLinks => "fit-except-links",
        hauksbee_extract::dnp::DnpPolicy::FitAll => "fit-all",
        hauksbee_extract::dnp::DnpPolicy::Honour => "honour",
    };
    let options = BTreeMap::from([
        ("ac".into(), serde_json::json!(cfg.ac)),
        ("ac_csv".into(), serde_json::json!(cfg.ac_csv)),
        ("ac_loop".into(), serde_json::json!(cfg.ac_loop)),
        ("ac_node".into(), serde_json::json!(cfg.ac_node)),
        ("ambient_c".into(), serde_json::json!(cfg.ambient)),
        ("ampacity".into(), serde_json::json!(cfg.ampacity)),
        ("apply_shorts".into(), serde_json::json!(cfg.apply_shorts)),
        ("bom_columns".into(), serde_json::json!(cfg.bom_columns)),
        ("check".into(), serde_json::json!(cfg.check)),
        ("chunk_us".into(), serde_json::json!(cfg.chunk_us)),
        ("dnp_policy".into(), serde_json::json!(dnp_policy)),
        ("drc".into(), serde_json::json!(cfg.drc)),
        ("example".into(), serde_json::json!(cfg.example)),
        ("fit".into(), serde_json::json!(cfg.fit)),
        ("headless".into(), serde_json::json!(cfg.headless)),
        ("json".into(), serde_json::json!(cfg.json)),
        ("junit".into(), serde_json::json!(cfg.junit)),
        ("lint".into(), serde_json::json!(cfg.lint)),
        ("list_nets".into(), serde_json::json!(cfg.list_nets)),
        ("no_fit".into(), serde_json::json!(cfg.no_fit)),
        ("no_open".into(), serde_json::json!(cfg.no_open)),
        ("open".into(), serde_json::json!(cfg.open)),
        ("oracle".into(), serde_json::json!(cfg.oracle)),
        ("plain".into(), serde_json::json!(cfg.plain)),
        ("port".into(), serde_json::json!(cfg.port)),
        ("probe".into(), serde_json::json!(cfg.probe)),
        ("probe_csv".into(), serde_json::json!(cfg.probe_csv)),
        ("report".into(), serde_json::json!(cfg.report)),
        ("resources".into(), serde_json::json!(cfg.resources)),
        ("sarif".into(), serde_json::json!(cfg.sarif)),
        ("seconds".into(), serde_json::json!(cfg.seconds)),
        ("serial_attach".into(), serde_json::json!(cfg.serial_attach)),
        ("serial_mcu".into(), serde_json::json!(cfg.serial_mcu)),
        (
            "serial_no_pace".into(),
            serde_json::json!(cfg.serial_no_pace),
        ),
        (
            "serial_transport".into(),
            serde_json::json!(cfg.serial_transport.as_str()),
        ),
        ("serial_wait_s".into(), serde_json::json!(cfg.serial_wait)),
        ("serve".into(), serde_json::json!(cfg.serve)),
        ("si".into(), serde_json::json!(cfg.si)),
        ("strict".into(), serde_json::json!(cfg.strict)),
        ("strict_boot".into(), serde_json::json!(cfg.strict_boot)),
        (
            "strict_thermal".into(),
            serde_json::json!(cfg.strict_thermal),
        ),
        (
            "no_strict_thermal".into(),
            serde_json::json!(cfg.no_strict_thermal),
        ),
        ("thermal".into(), serde_json::json!(cfg.thermal)),
        ("tui".into(), serde_json::json!(cfg.tui)),
        ("usb_c".into(), serde_json::json!(cfg.usb_c)),
        ("verbose".into(), serde_json::json!(cfg.verbose)),
    ]);
    let mut features = Vec::new();
    if cfg!(feature = "avr") {
        features.push("avr".to_string());
    }
    if cfg!(feature = "embed-web") {
        features.push("embed-web".to_string());
    }
    if cfg!(feature = "qemu") {
        features.push("qemu".to_string());
    }
    if cfg!(feature = "renode") {
        features.push("renode".to_string());
    }
    let replay_paths = [
        cfg.example.is_none().then(|| cfg.board.clone()),
        cfg.bom.clone(),
        cfg.placement.clone(),
        explicit_schematic.map(std::path::Path::to_path_buf),
        firmware_source.map(std::path::Path::to_path_buf),
        cfg.firmware.clone(),
        cfg.asbuilt.clone(),
        cfg.junit.clone(),
        cfg.sarif.clone(),
        cfg.models_dir.clone(),
        cfg.ac_csv.clone(),
        cfg.probe_csv.clone(),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    let base = std::env::current_dir()?;
    crate::run_manifest::RunManifest::capture(ManifestRequest {
        tool: ToolIdentity::workspace("hauksbee"),
        command: absolutize_argv_paths(cfg.manifest_command.clone(), &base, &replay_paths),
        options,
        inputs,
        feature_flags: features,
    })
}
