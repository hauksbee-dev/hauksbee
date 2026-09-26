//! The spec's `models_dir` key binds (resolved against the spec, replaced by
//! `--models-dir`, a missing directory refused) and `--waveforms` writes the
//! asserted nets' series. Uses the exit3 fixture: a 1k/1k divider on VIN with
//! one custom-model part (`hbtest_divzero`) whose card lives in `models/`.

use std::path::{Path, PathBuf};

use hauksbee_ci::{run, RunConfig};

use crate::support;

fn fixture(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/exit3")
        .join(rel)
}

/// A spec in a fresh directory holding `models/` (the fixture card), with
/// `extra` appended to the top-level keys.
fn spec_with(extra: &str) -> PathBuf {
    let body = format!(
        "name = \"models dir\"\nboard = {}\nduration_ms = 2\nframe_ms = 0.2\n{extra}\n\
         [[supply]]\nnet = \"VIN\"\nkind = \"bench\"\nvolts = 5.0\ncurrent_limit_a = 1.0\n\
         [[assert]]\nkind = \"voltage\"\nnet = \"SENSE\"\nmin = 2.0\nmax = 3.0\n",
        support::toml_path(&fixture("board.net"))
    );
    let spec = support::write_tmp("spec.toml", &body);
    let models = spec.parent().unwrap().join("models");
    std::fs::create_dir_all(&models).unwrap();
    std::fs::copy(fixture("models/hbtest.toml"), models.join("hbtest.toml")).unwrap();
    spec
}

fn binds_card(spec: &Path, cli: Option<PathBuf>) -> Result<bool, String> {
    let cfg = RunConfig {
        spec: spec.to_path_buf(),
        models_dir: cli,
        ..Default::default()
    };
    let result = run(&cfg).map_err(|e| e.to_string())?;
    Ok(result.render_json().contains("hbtest_divzero"))
}

#[test]
fn spec_models_dir_binds_relative_to_the_spec_and_the_flag_replaces_it() {
    assert_eq!(
        binds_card(&spec_with(""), None),
        Ok(false),
        "no key, no card"
    );
    assert_eq!(
        binds_card(&spec_with("models_dir = \"models\""), None),
        Ok(true)
    );
    let missing = spec_with("models_dir = \"nope\"");
    let err = binds_card(&missing, None).unwrap_err();
    assert!(
        err.contains("the spec's models_dir") && err.contains("nope"),
        "{err}"
    );
    let flag = missing.parent().unwrap().join("models");
    assert_eq!(
        binds_card(&missing, Some(flag)),
        Ok(true),
        "--models-dir replaces the key"
    );
}

#[test]
fn waveforms_export_writes_each_asserted_net_per_seed_and_window() {
    let spec = spec_with("");
    let dir = spec.parent().unwrap().join("waves");
    let cfg = RunConfig {
        spec: spec.clone(),
        waveforms: Some(dir.clone()),
        ..Default::default()
    };
    let with = run(&cfg).expect("runs");
    let without = run(&support::run_config(spec)).expect("runs");
    assert_eq!(
        with.results[0].detail, without.results[0].detail,
        "report unchanged"
    );

    let root = dir.join("spec");
    for file in ["seed0/net_SENSE.csv", "seed0/a0_SENSE.csv"] {
        let csv = std::fs::read_to_string(root.join(file)).expect(file);
        let mut lines = csv.lines();
        assert_eq!(lines.next(), Some("time_s,SENSE"), "{file}");
        let rows: Vec<(f64, f64)> = lines
            .map(|l| {
                let (t, v) = l.split_once(',').unwrap();
                (t.parse().unwrap(), v.parse().unwrap())
            })
            .collect();
        assert!(
            rows.len() >= 5,
            "{file}: one row per 0.2 ms frame, got {}",
            rows.len()
        );
        assert!(
            rows.iter().all(|&(_, v)| (v - 2.5).abs() < 0.1),
            "{file}: {rows:?}"
        );
    }
    let index: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(root.join("index.json")).unwrap()).unwrap();
    let files = index["files"].as_array().unwrap();
    assert!(files
        .iter()
        .any(|f| f["assert"] == 0 && f["kind"] == "voltage" && f["window_s"].is_array()));
}
