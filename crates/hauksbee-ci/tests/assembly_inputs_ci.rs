//! Production acceptance for BOM, placement and explicit assembly variants in
//! `hauksbee-ci`. These enter through the public runner and the CLI validator:
//! reader-only unit tests would not prove that the assembled identity and exact
//! artifact bytes reach the gate's evidence inventory.

use std::path::{Path, PathBuf};
use std::process::Command;

use hauksbee_ci::run;

use crate::support::{bin, stderr, stdout};

/// A routed board: VCC joined above the parts and GND below, so the static
/// check has no copper finding to add to the gate this suite is about.
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
    (pad 4 smd rect (at 1 0) (net 1 "GND")))
  (segment (start 10 20) (end 10 18) (width 0.25) (layer F.Cu) (net 2))
  (segment (start 12 20) (end 12 18) (width 0.25) (layer F.Cu) (net 2))
  (segment (start 15 20) (end 15 18) (width 0.25) (layer F.Cu) (net 2))
  (segment (start 10 18) (end 15 18) (width 0.25) (layer F.Cu) (net 2))
  (segment (start 11 20) (end 11 22) (width 0.25) (layer F.Cu) (net 1))
  (segment (start 13 20) (end 13 22) (width 0.25) (layer F.Cu) (net 1))
  (segment (start 16 20) (end 16 22) (width 0.25) (layer F.Cu) (net 1))
  (segment (start 11 22) (end 16 22) (width 0.25) (layer F.Cu) (net 1)))"#;

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

/// A fresh [`write_fixture`] dir whose `file` is then replaced by `body`.
fn fixture_with(file: &str, body: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let spec = write_fixture(dir.path());
    std::fs::write(dir.path().join(file), body).unwrap();
    (dir, spec)
}

fn artifact<'a>(json: &'a serde_json::Value, role: &str) -> &'a serde_json::Value {
    json["inventory"]
        .as_array()
        .expect("typed run inventory")
        .iter()
        .find(|item| item["role"] == role)
        .unwrap_or_else(|| panic!("missing {role} artifact: {}", json["inventory"]))
}

/// Run `hauksbee-ci <subcommand> <spec> <extra...>`, returning its exit code
/// and stdout+stderr joined: a refusal's sentence can land on either stream, so
/// every check below reads the pair as one diagnostic.
fn cli(subcommand: &str, spec: &Path, extra: &[&str]) -> (Option<i32>, String) {
    let output = Command::new(bin())
        .arg(subcommand)
        .arg(spec)
        .args(extra)
        .output();
    let output = output.expect("hauksbee-ci runs");
    let text = format!("{}\n{}", stdout(&output), stderr(&output));
    (output.status.code(), text)
}

/// `check` and `run` must refuse the SAME input the SAME way (exit 2, naming
/// every `wants` substring). The pair is the point: a refusal only `run` makes
/// is one the fast pre-flight told the user they had passed.
fn both_refuse(spec: &Path, extra: &[&str], wants: &[&str]) {
    for subcommand in ["check", "run"] {
        let (code, diagnostic) = cli(subcommand, spec, extra);
        assert_eq!(code, Some(2), "{subcommand} must refuse: {diagnostic}");
        for want in wants {
            assert!(
                diagnostic.contains(want),
                "{subcommand} must say {want:?}: {diagnostic}"
            );
        }
    }
}

/// Does `artifact`'s contribution list record a `what` whose detail mentions
/// `needle`? Reconciliation that leaves no such record is an unreviewable
/// pre-pass, which is what each caller is refusing.
fn contributed(artifact: &serde_json::Value, what: &str, needle: &str) -> bool {
    artifact["contributed"].as_array().is_some_and(|items| {
        items.iter().any(|item| {
            item["what"] == what
                && item["detail"]
                    .as_str()
                    .is_some_and(|line| line.contains(needle))
        })
    })
}

#[test]
fn bom_placement_and_variant_shape_the_gate_and_its_exact_evidence() {
    let variant = "name = \"sensorless\"\nno_fit = [\"R2\", \"U4\"]\n";
    let (_dir, spec) = fixture_with("sensorless.variant.toml", variant);
    let result = run(&crate::support::run_config(spec))
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
        contributed(bom, "part identity", "manufacturer part number"),
        "BOM reconciliation must be evidence, not an unrecorded pre-pass: {bom}"
    );
    let placement = artifact(&json, "placement");
    assert!(
        contributed(placement, "assembly placement", "2 parts placed"),
        "placement reconciliation must be evidence: {placement}"
    );
    let variant = artifact(&json, "variant");
    assert_eq!(variant["kind"], "toml");
    for detail in ["sensorless", "left open [R2, U4]"] {
        assert!(
            variant["contributed"][0]["detail"]
                .as_str()
                .is_some_and(|line| line.contains(detail)),
            "the exact population decision must be reviewable: {variant}"
        );
    }

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
    let bom = "Designator,Value,Manufacturer,Manufacturer Name\nR1,10k,Yageo,Other\n";
    let (_dir, spec) = fixture_with("bom.csv", bom);
    both_refuse(&spec, &[], &["two columns that could be the manufacturer"]);
}

#[test]
fn ci_refuses_a_bom_whose_part_dimension_contradicts_the_layout() {
    let bom = "Designator,Value,MPN,Manufacturer\nR1,10uH,LQH32CN100K23,Murata\n";
    let (_dir, spec) = fixture_with("bom.csv", bom);
    let error = run(&crate::support::run_config(spec))
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
        let (_dir, spec) = fixture_with("sensorless.variant.toml", body);
        let error = run(&crate::support::run_config(spec))
            .expect_err("a variant typo or contradiction must fail closed");
        assert!(error.to_string().contains(expected), "{name}: {error}");
    }
}

#[test]
fn an_assembly_that_leaves_every_component_open_is_invalid_not_green() {
    let variant = "name = \"empty\"\nno_fit = [\"R1\", \"R2\", \"U4\"]\n";
    let (_dir, spec) = fixture_with("sensorless.variant.toml", variant);
    // The refusal must explain the false-green risk, not just say no.
    both_refuse(
        &spec,
        &[],
        &["leaves every board component open", "`no_faults`"],
    );
}

#[test]
fn a_variant_absent_spi_part_cannot_be_reintroduced_as_a_virtual_peripheral() {
    let variant = "name = \"without-eeprom\"\nno_fit = [\"U4\"]\n";
    let (_dir, spec) = fixture_with("sensorless.variant.toml", variant);
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

    // The refusal must name the absent physical device.
    both_refuse(&spec, &[], &["U4", "left open", "will not simulate"]);
}

#[test]
fn check_and_run_use_the_same_explicit_model_layer_for_bom_identity() {
    let board = BOARD.replace("25LC256", "HB-CUSTOM-LAYOUT");
    let (dir, spec) = fixture_with("board.kicad_pcb", &board);
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

    let (code, diagnostic) = cli("check", &spec, &[]);
    assert_eq!(
        code,
        Some(0),
        "without the repository model, the custom layout identity is unknown: {diagnostic}"
    );

    // Both commands must reconcile under the same custom model authority.
    let models_dir = models.to_str().expect("models dir path");
    both_refuse(
        &spec,
        &["--models-dir", models_dir],
        &[
            "U4",
            "HB-CUSTOM-LAYOUT",
            "ATmega328P",
            "different revisions",
        ],
    );
}
