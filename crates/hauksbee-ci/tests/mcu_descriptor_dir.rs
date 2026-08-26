//! `[mcu] descriptor_dir` end to end (E31): a spec-declared SoC-descriptor
//! override directory must feed the co-sim's descriptor resolution exactly the
//! way `$HAUKSBEE_MCU_DIR` does, with no env var set by the caller.
//!
//! The proof reuses the cosim_coverage_honesty fixture shape: the shipped
//! stm32f103 descriptor with its I2C controllers emptied, placed in a
//! directory the SPEC (not the environment) points at. If the overridden
//! descriptor is really loaded from there, the run reports the "models no I2C
//! controller" coverage hole; if the spec field were ignored, the stock
//! descriptor (which has i2c1) would load and no such warning could appear.
//!
//! Runs only with Renode + the thermostat fixtures present; skips cleanly
//! otherwise. Sets no env var itself, but serializes against nothing: it is
//! the only test in this file precisely because the runner publishes the dir
//! through the process-global env for the run's duration.

mod support;

use std::path::PathBuf;

use hauksbee_ci::{run, RunConfig};

fn repo(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel)
}

#[test]
fn a_spec_descriptor_dir_serves_the_overridden_descriptor() {
    if !hauksbee_mcu::renode::is_available() {
        eprintln!("SKIP: Renode not installed");
        return;
    }
    let board = repo("testdata/boards/stm32_i2c_thermostat.kicad_pcb");
    let fw = repo("testdata/firmware/stm32_i2c_thermostat/thermostat.elf");
    if !board.exists() || !fw.exists() {
        eprintln!("SKIP: thermostat board/firmware not present");
        return;
    }
    assert!(
        std::env::var_os("HAUKSBEE_MCU_DIR").is_none(),
        "this test proves the SPEC field alone plumbs through; the env must be unset"
    );

    // The override dir lives beside the spec and is referenced RELATIVELY,
    // proving descriptor_dir resolves against the spec file's directory.
    let dir = tempfile::tempdir().expect("tempdir");
    let socs = dir.path().join("socs");
    std::fs::create_dir_all(&socs).expect("mkdir socs");
    let stock = std::fs::read_to_string(repo("crates/hauksbee-mcu/db/mcu/stm32f103.soc.toml"))
        .expect("read stock descriptor");
    let no_i2c = stock.replace("controllers = [\"i2c1\"]", "controllers = []");
    assert_ne!(no_i2c, stock, "the stock descriptor must have had i2c1");
    std::fs::write(socs.join("stm32f103.soc.toml"), no_i2c).expect("write override");

    let spec_path = dir.path().join("descriptor_dir.toml");
    std::fs::write(
        &spec_path,
        format!(
            r#"
name        = "spec-declared descriptor dir"
board       = {}
firmware    = {}
duration_ms = 150
frame_ms    = 5.0

[mcu]
descriptor_dir = "socs"

[[supply]]
net   = "+3V3"
kind  = "ideal"
volts = 3.3

[[sensor]]
id = "U2_lm75"
spec = """
[sensor]
name        = "LM75_declarative"
bus         = "i2c"
i2c_address = 0x48

[[sensor.input]]
name    = "temperature_c"
default = 25.0

[[sensor.register]]
addr     = 0x00
bytes    = 2
encoding = "q7.1_be"
expr     = "temperature_c"

[sensor.protocol]
style = "i2c_pointer"
"""

[[assert]]
kind  = "peripheral"
id    = "U2_lm75"
field = "temperature_c"
min   = 20.0
max   = 45.0
"#,
            support::toml_path(&board.canonicalize().unwrap()),
            support::toml_path(&fw.canonicalize().unwrap()),
        ),
    )
    .expect("write spec");

    let result = run(&RunConfig {
        spec: spec_path,
        ..Default::default()
    })
    .expect("ci run");

    // The no-I2C descriptor could ONLY have come from the spec's socs/ dir:
    // the stock stm32f103 models i2c1, so this warning proves descriptor_dir
    // reached the engine's resolution path.
    assert!(
        result
            .coverage_warnings
            .iter()
            .any(|w| w.contains("models no I2C controller")),
        "the overridden (I2C-less) descriptor must have loaded from the spec's \
         descriptor_dir: {:?}",
        result.coverage_warnings
    );
    // And the run's descriptor scoping did not leak into the process env.
    assert!(
        std::env::var_os("HAUKSBEE_MCU_DIR").is_none(),
        "the runner must restore the env after the run"
    );
}
