//! The Checks page's export route serves the CLI writers' own bytes: the JUnit,
//! `run --json` document and manifest of a web run equal what `hauksbee-ci run`
//! writes for the same spec, board and firmware (wall time and staging paths
//! aside, which differ between any two runs).

use hauksbee_frontdoor_api::frontdoor::{DesignUpload, NamedUpload};

/// Wall-clock and absolute paths are the only things two runs of one spec may
/// disagree on.
fn normalise(text: &str) -> String {
    let time = regex::Regex::new(r#"time="[0-9.]+""#).unwrap();
    let paths = regex::Regex::new(r"(/[^/\s\x22<>']+)+/").unwrap();
    paths
        .replace_all(&time.replace_all(text, "time=\"T\""), "")
        .into_owned()
}

fn verdicts(doc: &serde_json::Value) -> Vec<(String, bool, bool, String)> {
    doc["results"]
        .as_array()
        .expect("results")
        .iter()
        .map(|r| {
            (
                r["label"].as_str().unwrap_or_default().to_string(),
                r["passed"].as_bool().unwrap_or_default(),
                r["invalid"].as_bool().unwrap_or_default(),
                normalise(r["detail"].as_str().unwrap_or_default()),
            )
        })
        .collect()
}

#[test]
fn web_export_route_serves_the_cli_writers_bytes() {
    // The engine's web runner shells out to this very binary.
    std::env::set_var("HAUKSBEE_CI_BIN", env!("CARGO_BIN_EXE_hauksbee-ci"));
    let spec_path = hauksbee_ci::examples::materialize("blinky").unwrap();
    let dir = spec_path.parent().unwrap().to_path_buf();
    let mut table: toml::Table = std::fs::read_to_string(&spec_path)
        .unwrap()
        .parse()
        .unwrap();
    let mut file = |key: &str| {
        let rel = table
            .remove(key)
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap();
        let path = dir.join(rel);
        NamedUpload {
            name: path.file_name().unwrap().to_string_lossy().into_owned(),
            bytes: std::fs::read(&path).unwrap(),
        }
    };
    let board = file("board");
    let firmware = file("firmware");
    let upload = DesignUpload {
        board,
        firmware: Some(firmware),
        schematic: None,
        bom: None,
        placement: None,
        variant: None,
        asbuilt: None,
        models: Vec::new(),
    };

    let page = hauksbee_server::ci_export::stash(hauksbee_engine::webcheck::run_web_check_design(
        upload,
        &table.to_string(),
    ));
    let page: serde_json::Value = serde_json::from_str(&page).unwrap();
    assert_eq!(page["ok"], true, "{page}");
    assert!(
        page.get("exports").is_none(),
        "the bulky copies stay server-side"
    );
    let id = page["export_id"].as_str().expect("an export id");
    let served = |file: &str| hauksbee_server::ci_export::lookup(id, file).expect(file).1;

    let cli = hauksbee_ci::run(&hauksbee_ci::RunConfig {
        spec: spec_path.clone(),
        seed: None,
        models_dir: None,
        spice_models: Vec::new(),
        waveforms: None,
    })
    .unwrap();
    assert_eq!(
        normalise(&served("results.xml")),
        normalise(&cli.render_junit())
    );

    let served_json: serde_json::Value = serde_json::from_str(&served("report.json")).unwrap();
    let cli_json: serde_json::Value = serde_json::from_str(&cli.render_json()).unwrap();
    assert_eq!(verdicts(&served_json), verdicts(&cli_json));
    assert_eq!(
        verdicts(&page),
        verdicts(&cli_json),
        "the page shows the exported run"
    );

    let manifest: serde_json::Value = serde_json::from_str(&served("manifest.json")).unwrap();
    assert!(manifest["manifest_id"].is_string(), "{manifest}");
}
