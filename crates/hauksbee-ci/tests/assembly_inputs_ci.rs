//! Production acceptance for BOM, placement and explicit assembly variants in
//! `hauksbee-ci`. These enter through the public runner and the CLI validator:
//! reader-only unit tests would not prove that the assembled identity and exact
//! artifact bytes reach the gate's evidence inventory.

use std::path::{Path, PathBuf};
use std::process::Command;

use hauksbee_ci::{run, RunConfig};

const BOARD: &str = r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "VCC")
  (module Resistor_SMD:R_0402_1005Metric (layer F.Cu)
    (at 10 20)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 2 "VCC"))
    (pad 2 smd rect (at 1 0) (net 1 "GND")))
  (module Resistor_SMD:R_0402_1005Metric (layer F.Cu)
    (at 12 20)
    (fp_text reference R2 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 2 "VCC"))
    (pad 2 smd rect (at 1 0) (net 1 "GND")))
  (module Package_SO:SOIC-8_3.9x4.9mm_P1.27mm (layer F.Cu)
    (at 15 20)
    (fp_text reference U4 (at 0 0) (layer F.SilkS))
    (fp_text value 25LC256 (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 2 "VCC"))
    (pad 4 smd rect (at 1 0) (net 1 "GND"))))"#;

const BOM: &str = "Designator,Value,MPN,Manufacturer,Footprint\n\
                   R1,10k,RC0402FR-0710KL,Yageo,R_0402_1005Metric\n\
                   R2,10k,RC0402FR-0710KL,Yageo,R_0402_1005Metric\n";

const PLACEMENT: &str = "Designator,Val,Package,Mid X,Mid Y,Rotation,Layer\n\
                         R1,10k,R_0402_1005Metric,10,20,0,top\n\
                         R2,10k,R_0402_1005Metric,12,20,0,top\n";

const VARIANT: &str = "name = \"sensorless\"\nno_fit = [\"R2\"]\n";

fn write_fixture(dir: &Path) -> PathBuf {
    std::fs::write(dir.join("board.kicad_pcb"), BOARD).unwrap();
    std::fs::write(dir.join("bom.csv"), BOM).unwrap();
    std::fs::write(dir.join("positions.csv"), PLACEMENT).unwrap();
    std::fs::write(dir.join("sensorless.variant.toml"), VARIANT).unwrap();
    let spec = dir.join("check.toml");
    std::fs::write(
        &spec,
        r#"name = "assembly input acceptance"
board = "board.kicad_pcb"
bom = "bom.csv"
placement = "positions.csv"
variant = "sensorless.variant.toml"
duration_ms = 0.01

[[assert]]
kind = "no_faults"
"#,
    )
    .unwrap();
    spec
}

fn artifact<'a>(json: &'a serde_json::Value, role: &str) -> &'a serde_json::Value {
    json["inventory"]
        .as_array()
        .expect("typed run inventory")
        .iter()
        .find(|item| item["role"] == role)
        .unwrap_or_else(|| panic!("missing {role} artifact: {}", json["inventory"]))
}

#[test]
fn bom_placement_and_variant_shape_the_gate_and_its_exact_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let spec = write_fixture(dir.path());
    let result = run(&RunConfig {
        spec,
        ..Default::default()
    })
    .expect("the production runner accepts the reconciled assembly inputs");
    assert_eq!(result.exit_code(), 0, "{}", result.render_human());

    let json: serde_json::Value =
        serde_json::from_str(&result.render_json()).expect("CI JSON parses");
    for role in ["layout", "spec", "bom", "placement", "variant"] {
        let item = artifact(&json, role);
        assert_eq!(item["sha256"].as_str().map(str::len), Some(64), "{item}");
    }

    let bom = artifact(&json, "bom");
    assert!(
        bom["contributed"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| {
                item["what"] == "part identity"
                    && item["detail"]
                        .as_str()
                        .is_some_and(|line| line.contains("manufacturer part number"))
            })),
        "BOM reconciliation must be evidence, not an unrecorded pre-pass: {bom}"
    );
    let placement = artifact(&json, "placement");
    assert!(
        placement["contributed"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| {
                item["what"] == "assembly placement"
                    && item["detail"]
                        .as_str()
                        .is_some_and(|line| line.contains("2 parts placed"))
            })),
        "placement reconciliation must be evidence: {placement}"
    );
    let variant = artifact(&json, "variant");
    assert_eq!(variant["kind"], "toml");
    assert!(
        variant["contributed"][0]["detail"]
            .as_str()
            .is_some_and(|detail| {
                detail.contains("sensorless") && detail.contains("left open [R2]")
            }),
        "the exact population decision must be reviewable: {variant}"
    );

    let evidence_artifacts = json["results"][0]["evidence"]["artifacts"]
        .as_array()
        .expect("assertion evidence cites artifacts");
    assert!(
        evidence_artifacts.len() >= 5,
        "the gate must cite every assembly input on its causal path: {}",
        json["results"][0]["evidence"]
    );
}

#[test]
fn check_and_run_both_refuse_the_same_ambiguous_bom() {
    let dir = tempfile::tempdir().unwrap();
    let spec = write_fixture(dir.path());
    std::fs::write(
        dir.path().join("bom.csv"),
        "Designator,Value,Manufacturer,Manufacturer Name\nR1,10k,Yageo,Other\n",
    )
    .unwrap();

    for subcommand in ["check", "run"] {
        let output = Command::new(env!("CARGO_BIN_EXE_hauksbee-ci"))
            .arg(subcommand)
            .arg(&spec)
            .output()
            .expect("hauksbee-ci runs");
        assert_eq!(
            output.status.code(),
            Some(2),
            "{subcommand} must refuse; stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let diagnostic = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            diagnostic.contains("two columns that could be the manufacturer"),
            "{subcommand} must name the same reader refusal: {diagnostic}"
        );
    }
}

#[test]
fn ci_refuses_a_bom_whose_part_dimension_contradicts_the_layout() {
    let dir = tempfile::tempdir().unwrap();
    let spec = write_fixture(dir.path());
    std::fs::write(
        dir.path().join("bom.csv"),
        "Designator,Value,MPN,Manufacturer\nR1,10uH,LQH32CN100K23,Murata\n",
    )
    .unwrap();

    let error = run(&RunConfig {
        spec,
        ..Default::default()
    })
    .expect_err("a resistor/inductor identity contradiction must fail closed");
    let diagnostic = error.to_string();
    assert!(diagnostic.contains("R1"), "{diagnostic}");
    assert!(
        diagnostic.contains("resistance") && diagnostic.contains("inductance"),
        "the binder must name both conflicting dimensions: {diagnostic}"
    );
}

#[test]
fn a_variant_cannot_silently_name_an_unknown_or_contradictory_reference() {
    for (name, body, expected) in [
        (
            "unknown",
            "name = \"bad\"\nno_fit = [\"R9\"]\n",
            "unknown reference(s): R9",
        ),
        (
            "contradictory",
            "name = \"bad\"\nfit = [\"R1\"]\nno_fit = [\"R1\"]\n",
            "both fitted and left open",
        ),
        (
            "duplicate",
            "name = \"bad\"\nno_fit = [\"R1\", \"R1\"]\n",
            "more than once in `no_fit`",
        ),
        (
            "empty",
            "name = \"bad\"\n",
            "names no `fit` or `no_fit` references",
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let spec = write_fixture(dir.path());
        std::fs::write(dir.path().join("sensorless.variant.toml"), body).unwrap();
        let error = run(&RunConfig {
            spec,
            ..Default::default()
        })
        .expect_err("a variant typo or contradiction must fail closed");
        assert!(error.to_string().contains(expected), "{name}: {error}");
    }
}

#[test]
fn an_assembly_that_leaves_every_component_open_is_invalid_not_green() {
    let dir = tempfile::tempdir().unwrap();
    let spec = write_fixture(dir.path());
    std::fs::write(
        dir.path().join("sensorless.variant.toml"),
        "name = \"empty\"\nno_fit = [\"R1\", \"R2\", \"U4\"]\n",
    )
    .unwrap();

    for subcommand in ["check", "run"] {
        let output = Command::new(env!("CARGO_BIN_EXE_hauksbee-ci"))
            .arg(subcommand)
            .arg(&spec)
            .output()
            .expect("hauksbee-ci runs");
        assert_eq!(
            output.status.code(),
            Some(2),
            "{subcommand} must reject a vacuous empty assembly; stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let diagnostic = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            diagnostic.contains("leaves every board component open")
                && diagnostic.contains("`no_faults`"),
            "the refusal must explain the false-green risk: {diagnostic}"
        );
    }
}

#[test]
fn a_variant_absent_spi_part_cannot_be_reintroduced_as_a_virtual_peripheral() {
    let dir = tempfile::tempdir().unwrap();
    let spec = write_fixture(dir.path());
    std::fs::write(
        dir.path().join("sensorless.variant.toml"),
        "name = \"without-eeprom\"\nno_fit = [\"U4\"]\n",
    )
    .unwrap();
    let mut body = std::fs::read_to_string(&spec).unwrap();
    body.push_str(
        r#"
[[peripheral]]
id = "memory"
type = "spi_eeprom"
ref = "U4"
cs_net = "VCC"
"#,
    );
    std::fs::write(&spec, body).unwrap();

    for subcommand in ["check", "run"] {
        let output = Command::new(env!("CARGO_BIN_EXE_hauksbee-ci"))
            .arg(subcommand)
            .arg(&spec)
            .output()
            .expect("hauksbee-ci runs");
        assert_eq!(output.status.code(), Some(2), "{subcommand} must refuse");
        let diagnostic = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            diagnostic.contains("U4")
                && diagnostic.contains("left open")
                && diagnostic.contains("will not simulate"),
            "the refusal must name the absent physical device: {diagnostic}"
        );
    }
}

#[test]
fn check_and_run_use_the_same_explicit_model_layer_for_bom_identity() {
    let dir = tempfile::tempdir().unwrap();
    let spec = write_fixture(dir.path());
    let board = BOARD.replace("25LC256", "HB-CUSTOM-LAYOUT");
    std::fs::write(dir.path().join("board.kicad_pcb"), &board).unwrap();
    std::fs::write(
        dir.path().join("bom.csv"),
        format!("{BOM}U4,ATmega328P-AU,ATmega328P,Microchip,\n"),
    )
    .unwrap();
    let models = dir.path().join("models");
    std::fs::create_dir(&models).unwrap();
    std::fs::write(
        models.join("custom.toml"),
        r#"
[[models]]
id = "hb_custom_layout_resistor"
kind = "passive"
passive_class = "resistor"
description = "test-only custom layout identity"

[models.match]
value_re = "^HB-CUSTOM-LAYOUT$"

[models.params]
value_override = "10k"

[models.pins]
"1" = "a"
"4" = "b"
"#,
    )
    .unwrap();

    let library = hauksbee_models::ModelLibrary::builtin_with_user_dirs(&[models.as_path()]);
    let resolution = library.resolve(&hauksbee_models::ComponentQuery::new(
        None,
        Some("HB-CUSTOM-LAYOUT".to_string()),
        None,
    ));
    assert_eq!(
        resolution.model.as_ref().map(|model| model.id.as_str()),
        Some("hb_custom_layout_resistor"),
        "the fixture must prove that --models-dir changes identity authority"
    );
    let mut direct_board =
        hauksbee_extract::ExtractedBoard::from_kicad_pcb(&board).expect("fixture board parses");
    let direct_bom =
        hauksbee_extract::bom::Bom::read(&dir.path().join("bom.csv")).expect("fixture BOM parses");
    assert_eq!(
        direct_board
            .component("U4")
            .map(|component| component.value.as_str()),
        Some("HB-CUSTOM-LAYOUT")
    );
    let u4_hint = direct_bom
        .identity_hints()
        .into_iter()
        .find(|hint| hint.reference == "U4")
        .expect("U4 BOM identity hint");
    assert_eq!(u4_hint.mpn.as_deref(), Some("ATmega328P"));
    let mut bom_query =
        hauksbee_models::ComponentQuery::new(None, Some("ATmega328P".to_string()), None);
    bom_query.mpn = Some("ATmega328P".to_string());
    assert_eq!(
        library
            .resolve(&bom_query)
            .model
            .as_ref()
            .map(|model| model.id.as_str()),
        Some("atmega328p")
    );
    let direct_error =
        hauksbee_engine::binder::apply_bom_identity(&mut direct_board, &direct_bom, &library)
            .expect_err("the custom layout identity contradicts the BOM identity");
    assert!(direct_error.to_string().contains("U4"), "{direct_error}");

    let plain_check = Command::new(env!("CARGO_BIN_EXE_hauksbee-ci"))
        .arg("check")
        .arg(&spec)
        .output()
        .expect("hauksbee-ci check runs");
    assert_eq!(
        plain_check.status.code(),
        Some(0),
        "without the repository model, the custom layout identity is unknown: {}",
        String::from_utf8_lossy(&plain_check.stderr)
    );

    for subcommand in ["check", "run"] {
        let output = Command::new(env!("CARGO_BIN_EXE_hauksbee-ci"))
            .arg(subcommand)
            .arg(&spec)
            .arg("--models-dir")
            .arg(&models)
            .output()
            .expect("hauksbee-ci runs");
        assert_eq!(output.status.code(), Some(2), "{subcommand} must refuse");
        let diagnostic = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            diagnostic.contains("U4")
                && diagnostic.contains("HB-CUSTOM-LAYOUT")
                && diagnostic.contains("ATmega328P")
                && diagnostic.contains("different revisions"),
            "both commands must reconcile under the same custom model authority: {diagnostic}"
        );
    }
}
