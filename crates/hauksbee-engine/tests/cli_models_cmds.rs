//! The `hauksbee models` family: `lint`, `list`, `resolve`, `new`, `coverage`,
//! through the compiled binary and the library surface behind them, plus the
//! bundled sensor-spec catalog. All offline.
//!
//! Merged from cli_models_cmds, cli_models_lint, models_resolve_layers and
//! bundled_sensor_catalog.

use std::path::{Path, PathBuf};
use std::process::Command;

use hauksbee_engine::commands::models::{
    model_requirement_refusals, resolve_report, resolve_report_json, ModelRequirement,
};
use hauksbee_extract::{Component, ExtractedBoard};
use hauksbee_models::{ModelLibrary, SourceLayer};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_hauksbee")
}

fn run(args: &[&str]) -> std::process::Output {
    Command::new(bin())
        .args(args)
        // add/remove/list resolve ~/.hauksbee from HOME; point it somewhere
        // disposable so no test can touch the real store.
        .env("HOME", std::env::temp_dir().join("hauksbee_cli_models_home"))
        .output()
        .expect("hauksbee binary runs")
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/logic_lint")
        .join(name)
}

fn pic_programmer() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../frontend/public/boards/pic_programmer.kicad_pcb")
}

fn lint(path: &Path) -> (i32, String) {
    let out = Command::new(bin())
        .args(["models", "lint"])
        .arg(path)
        .output()
        .expect("hauksbee binary runs");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr);
    (out.status.code().unwrap_or(-1), format!("{stdout}{stderr}"))
}

// ── models --help / list ────────────────────────────────────────────────────

#[test]
fn models_help_states_all_six_layers_and_list_runs() {
    let out = run(&["models", "--help"]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    for layer in [
        "builtin=0",
        "pack=10",
        "user-dir=20",
        "user-config-dir=25",
        "--models-dir=30",
        "spice=40",
    ] {
        assert!(text.contains(layer), "layer '{layer}' missing:\n{text}");
    }
    let out = run(&["models", "list", "--builtin"]);
    assert!(
        out.status.success(),
        "models list --builtin exits 0: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("74hc595"),
        "the builtin catalog lists the shipped 595 model"
    );
}

// ── models lint ─────────────────────────────────────────────────────────────

/// One broken fixture per `[models.logic]` validation category, with the named
/// error the lint output must carry; every one exits 2.
#[test]
fn lint_names_every_broken_logic_category() {
    let cases: &[(&str, &str)] = &[
        ("undeclared_pin.toml", "references undeclared name 'phantom_pin'"),
        (
            "width_mismatch.toml",
            "bit index 7 is out of range for register 'reg' (4 bits)",
        ),
        (
            "register_as_scalar.toml",
            "uses register 'reg' (8 bits) as a 1-bit value",
        ),
        (
            "clock_also_comb.toml",
            "clock pin 'clk' of register 'ff' is also referenced combinationally",
        ),
        ("unreachable_output.toml", "unreachable output"),
        ("bad_tristate.toml", "enable pin 'oe_missing' is not a declared input"),
        ("non_converging.toml", "does not converge within 16 fixpoint sweeps"),
    ];
    for (file, needle) in cases {
        let (code, out) = lint(&fixture(file));
        assert_eq!(code, 2, "{file}: lint must exit 2 on a finding; output:\n{out}");
        assert!(out.contains(needle), "{file}: expected {needle:?} in:\n{out}");
    }
}

/// The shipping builtin digital db lints clean through the same compile path
/// binding uses.
#[test]
fn builtin_digital_db_lints_clean() {
    let db = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../hauksbee-models/db/digital.toml");
    let (code, out) = lint(&db);
    assert_eq!(code, 0, "builtin digital.toml must lint clean; output:\n{out}");
    for id in ["74hc595", "74hc165", "74hc125", "74hc27", "74hc02"] {
        assert!(out.contains(&format!("model '{id}': ok")), "'{id}' ok line in:\n{out}");
    }
}

#[test]
fn lint_refuses_non_binding_models_unrecognized_toml_and_boards() {
    let dir = tempfile::tempdir().unwrap();
    let stale = dir.path().join("stale-editor.toml");
    std::fs::write(
        &stale,
        "[[models]]\nid = \"test_r\"\nkind = \"passive\"\n[models.match]\nvalue = [\"^10k$\"]\n",
    )
    .unwrap();
    let (code, out) = lint(&stale);
    assert_eq!(code, 2, "a non-binding model is a lint finding:\n{out}");
    assert!(out.contains("no match rules"), "{out}");

    let neither = dir.path().join("neither.toml");
    std::fs::write(&neither, "[something_else]\nx = 1\n").unwrap();
    let (code, _out) = lint(&neither);
    assert_eq!(code, 1, "unrecognized TOML shape is a hard error");

    // A board file handed to `models lint` names the command they meant.
    let (code, out) = lint(&pic_programmer());
    assert_eq!(code, 1);
    assert!(out.contains("hauksbee models resolve"), "{out}");
    assert!(!out.contains("(kicad_pcb"), "the board must not be dumped as context");
}

// ── models new ──────────────────────────────────────────────────────────────

#[test]
fn scaffold_does_not_infer_model_kind_and_stays_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let scaffold = dir.path().join("u3.toml");
    let board = pic_programmer();
    let made = Command::new(bin())
        .args(["models", "new", "U3", "--board"])
        .arg(&board)
        .arg("--out")
        .arg(&scaffold)
        .output()
        .expect("scaffold command runs");
    assert!(made.status.success(), "{}", String::from_utf8_lossy(&made.stderr));
    let text = std::fs::read_to_string(&scaffold).unwrap();
    assert!(text.contains("kind = \"choose_kind\""), "{text}");
    let (code, out) = lint(&scaffold);
    assert_ne!(code, 0, "the undecided scaffold must not lint green:\n{out}");
    assert!(out.contains("unknown kind 'choose_kind'"), "{out}");

    let explicit = dir.path().join("u3-vreg.toml");
    let made = Command::new(bin())
        .args(["models", "new", "U3", "--board"])
        .arg(&board)
        .args(["--kind", "vreg", "--out"])
        .arg(&explicit)
        .output()
        .expect("explicit scaffold command runs");
    assert!(made.status.success(), "{}", String::from_utf8_lossy(&made.stderr));
    let text = std::fs::read_to_string(&explicit).unwrap();
    assert!(text.contains("kind = \"vreg\""), "{text}");
    let (code, out) = lint(&explicit);
    assert_ne!(code, 0, "a kind without its parameters stays fail-closed:\n{out}");
    assert!(out.contains("missing required param 'vout'"), "{out}");

    // A pack scaffold lands inside the pack, never in the cwd, and refuses to
    // overwrite itself.
    let pack = dir.path().join("acme-pack");
    let made = Command::new(bin())
        .args(["models", "new", "U3", "--board"])
        .arg(&board)
        .arg("--pack-dir")
        .arg(&pack)
        .output()
        .expect("pack scaffold command runs");
    assert!(made.status.success(), "{}", String::from_utf8_lossy(&made.stderr));
    assert!(pack.join("pack.toml").is_file());
    let model = pack.join("models").join("u3_7805.toml");
    assert!(model.is_file(), "model was not created inside pack/models");
    let parsed: toml::Value =
        toml::from_str(&std::fs::read_to_string(&model).unwrap()).expect("valid TOML");
    assert_eq!(
        parsed["models"][0]["source"]["uncertainty"][0]["status"].as_str(),
        Some("unknown")
    );
    assert_eq!(parsed["models"][0]["source"]["tier"].as_str(), Some("user-model"));
    let second = Command::new(bin())
        .args(["models", "new", "U3", "--board"])
        .arg(&board)
        .arg("--pack-dir")
        .arg(&pack)
        .output()
        .expect("second scaffold command runs");
    assert!(!second.status.success(), "overwrite must be refused");
}

// ── models coverage ─────────────────────────────────────────────────────────

#[test]
fn coverage_json_stages_identity_only_partial_and_unresolved_parts() {
    let dir = tempfile::tempdir().unwrap();
    let models_dir = dir.path().join("models-dir");
    std::fs::create_dir_all(&models_dir).unwrap();
    std::fs::write(
        models_dir.join("fixture.toml"),
        r#"
[[models]]
id = "24cxx_identity_fixture"
kind = "digital"
description = "identity only"
[models.match]
value_re = "^24Cxx$"
[models.params]
identity_only = true
warning = "identity and board pins only"
unlocked_by = "a source-bound EEPROM protocol model"
[models.pins]
"1" = "a0"
"2" = "a1"
"3" = "a2"
"4" = "gnd"
"5" = "sda"
"6" = "scl"
"7" = "wp"
"8" = "vcc"

[[models]]
id = "lt1373_partial_fixture"
kind = "vreg"
description = "nominal DC output only"
[models.match]
value_re = "^LT1373$"
[models.params]
vout = 5.0
dropout_v = 1.0
iq_a = 0.0001
[models.pins]
"1" = "vc"
"2" = "fb"
"3" = "fb_n"
"4" = "ss"
"5" = "gnd"
"6" = "sw"
"7" = "vin"
"8" = "vin2"
[models.coverage]
implements = ["nominal_dc_output"]
missing = ["switching_ripple", "current_limit", "soft_start"]
"#,
    )
    .unwrap();
    let board = pic_programmer();
    let coverage = |extra: &[&str]| {
        let mut cmd = Command::new(bin());
        cmd.args(["models", "coverage"])
            .arg(&board)
            .arg("--models-dir")
            .arg(&models_dir)
            .args(extra)
            .arg("--json");
        cmd.output().expect("coverage command runs")
    };

    let out = coverage(&[]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["schema_version"], 4);
    assert_eq!(report["board"]["sha256"].as_str().map(str::len), Some(64));
    assert_eq!(
        report["summary"]["identified"].as_u64().unwrap(),
        report["summary"]["active_connected"].as_u64().unwrap()
            - report["summary"]["unresolved"].as_u64().unwrap()
    );
    let rows = report["components"].as_array().unwrap();
    let row = |reference: &str| {
        rows.iter()
            .find(|row| row["reference"] == reference)
            .unwrap_or_else(|| panic!("missing coverage row {reference}"))
    };
    assert_eq!(row("U1")["stage"], "identity_only");
    assert_eq!(row("U1")["model_id"], "24cxx_identity_fixture");
    assert_eq!(row("U4")["stage"], "executable_partial");
    assert_eq!(row("U4")["implements"][0], "nominal_dc_output");
    assert_eq!(row("U4")["missing"][0], "switching_ripple");
    assert_eq!(row("U5")["stage"], "unresolved");

    let met = coverage(&["--require", "U4:nominal_dc_output"]);
    assert!(met.status.success(), "a declared capability passes");
    let met_report: serde_json::Value = serde_json::from_slice(&met.stdout).unwrap();
    assert_eq!(met_report["requirements"][0]["met"], true);

    let missing = coverage(&["--require", "U4:switching_ripple"]);
    assert!(!missing.status.success(), "a declared-missing capability fails closed");
    let missing_report: serde_json::Value = serde_json::from_slice(&missing.stdout).unwrap();
    assert_eq!(missing_report["requirements"][0]["met"], false);
}

// ── models resolve (library surface) ────────────────────────────────────────

fn comp(reference: &str, value: &str, footprint: &str) -> Component {
    Component {
        reference: reference.to_string(),
        value: value.to_string(),
        lib_id: String::new(),
        footprint: footprint.to_string(),
        position: None,
        layer: String::new(),
        properties: Vec::new(),
        dnp: false,
        pins: Vec::new(),
    }
}

fn resolve_board() -> ExtractedBoard {
    ExtractedBoard {
        name: "layer_test".to_string(),
        nets: Vec::new(),
        components: vec![
            comp("D1", "BAT43", "Diode_THT:D_DO-35_SOD27_P7.62mm_Horizontal"),
            comp("D2", "1N914", "RESOLVETEST_FP:D_0805"),
            comp("U99", "TOTALLY_UNKNOWN_XYZ", ""),
        ],
    }
}

#[test]
fn resolve_report_names_layers_and_origins() {
    let flag_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        flag_dir.path().join("mine.toml"),
        r#"
[[models]]
id = "my_resolve_diode"
kind = "diode"
[models.match]
footprint_re = "RESOLVETEST_FP"
[models.params]
is = 2.5e-9
n = 1.75
rs = 0.6
"#,
    )
    .unwrap();
    let mut lib = ModelLibrary::builtin();
    assert!(lib
        .load_dir_layer(flag_dir.path(), SourceLayer::ModelsDirFlag)
        .is_empty());

    let out = resolve_report(&lib, &resolve_board());
    assert!(
        out.contains(
            "builtin(0) < pack(10) < user-dir(20) < user-config-dir(25) < models-dir(30) \
             < spice(40)"
        ),
        "legend missing:\n{out}"
    );
    let row = |reference: &str| {
        out.lines()
            .find(|l| l.starts_with(&format!("│ {reference} ")))
            .unwrap_or_else(|| panic!("{reference} row missing:\n{out}"))
    };
    let d1 = row("D1");
    assert!(d1.contains("builtin(0)") && d1.contains("curated-library"), "D1 row: {d1}");
    let d2 = row("D2");
    assert!(
        d2.contains("my_resolve_diode") && d2.contains("models-dir(30)") && d2.contains("mine"),
        "D2 row: {d2}"
    );
    assert!(row("U99").contains("UNRESOLVED"), "U99 row: {}", row("U99"));

    let value: serde_json::Value =
        serde_json::from_str(&resolve_report_json(&ModelLibrary::builtin(), &resolve_board()))
            .unwrap();
    let d1 = value["components"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["ref"] == "D1")
        .unwrap();
    assert_eq!(d1["source"]["tier"], "curated-library");
    assert_eq!(d1["source"]["layer"], "builtin");
    assert_eq!(d1["source"]["validation"], "physical-bounds-only");
    assert_eq!(d1["source"]["uncertainty"][0]["status"], "unknown");
}

#[test]
fn accuracy_requirements_refuse_unknown_or_unvalidated_sources_by_reference() {
    let requirements = ModelRequirement {
        minimum_tier: Some(hauksbee_ir::evidence::ModelSourceTier::CuratedLibrary),
        minimum_validation: None,
        require_intervals: true,
    };
    let refusals =
        model_requirement_refusals(&ModelLibrary::builtin(), &resolve_board(), requirements);
    assert!(refusals
        .iter()
        .any(|issue| issue.reference == "D1" && issue.reason.contains("interval")));
    assert!(refusals
        .iter()
        .any(|issue| issue.reference == "U99" && issue.reason.contains("open")));
    let validation_refusals = model_requirement_refusals(
        &ModelLibrary::builtin(),
        &resolve_board(),
        ModelRequirement {
            minimum_tier: None,
            minimum_validation: Some(hauksbee_ir::evidence::ModelValidation::DatasheetCurves),
            require_intervals: false,
        },
    );
    assert!(validation_refusals.iter().any(|issue| {
        issue.reference == "D1" && issue.reason.contains("validation physical-bounds-only")
    }));
}

// ── the bundled sensor-spec catalog ─────────────────────────────────────────

const CATALOG: [(&str, &str); 8] = [
    ("ads1115.toml", include_str!("../../../testdata/sensor-specs/ads1115.toml")),
    (
        "bma423_chip_id.toml",
        include_str!("../../../testdata/sensor-specs/bma423_chip_id.toml"),
    ),
    ("bme280.toml", include_str!("../../../testdata/sensor-specs/bme280.toml")),
    ("icm42605.toml", include_str!("../../../testdata/sensor-specs/icm42605.toml")),
    ("ina219.toml", include_str!("../../../testdata/sensor-specs/ina219.toml")),
    ("lm75.toml", include_str!("../../../testdata/sensor-specs/lm75.toml")),
    ("mcp4728.toml", include_str!("../../../testdata/sensor-specs/mcp4728.toml")),
    ("mpu6050.toml", include_str!("../../../testdata/sensor-specs/mpu6050.toml")),
];

/// Every checked-in sensor spec is catalogued and parses through the same
/// `SensorSpec` path live attachment uses.
#[test]
fn every_checked_in_sensor_behavior_is_catalogued_and_executable() {
    use std::collections::BTreeSet;
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/sensor-specs");
    let on_disk = std::fs::read_dir(&directory)
        .unwrap()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            (path.extension().and_then(|v| v.to_str()) == Some("toml"))
                .then(|| path.file_name().unwrap().to_string_lossy().into_owned())
        })
        .collect::<BTreeSet<_>>();
    let catalogued = CATALOG
        .iter()
        .map(|(name, _)| (*name).to_string())
        .collect::<BTreeSet<_>>();
    assert_eq!(on_disk, catalogued, "a checked-in spec and the catalog drifted");
    for (name, bytes) in CATALOG {
        let spec = hauksbee_models::SensorSpec::from_toml(bytes)
            .unwrap_or_else(|error| panic!("{name} is not executable: {error}"));
        assert!(!spec.sensor().name.trim().is_empty(), "{name} has no name");
    }
}
