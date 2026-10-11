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
//! Reply bytes come entirely from the declarative interpreter, no hand-coded
//! Lm75, no injection.
//!
//! ## Running
//!
//! ```
//! cargo test -p hauksbee-ci sensor_attach -- --nocapture
//! ```
//! Requires Renode at `~/renode-portable`. Both co-sim tests take
//! `support::descriptor_env_lock`: they need the stock stm32f103 descriptor,
//! and a sibling test in this binary publishes an I2C-less one through the
//! process-global `HAUKSBEE_MCU_DIR`.

use std::path::PathBuf;

use hauksbee_ci::run;

use crate::support::{self, example};

#[cfg(feature = "renode")]
fn firmware_present() -> bool {
    support::repo("testdata/firmware/stm32_i2c_thermostat/thermostat.elf").exists()
}

/// One declarative-LM75 thermostat run from `examples/<spec>`, asserted GREEN
/// with the whole human report as the message. Holds the descriptor lock, and
/// skips cleanly when Renode or the thermostat firmware is absent.
#[cfg(feature = "renode")]
fn lm75_run(case: &str, spec: &str, expect: &str) -> Option<hauksbee_ci::CiResult> {
    let _guard = support::descriptor_env_lock();
    if !hauksbee_mcu::renode::is_available() || !firmware_present() {
        eprintln!(
            "SKIP {case}: needs Renode and \
             `make -C testdata/firmware/stm32_i2c_thermostat`"
        );
        return None;
    }
    let result = run(&support::run_config(example(spec))).expect("the spec loads and runs");
    support::assert_green(&result, &format!("{case}: {expect}"));
    eprintln!("{case}: PASS: {expect}, via the Renode I2C bridge");
    Some(result)
}

/// The hot run: declarative LM75 at 40 C drives FLAG HIGH (boot-coverage PASS).
///
/// This is the primary proof. It fails only if the Renode I2C bridge is not
/// wired (FLAG never reaches 2.5 V) or the interpreter encoded the temperature
/// incorrectly (firmware reads != 40 C). Either failure is a clear signal.
#[cfg(feature = "renode")]
#[test]
fn sensor_attach_lm75_hot_flag_goes_high() {
    let expect = "declarative LM75 at 40 C drove FLAG HIGH (boot-coverage PASS)";
    let Some(result) = lm75_run("lm75_hot", "lm75_thermostat.toml", expect) else {
        return;
    };
    // Specifically the boot-coverage assertion must be green.
    let bc = support::row(&result, "kind:boot_coverage");
    support::assert_row_green(bc, "boot-coverage on FLAG must pass at 40 C");
}

/// The cold run: declarative LM75 at 20 C, FLAG must stay LOW (voltage PASS).
///
/// Negative control: the firmware reads 20 C < 30 C and must not drive FLAG.
/// The 4k7 pull-down holds the net at ~0 V the whole run.
#[cfg(feature = "renode")]
#[test]
fn sensor_attach_lm75_cold_flag_stays_low() {
    let expect = "declarative LM75 at 20 C held FLAG LOW (voltage PASS)";
    lm75_run("lm75_cold", "lm75_thermostat_cold.toml", expect);
}

// ── Spec parsing tests (no Renode needed) ────────────────────────────────────

/// The inline `spec` field is parsed correctly without running the co-sim.
/// This catches TOML-format regressions without a Renode dependency.
#[test]
fn sensor_spec_inline_parses() {
    use hauksbee_ci::Spec;
    use std::path::Path;

    let spec = Spec::load(Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/lm75_thermostat.toml"
    )))
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

/// The smallest board a `[[sensor]]` load error can be raised against (one 10k
/// between +3V3 and GND), plus a spec naming it, written to a fresh temp file.
/// Both rejection cases below differ only in the `[[sensor]]` body they carry.
fn sensor_spec(name: &str, sensor: &str) -> PathBuf {
    let board = support::write_tmp(
        "sensor_board.kicad_pcb",
        r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
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
"#,
    );
    support::write_tmp(
        name,
        &format!(
            r#"name = "sensor test"
board = {board}
duration_ms = 10

{sensor}
[[assert]]
kind = "voltage"
net = "+3V3"
min = 3.0
"#,
            board = support::toml_path(&board)
        ),
    )
}

/// A `[[sensor]]` with neither `spec` nor `spec_file` is rejected at load.
#[test]
fn sensor_spec_missing_source_is_rejected() {
    let path = sensor_spec("bad_sensor.toml", "[[sensor]]\nid = \"bad_sensor\"\n");
    let err =
        hauksbee_ci::Spec::load(&path).expect_err("spec with sourceless sensor must fail to load");
    let msg = err.to_string();
    assert!(
        msg.contains("bad_sensor") && (msg.contains("spec") || msg.contains("spec_file")),
        "error must name the sensor and mention spec/spec_file: {msg}"
    );
}

/// A `[[sensor]]` with both `spec` and `spec_file` is rejected at load.
#[test]
fn sensor_spec_both_sources_is_rejected() {
    let path = sensor_spec(
        "both_sources.toml",
        "[[sensor]]\nid = \"both_sensor\"\n\
         spec = \"[sensor]\\nname=\\\"X\\\"\\n\"\nspec_file = \"some.toml\"\n",
    );
    let err = hauksbee_ci::Spec::load(&path)
        .expect_err("spec with both spec and spec_file must fail to load");
    let msg = err.to_string();
    assert!(
        msg.contains("both_sensor") && msg.contains("mutually exclusive"),
        "error must mention mutual exclusivity: {msg}"
    );
}

#[test]
fn unknown_sensor_input_is_refused_before_simulation() {
    let dir = tempfile::tempdir().expect("temporary spec directory");
    let board = example("boards/tolerance_divider.kicad_pcb");
    let spec = format!(
        r#"name = "bad sensor input"
board = {}
duration_ms = 1

[[sensor]]
id = "U_TEMP"
spec = '''
[sensor]
name = "temperature"
bus = "i2c"
i2c_address = 0x48
[[sensor.input]]
name = "temperature_c"
default = 25.0
[[sensor.register]]
addr = 0
bytes = 2
encoding = "q7.1_be"
expr = "temperature_c"
[sensor.protocol]
style = "i2c_pointer"
'''
[sensor.inputs]
tempereture_c = 40.0

[[assert]]
kind = "no_faults"
"#,
        support::toml_path(&board)
    );
    let path = dir.path().join("unknown-input.toml");
    std::fs::write(&path, spec).expect("write test spec");
    let error = run(&support::run_config(path))
        .expect_err("unknown register-map input must not silently keep its default");
    assert!(
        error.to_string().contains("tempereture_c") && error.to_string().contains("not declared"),
        "error should name the typo and exact-spec contract: {error}"
    );
}
