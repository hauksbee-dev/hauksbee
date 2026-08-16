//! CLI tests for `hauksbee models lint`: every `[models.logic]` validation
//! category is proven against a deliberately-broken fixture, asserting the
//! NAMED error text (not just a non-zero exit), and the shipping builtin db
//! must lint clean through the same code path binding uses.

use std::path::PathBuf;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_hauksbee")
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/logic_lint")
        .join(name)
}

fn lint(path: &std::path::Path) -> (i32, String) {
    let out = Command::new(bin())
        .args(["models", "lint"])
        .arg(path)
        .output()
        .expect("hauksbee binary runs");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr);
    (out.status.code().unwrap_or(-1), format!("{stdout}{stderr}"))
}

/// One broken fixture per validation category, with the named-error substring
/// the lint output must carry.
#[test]
fn lint_names_every_broken_category() {
    let cases: &[(&str, &str)] = &[
        (
            "undeclared_pin.toml",
            "references undeclared name 'phantom_pin'",
        ),
        (
            "width_mismatch.toml",
            "bit index 7 is out of range for register 'reg' (4 bits)",
        ),
        (
            "register_as_scalar.toml",
            "width mismatch: comb expression for 'y' uses register 'reg' (8 bits) as a 1-bit value",
        ),
        (
            "clock_also_comb.toml",
            "clock pin 'clk' of register 'ff' is also referenced combinationally",
        ),
        (
            "unreachable_output.toml",
            "output 'y_ghost' is declared but never assigned by comb (unreachable output)",
        ),
        (
            "bad_tristate.toml",
            "enable pin 'oe_missing' is not a declared input",
        ),
        (
            "non_converging.toml",
            "does not converge within 16 fixpoint sweeps",
        ),
    ];
    for (file, needle) in cases {
        let (code, out) = lint(&fixture(file));
        assert_eq!(
            code, 2,
            "{file}: lint must exit 2 on a finding; output:\n{out}"
        );
        assert!(
            out.contains(needle),
            "{file}: expected the named error {needle:?} in output:\n{out}"
        );
    }
}

/// The shipping builtin digital db must lint clean (same compile path as
/// binding, so lint-ok == bind-ok).
#[test]
fn builtin_digital_db_lints_clean() {
    let db = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../hauksbee-models/db/digital.toml");
    let (code, out) = lint(&db);
    assert_eq!(
        code, 0,
        "builtin digital.toml must lint clean; output:\n{out}"
    );
    for id in ["74hc595", "74hc165", "74hc125", "74hc27", "74hc02"] {
        assert!(
            out.contains(&format!("model '{id}': ok")),
            "expected '{id}' ok line in:\n{out}"
        );
    }
    assert!(out.contains(": clean"), "clean summary line:\n{out}");
}

#[test]
fn lint_refuses_a_model_with_no_real_match_rule() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stale-editor.toml");
    std::fs::write(
        &path,
        "[[models]]\nid = \"test_r\"\nkind = \"passive\"\n[models.match]\nvalue = [\"^10k$\"]\n",
    )
    .unwrap();
    let (code, out) = lint(&path);
    assert_eq!(code, 2, "a non-binding model is a lint finding:\n{out}");
    assert!(out.contains("no match rules"), "{out}");
}

#[test]
fn scaffold_does_not_infer_model_kind_from_reference_letter() {
    let dir = tempfile::tempdir().unwrap();
    let scaffold = dir.path().join("u3.toml");
    let board = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../frontend/public/boards/pic_programmer.kicad_pcb");
    let made = Command::new(bin())
        .args(["models", "new", "U3", "--board"])
        .arg(&board)
        .arg("--out")
        .arg(&scaffold)
        .output()
        .expect("scaffold command runs");
    assert!(
        made.status.success(),
        "{}",
        String::from_utf8_lossy(&made.stderr)
    );
    let text = std::fs::read_to_string(&scaffold).unwrap();
    assert!(text.contains("kind = \"choose_kind\""), "{text}");
    let (code, out) = lint(&scaffold);
    assert_ne!(
        code, 0,
        "the undecided scaffold must not lint green:\n{out}"
    );
    assert!(out.contains("unknown kind 'choose_kind'"), "{out}");

    let explicit = dir.path().join("u3-vreg.toml");
    let made = Command::new(bin())
        .args(["models", "new", "U3", "--board"])
        .arg(&board)
        .args(["--kind", "vreg", "--out"])
        .arg(&explicit)
        .output()
        .expect("explicit scaffold command runs");
    assert!(
        made.status.success(),
        "{}",
        String::from_utf8_lossy(&made.stderr)
    );
    let text = std::fs::read_to_string(explicit).unwrap();
    assert!(text.contains("kind = \"vreg\""), "{text}");
    assert!(!text.contains("kind = \"digital\""), "{text}");
    let explicit_path = dir.path().join("u3-vreg.toml");
    let (code, out) = lint(&explicit_path);
    assert_ne!(
        code, 0,
        "choosing a kind without supplying its parameters must stay fail-closed:\n{out}"
    );
    assert!(out.contains("missing required param 'vout'"), "{out}");
}

#[test]
fn scaffold_emits_unknown_provenance_and_a_pack_without_writing_elsewhere() {
    let dir = tempfile::tempdir().unwrap();
    let pack = dir.path().join("acme-pack");
    let board = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../frontend/public/boards/pic_programmer.kicad_pcb");
    let made = Command::new(bin())
        .args(["models", "new", "U3", "--board"])
        .arg(&board)
        .args(["--pack-dir"])
        .arg(&pack)
        .output()
        .expect("pack scaffold command runs");
    assert!(
        made.status.success(),
        "{}",
        String::from_utf8_lossy(&made.stderr)
    );

    let manifest = pack.join("pack.toml");
    let model_dir = pack.join("models");
    let model = model_dir.join("u3_7805.toml");
    assert!(manifest.is_file(), "pack manifest was not created");
    assert!(model.is_file(), "model was not created inside pack/models");
    assert!(
        !dir.path().join("u3_7805.toml").exists(),
        "--pack-dir must never leave a model in the caller's cwd"
    );

    let model_text = std::fs::read_to_string(&model).unwrap();
    let parsed: toml::Value = toml::from_str(&model_text).expect("scaffold is valid TOML");
    assert_eq!(
        parsed["models"][0]["source"]["uncertainty"][0]["status"].as_str(),
        Some("unknown"),
        "scaffold must retain explicit unknown uncertainty"
    );
    assert_eq!(
        parsed["models"][0]["source"]["tier"].as_str(),
        Some("user-model"),
        "a completed user scaffold must be able to win user-model resolution"
    );
    assert!(
        model_text.contains("TODO: cite a datasheet"),
        "{model_text}"
    );

    let (code, out) = lint(&model);
    assert_ne!(
        code, 0,
        "untouched scaffold must remain fail-closed:\n{out}"
    );
    assert!(out.contains("unknown kind 'choose_kind'"), "{out}");

    let second = Command::new(bin())
        .args(["models", "new", "U3", "--board"])
        .arg(&board)
        .args(["--pack-dir"])
        .arg(&pack)
        .output()
        .expect("second scaffold command runs");
    assert!(!second.status.success(), "overwrite must be refused");
    let second_out = String::from_utf8_lossy(&second.stderr);
    assert!(second_out.contains("refusing to overwrite"), "{second_out}");
}

#[test]
fn prepare_yes_writes_only_the_printed_pack_plan() {
    let dir = tempfile::tempdir().unwrap();
    let pack = dir.path().join("prepared-pack");
    let board = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../frontend/public/boards/pic_programmer.kicad_pcb");
    let out = Command::new(bin())
        .args(["models", "prepare"])
        .arg(&board)
        .args(["--pack-dir"])
        .arg(&pack)
        .arg("--yes")
        .output()
        .expect("prepare command runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("no network, LLM, installer, or pack registration"),
        "{stdout}"
    );
    assert!(stdout.contains("pack.toml"), "{stdout}");
    assert!(pack.join("pack.toml").is_file());
    assert!(pack.join("inventory.json").is_file());
    let workplan: serde_json::Value = serde_json::from_slice(
        &std::fs::read(pack.join("workplan.json")).expect("workplan written"),
    )
    .expect("workplan is valid JSON");
    assert_eq!(workplan["schema_version"], 1);
    assert_eq!(
        workplan["items"].as_array().unwrap().len(),
        std::fs::read_dir(pack.join("models")).unwrap().count(),
        "every prepared card has one deterministic work item"
    );
    assert!(
        workplan["validation_commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command.as_str().unwrap().contains("models coverage")),
        "the handoff says how to prove coverage after editing"
    );
    let validation_commands = workplan["validation_commands"].as_array().unwrap();
    assert_eq!(
        validation_commands
            .iter()
            .filter(|command| command.as_str().unwrap().contains("models lint"))
            .count(),
        workplan["items"].as_array().unwrap().len(),
        "models lint accepts one file, so the handoff must emit one exact command per prepared card"
    );
    assert!(
        validation_commands
            .iter()
            .all(|command| !command.as_str().unwrap().contains("*.toml")),
        "a wildcard would expand into an invalid multi-file models lint invocation"
    );
    let mut models = std::fs::read_dir(pack.join("models"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    models.sort();
    assert!(
        models.len() >= 4,
        "the fixture has at least four unresolved active refs, and partial executable cards are now prepared too"
    );
    for model in models {
        let text = std::fs::read_to_string(model).unwrap();
        let parsed = toml::from_str::<toml::Value>(&text).expect("bulk scaffold is valid TOML");
        let row = &parsed["models"][0];
        if row["kind"].as_str() == Some("choose_kind") {
            assert_eq!(
                row["source"]["uncertainty"][0]["status"].as_str(),
                Some("unknown")
            );
        } else {
            assert_eq!(row["source"]["tier"].as_str(), Some("user-model"));
            assert_eq!(row["source"]["validation"].as_str(), Some("unvalidated"));
            assert!(text.contains("Copied from winning model"), "{text}");
        }
    }
}

#[test]
fn prepare_without_yes_refuses_a_non_tty_before_writing() {
    let dir = tempfile::tempdir().unwrap();
    let pack = dir.path().join("prepared-pack");
    let board = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../frontend/public/boards/pic_programmer.kicad_pcb");
    let out = Command::new(bin())
        .args(["models", "prepare"])
        .arg(&board)
        .args(["--pack-dir"])
        .arg(&pack)
        .output()
        .expect("prepare command runs");
    assert!(!out.status.success());
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(combined.contains("stdin is not a terminal"), "{combined}");
    assert!(!pack.exists(), "refusal must not create the pack directory");
}

#[test]
fn prepare_refuses_overwrite_without_touching_existing_files() {
    let dir = tempfile::tempdir().unwrap();
    let pack = dir.path().join("prepared-pack");
    std::fs::create_dir_all(pack.join("models")).unwrap();
    let manifest = pack.join("pack.toml");
    std::fs::write(&manifest, "sentinel").unwrap();
    let board = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../frontend/public/boards/pic_programmer.kicad_pcb");
    let out = Command::new(bin())
        .args(["models", "prepare"])
        .arg(&board)
        .args(["--pack-dir"])
        .arg(&pack)
        .arg("--yes")
        .output()
        .expect("prepare command runs");
    assert!(!out.status.success());
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(combined.contains("refusing to overwrite"), "{combined}");
    assert_eq!(std::fs::read_to_string(manifest).unwrap(), "sentinel");
    assert_eq!(std::fs::read_dir(pack.join("models")).unwrap().count(), 0);
}

#[test]
fn coverage_and_prepare_treat_identity_only_as_a_behavior_gap() {
    let dir = tempfile::tempdir().unwrap();
    let models_dir = dir.path().join("models-dir");
    let pack = dir.path().join("prepared-pack");
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
    let board = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../frontend/public/boards/pic_programmer.kicad_pcb");

    let out = Command::new(bin())
        .args(["models", "coverage"])
        .arg(&board)
        .args(["--models-dir"])
        .arg(&models_dir)
        .arg("--json")
        .output()
        .expect("coverage command runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["schema_version"], 4);
    assert!(report["board"]["sha256"].as_str().unwrap().len() == 64);
    assert_eq!(
        report["summary"]["identified"].as_u64().unwrap(),
        report["summary"]["active_connected"].as_u64().unwrap()
            - report["summary"]["unresolved"].as_u64().unwrap()
    );
    assert_eq!(
        report["summary"]["executable_available"].as_u64().unwrap(),
        report["summary"]["executable_scope_unspecified"]
            .as_u64()
            .unwrap()
            + report["summary"]["executable_partial"].as_u64().unwrap()
            + report["summary"]["executable_declared"].as_u64().unwrap()
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
    assert!(
        row("U1")["pins"]
            .as_array()
            .unwrap()
            .iter()
            .any(|pin| pin["number"] == "5" && pin["net"].is_string()),
        "coverage must retain the board-observed pad/net inventory: {}",
        row("U1")
    );

    let met = Command::new(bin())
        .args(["models", "coverage"])
        .arg(&board)
        .args(["--models-dir"])
        .arg(&models_dir)
        .args(["--require", "U4:nominal_dc_output", "--json"])
        .output()
        .expect("capability-gated coverage runs");
    assert!(
        met.status.success(),
        "declared implemented capability should pass: {}",
        String::from_utf8_lossy(&met.stderr)
    );
    let met_report: serde_json::Value = serde_json::from_slice(&met.stdout).unwrap();
    assert_eq!(met_report["summary"]["requirements_met"], 1);
    assert_eq!(met_report["requirements"][0]["met"], true);

    let missing = Command::new(bin())
        .args(["models", "coverage"])
        .arg(&board)
        .args(["--models-dir"])
        .arg(&models_dir)
        .args(["--require", "U4:switching_ripple", "--json"])
        .output()
        .expect("missing-capability coverage runs");
    assert!(
        !missing.status.success(),
        "an explicitly missing capability must fail closed"
    );
    let missing_report: serde_json::Value = serde_json::from_slice(&missing.stdout).unwrap();
    assert_eq!(missing_report["summary"]["requirements_unmet"], 1);
    assert_eq!(missing_report["requirements"][0]["met"], false);
    assert!(missing_report["requirements"][0]["reason"]
        .as_str()
        .unwrap()
        .contains("explicitly declares"));

    let out = Command::new(bin())
        .args(["models", "prepare"])
        .arg(&board)
        .args(["--models-dir"])
        .arg(&models_dir)
        .args(["--pack-dir"])
        .arg(&pack)
        .arg("--yes")
        .output()
        .expect("prepare command runs");
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let inventory: serde_json::Value = serde_json::from_slice(
        &std::fs::read(pack.join("inventory.json")).expect("inventory written"),
    )
    .unwrap();
    let prepared = inventory["authoring_targets"].as_array().unwrap();
    assert!(
        prepared
            .iter()
            .any(|row| row["reference"] == "U1" && row["stage"] == "identity_only"),
        "identity-only models must be offered for behavioral upgrade"
    );
    assert!(
        !prepared.iter().any(|row| row["reference"] == "U4"),
        "authoring_targets remains the unresolved/identity denominator"
    );
    let partial_upgrade = pack.join("models/u4_lt1373.toml");
    let partial_text = std::fs::read_to_string(&partial_upgrade)
        .unwrap_or_else(|error| panic!("partial model upgrade was not prepared: {error}"));
    let partial_doc: toml::Value = toml::from_str(&partial_text).expect("upgrade is valid TOML");
    assert_eq!(
        partial_doc["models"][0]["coverage"]["implements"][0].as_str(),
        Some("nominal_dc_output")
    );
    assert_eq!(
        partial_doc["models"][0]["coverage"]["missing"][0].as_str(),
        Some("switching_ripple")
    );
    assert_eq!(
        partial_doc["models"][0]["source"]["tier"].as_str(),
        Some("user-model")
    );
    assert_eq!(
        partial_doc["models"][0]["source"]["validation"].as_str(),
        Some("unvalidated")
    );
    assert!(
        partial_text.contains("Copied from winning model 'lt1373_partial_fixture'"),
        "{partial_text}"
    );
}

/// A file with neither [sensor] nor [[models]] is a usage error (exit 1),
/// not a silent pass.
#[test]
fn lint_refuses_unrecognized_toml() {
    let dir = std::env::temp_dir().join("hauksbee_lint_test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("neither.toml");
    std::fs::write(&path, "[something_else]\nx = 1\n").unwrap();
    let (code, _out) = lint(&path);
    assert_eq!(code, 1, "unrecognized TOML shape must be a hard error");
}

/// The descriptor inspection must state, per part, what the co-sim will and will
/// not do about the two facts a wrong answer hides best (F6c):
///
/// - the CLOCK, which is cross-checked against the platform's own declarations at
///   load. A mismatch needs no advisory here because it is already a hard load
///   error, surfaced through the `soc descriptor '{}': ERROR: {e}` path.
/// - the WATCHDOG, either the part's own limitation sentence rendered verbatim,
///   or, when the descriptor claims none, that the part claims full fidelity.
///
/// Proven on a shipped descriptor (nRF52840, whose watchdog arms and never
/// fires) and on that same descriptor with the field removed, which is the only
/// way to reach the full-fidelity branch: every shipped Renode part currently
/// carries a limitation.
#[cfg(feature = "renode")]
#[test]
fn soc_inspection_states_the_clock_cross_check_and_the_watchdog_fidelity() {
    let stock_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../hauksbee-mcu/db/mcu/nrf52840.soc.toml");
    let (code, out) = lint(&stock_path);
    assert_eq!(code, 0, "the shipped descriptor must lint clean:\n{out}");
    assert!(
        out.contains(
            "clock: 64000000 Hz (cross-checked at load against the platform's own \
             `cpu PerformanceInMips` / `nvic systickFrequency` declarations)"
        ),
        "the clock line must say the number is checked, not decorative:\n{out}"
    );
    assert!(
        out.contains(
            "watchdog: The nRF52840 watchdog arms in this co-simulator (it reads back as \
             running, with a correct 32768 Hz reload) but never fires:"
        ),
        "the part's own sentence, verbatim:\n{out}"
    );
    // TIMING, the same two-branch discipline as the watchdog. The nRF52840
    // declares no timing_limitation (its clock is clock-truth gated), so the
    // silence must print as the claim it is.
    assert!(
        out.contains(
            "timing: this part claims a firmware delay costs the virtual time it costs \
             on silicon (measured by the clock-truth gate)"
        ),
        "an absent timing limitation is a CLAIM and must be printed as one:\n{out}"
    );

    // And the descriptor that DOES declare one (the F103's deliberate
    // TIMx-at-72MHz divergence) must have its own sentence quoted verbatim.
    let f103_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../hauksbee-mcu/db/mcu/stm32f103.soc.toml");
    let (code, out) = lint(&f103_path);
    assert_eq!(
        code, 0,
        "the shipped F103 descriptor must lint clean:\n{out}"
    );
    assert!(
        out.contains(
            "timing: The STM32F103 TIMx timer blocks run at the post-PLL 72 MHz in this \
             co-simulator while the core and SysTick run at the 8 MHz reset default"
        ),
        "the F103's own timing sentence, verbatim:\n{out}"
    );

    // The other branch: a descriptor claiming no limitation says so as a claim,
    // rather than leaving the reader to infer it from a missing line. Drop the
    // `watchdog_limitation = """ … """` block, closing delimiter included.
    let stock = std::fs::read_to_string(&stock_path).expect("read stock descriptor");
    let mut silent = String::new();
    let mut skipping = false;
    for line in stock.lines() {
        if line.starts_with("watchdog_limitation") {
            skipping = true;
            continue;
        }
        if skipping {
            skipping = line.trim() != "\"\"\"";
            continue;
        }
        silent.push_str(line);
        silent.push('\n');
    }
    assert!(
        !silent.contains("watchdog_limitation") && silent.contains("[[soc.ports]]"),
        "the field must be gone and the rest of the descriptor intact:\n{silent}"
    );

    let dir = std::env::temp_dir().join(format!("hauksbee-soc-lint-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = dir.join("nrf52840.soc.toml");
    std::fs::write(&path, silent).expect("write descriptor");
    let (code, out) = lint(&path);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 0, "still a valid descriptor:\n{out}");
    assert!(
        out.contains(
            "watchdog: this part claims full fidelity, an armed watchdog that is never fed \
             reboots the core the way silicon does"
        ),
        "an absent limitation is a CLAIM and must be printed as one:\n{out}"
    );
}

#[cfg(feature = "qemu")]
#[test]
fn qemu_soc_inspection_names_register_direction_and_mailbox_boundaries() {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../hauksbee-mcu/db/mcu/esp32.soc.toml");
    let (code, out) = lint(&path);
    assert_eq!(
        code, 0,
        "the shipped ESP32 descriptor must lint clean:\n{out}"
    );
    assert!(
        out.contains("peripheral out 0x3ff44004 + enable 0x3ff44020"),
        "real output level and direction addresses must be inspectable:\n{out}"
    );
    assert!(
        out.contains("firmware-mailbox fallback 0x50000000; input mailbox 0x50000004"),
        "the fallback and remaining input contract must stay named:\n{out}"
    );
    assert!(
        out.contains("gpio capability probe: /machine/soc/gpio properties gpio-out + gpio-enable"),
        "the live capability boundary must be inspectable:\n{out}"
    );
}
