//! Integration tests for `[[sensor]]` in hauksbee-ci specs.
//!
//! These tests run the `lm75_thermostat.toml` / `lm75_thermostat_cold.toml`
//! example specs from `crates/hauksbee-ci/examples/` against the Renode STM32
//! backend. They skip cleanly when Renode is not installed, mirroring the
//! pattern used by the engine-level `i2c_sensor_cosim_renode.rs` tests.
//!
//! ## What is proven
//!
//! `RegisterMapSensor::from_toml` is called from the runner, the sensor is
//! attached to the I2C bus, Renode routes the firmware's I2C transactions
//! through the hauksbee bridge, and the FLAG net follows the 30 C threshold.
//! Reply bytes come entirely from the declarative interpreter — no hand-coded
//! Lm75, no injection.
//!
//! ## Running
//!
//! ```
//! cargo test -p hauksbee-ci sensor_attach -- --nocapture
//! ```
//! Requires Renode at `~/renode-portable`.

use std::path::PathBuf;

use hauksbee_ci::{run, RunConfig};

fn example(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join(name)
}

fn renode_available() -> bool {
    hauksbee_mcu::renode::is_available()
}

fn firmware_present() -> bool {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/firmware/stm32_i2c_thermostat/thermostat.elf")
        .exists()
}

/// The hot run: declarative LM75 at 40 C drives FLAG HIGH (boot-coverage PASS).
///
/// This is the primary proof. It fails only if the Renode I2C bridge is not
/// wired (FLAG never reaches 2.5 V) or the interpreter encoded the temperature
/// incorrectly (firmware reads != 40 C). Either failure is a clear signal.
#[test]
fn sensor_attach_lm75_hot_flag_goes_high() {
    if !renode_available() {
        eprintln!("SKIP sensor_attach_lm75_hot: Renode not installed");
        return;
    }
    if !firmware_present() {
        eprintln!(
            "SKIP sensor_attach_lm75_hot: STM32 I2C thermostat firmware not built \
             (run `make -C testdata/firmware/stm32_i2c_thermostat`)"
        );
        return;
    }

    let result = run(&RunConfig {
        spec: example("lm75_thermostat.toml"),
        ..Default::default()
    })
    .expect("lm75_thermostat spec should load and run");

    assert!(
        result.passed(),
        "declarative LM75 at 40 C: FLAG should go HIGH (boot-coverage PASS):\n{}",
        result.render_human()
    );

    // Specifically the boot-coverage assertion must be green.
    let bc = result
        .results
        .iter()
        .find(|r| r.kind == "boot-coverage")
        .expect("boot-coverage assertion must be present in lm75_thermostat.toml");
    assert!(
        bc.passed,
        "boot-coverage on FLAG must pass at 40 C: {}",
        bc.detail
    );

    eprintln!(
        "sensor_attach_lm75_hot: PASS — declarative LM75 at 40 C drove FLAG HIGH via Renode I2C bridge"
    );
}

/// The cold run: declarative LM75 at 20 C — FLAG must stay LOW (voltage PASS).
///
/// Negative control: the firmware reads 20 C < 30 C and must not drive FLAG.
/// The 4k7 pull-down holds the net at ~0 V the whole run.
#[test]
fn sensor_attach_lm75_cold_flag_stays_low() {
    if !renode_available() {
        eprintln!("SKIP sensor_attach_lm75_cold: Renode not installed");
        return;
    }
    if !firmware_present() {
        eprintln!(
            "SKIP sensor_attach_lm75_cold: STM32 I2C thermostat firmware not built \
             (run `make -C testdata/firmware/stm32_i2c_thermostat`)"
        );
        return;
    }

    let result = run(&RunConfig {
        spec: example("lm75_thermostat_cold.toml"),
        ..Default::default()
    })
    .expect("lm75_thermostat_cold spec should load and run");

    assert!(
        result.passed(),
        "declarative LM75 at 20 C: FLAG should stay LOW (voltage PASS):\n{}",
        result.render_human()
    );

    eprintln!(
        "sensor_attach_lm75_cold: PASS — declarative LM75 at 20 C held FLAG LOW via Renode I2C bridge"
    );
}

// ── Spec parsing tests (no Renode needed) ────────────────────────────────────

/// The inline `spec` field is parsed correctly without running the co-sim.
/// This catches TOML-format regressions without a Renode dependency.
#[test]
fn sensor_spec_inline_parses() {
    use hauksbee_ci::Spec;
    use std::path::Path;

    let spec = Spec::load(Path::new(
        concat!(env!("CARGO_MANIFEST_DIR"), "/examples/lm75_thermostat.toml"),
    ))
    .expect("lm75_thermostat.toml must load and validate");

    assert_eq!(spec.sensors.len(), 1, "one [[sensor]] block");
    let s = &spec.sensors[0];
    assert_eq!(s.id, "U2_lm75");
    assert!(s.spec.is_some(), "inline spec must be set");
    assert!(s.spec_file.is_none(), "spec_file must not be set");
    assert_eq!(
        s.inputs.get("temperature_c").copied(),
        Some(40.0),
        "temperature_c input override is 40.0"
    );
}

/// A `[[sensor]]` with neither `spec` nor `spec_file` is rejected at load.
#[test]
fn sensor_spec_missing_source_is_rejected() {
    use hauksbee_ci::Spec;

    // Write a minimal spec with a sensor that has no source.
    let board_content = r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+3V3")
  (module Resistor_THT:R_Axial_DIN0207_L6.3mm_D2.5mm (layer F.Cu)
    (at 100 100)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (net 2 "+3V3"))
    (pad 2 thru_hole circle (at 2 0) (net 1 "GND"))
  )
)
"#;
    let dir = std::env::temp_dir().join("hauksbee_ci_sensor_tests");
    std::fs::create_dir_all(&dir).unwrap();
    let board_path = dir.join("test_board.kicad_pcb");
    std::fs::write(&board_path, board_content).unwrap();

    let spec_src = format!(
        r#"name = "sensor test"
board = "{board}"
duration_ms = 10

[[sensor]]
id = "bad_sensor"

[[assert]]
kind = "voltage"
net = "+3V3"
min = 3.0
"#,
        board = board_path.display()
    );
    let spec_path = dir.join("bad_sensor.toml");
    std::fs::write(&spec_path, &spec_src).unwrap();

    let err = Spec::load(&spec_path).expect_err("spec with sourceless sensor must fail to load");
    let msg = err.to_string();
    assert!(
        msg.contains("bad_sensor") && (msg.contains("spec") || msg.contains("spec_file")),
        "error must name the sensor and mention spec/spec_file: {msg}"
    );
}

/// A `[[sensor]]` with both `spec` and `spec_file` is rejected at load.
#[test]
fn sensor_spec_both_sources_is_rejected() {
    use hauksbee_ci::Spec;

    let dir = std::env::temp_dir().join("hauksbee_ci_sensor_tests");
    std::fs::create_dir_all(&dir).unwrap();
    let board_content = r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+3V3")
  (module Resistor_THT:R_Axial_DIN0207_L6.3mm_D2.5mm (layer F.Cu)
    (at 100 100)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (net 2 "+3V3"))
    (pad 2 thru_hole circle (at 2 0) (net 1 "GND"))
  )
)
"#;
    let board_path = dir.join("test_board2.kicad_pcb");
    std::fs::write(&board_path, board_content).unwrap();

    let spec_src = format!(
        r#"name = "sensor test"
board = "{board}"
duration_ms = 10

[[sensor]]
id = "both_sensor"
spec = "[sensor]\nname=\"X\"\n"
spec_file = "some.toml"

[[assert]]
kind = "voltage"
net = "+3V3"
min = 3.0
"#,
        board = board_path.display()
    );
    let spec_path = dir.join("both_sources.toml");
    std::fs::write(&spec_path, &spec_src).unwrap();

    let err =
        Spec::load(&spec_path).expect_err("spec with both spec and spec_file must fail to load");
    let msg = err.to_string();
    assert!(
        msg.contains("both_sensor") && msg.contains("mutually exclusive"),
        "error must mention mutual exclusivity: {msg}"
    );
}
