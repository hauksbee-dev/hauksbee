//! Live-Renode CI-level proofs for the co-sim coverage honesty surfaces (U3).
//!
//! Two silent-degradation modes, each proven end to end through the REAL
//! hauksbee-ci pipeline (spec → runner → assertions → CiResult → all three
//! report formats) against the live Renode install:
//!
//!   1. **Dropped ADC injection** (finding 1): a board with a divider on PA0
//!      (`pa0_adc0` → engine channel 0) on the stock STM32F103 platform, which
//!      models no ADC. The run must carry a coverage warning naming the net in
//!      `CiResult::coverage_warnings` and render it in the human, JUnit, and
//!      GitHub-annotation formats.
//!
//!   2. **Unexercised bus sensor** (finding 2): the LM75 thermostat spec run
//!      against a platform whose descriptor models NO I2C controller (the
//!      shipped stm32f103 descriptor with its controllers emptied, loaded via
//!      `$HAUKSBEE_MCU_DIR`; the exact stock-nRF52840 failure shape before
//!      this round gave nRF real controllers). The bound sensor never sees a
//!      transaction; its state stays at the input default, which sits INSIDE
//!      the assertion window; the false green this round makes impossible. A
//!      `peripheral` assertion against it must FAIL with the never-exercised
//!      wording, and the coverage warning must reach every report format.
//!
//! Test 2 sets `HAUKSBEE_MCU_DIR` (process-global), so it lives alone in this
//! file's second test and test 1 runs the descriptor-untouched path FIRST via
//! a serial mutex. Both skip cleanly without Renode or the firmware fixtures.

use std::path::PathBuf;
use std::sync::Mutex;

use hauksbee_ci::{run, RunConfig};

/// Serialize the two tests: test 2 mutates `HAUKSBEE_MCU_DIR` process-wide.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn repo(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").join(rel)
}

fn renode_available() -> bool {
    hauksbee_mcu::renode::is_available()
}

fn write_spec(dir: &std::path::Path, name: &str, body: &str) -> PathBuf {
    std::fs::create_dir_all(dir).expect("mkdir spec dir");
    let p = dir.join(name);
    std::fs::write(&p, body).expect("write spec");
    p
}

#[test]
fn dropped_adc_injection_reaches_every_ci_report_format() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if !renode_available() {
        eprintln!("SKIP: Renode not installed");
        return;
    }
    let board = repo("testdata/boards/stm32_adc_divider_demo.kicad_pcb");
    let fw = repo("testdata/firmware/stm32_blinky/blinky.elf");
    if !fw.exists() {
        eprintln!("SKIP: blinky.elf not built");
        return;
    }

    let dir = std::env::temp_dir().join(format!("hauksbee-ci-adc-drop-{}", std::process::id()));
    let spec = write_spec(
        &dir,
        "adc_drop.toml",
        &format!(
            r#"
name        = "ADC coverage honesty (dropped injection)"
board       = "{}"
firmware    = "{}"
duration_ms = 200
frame_ms    = 5.0

[[supply]]
net   = "+3V3"
kind  = "ideal"
volts = 3.3

# The run itself is healthy; the HOLE is that TEMP_SENSE never reached the
# firmware. The assertion is deliberately unrelated (UART boots), so a green
# verdict would silently vouch for the un-run ADC path without the warning.
[[assert]]
kind     = "uart"
contains = "hello from stm32"
"#,
            board.canonicalize().unwrap().display(),
            fw.canonicalize().unwrap().display(),
        ),
    );

    let result = run(&RunConfig { spec, ..Default::default() }).expect("ci run");
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        result
            .coverage_warnings
            .iter()
            .any(|w| w.contains("TEMP_SENSE") && w.contains("no ADC injection map")),
        "the dropped channel must reach CiResult::coverage_warnings: {:?}",
        result.coverage_warnings
    );
    let human = result.render_human();
    assert!(
        human.contains("COVERAGE HOLE") && human.contains("TEMP_SENSE"),
        "human report must carry it: {human}"
    );
    let junit = result.render_junit();
    assert!(
        junit.contains("COVERAGE HOLE") && junit.contains("TEMP_SENSE"),
        "junit must carry it: {junit}"
    );
    let gh = result.render_github_annotations();
    assert!(
        gh.contains("COSIM COVERAGE HOLE") && gh.contains("TEMP_SENSE"),
        "github annotations must carry it: {gh}"
    );
}

#[test]
fn unexercised_bus_sensor_warns_and_fails_its_peripheral_assertion() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if !renode_available() {
        eprintln!("SKIP: Renode not installed");
        return;
    }
    let board = repo("testdata/boards/stm32_i2c_thermostat.kicad_pcb");
    let fw = repo("testdata/firmware/stm32_i2c_thermostat/thermostat.elf");
    if !board.exists() || !fw.exists() {
        eprintln!("SKIP: thermostat board/firmware not present");
        return;
    }

    // A platform with NO modeled I2C controller: the shipped stm32f103
    // descriptor, controllers emptied, served via $HAUKSBEE_MCU_DIR. This is
    // byte-for-byte the failure shape stock nrf52840 shipped with.
    let dir = std::env::temp_dir().join(format!("hauksbee-ci-unexercised-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir override dir");
    let stock = std::fs::read_to_string(repo("crates/hauksbee-mcu/db/mcu/stm32f103.soc.toml"))
        .expect("read stock descriptor");
    let no_i2c = stock.replace(
        "controllers = [\"i2c1\"]",
        "controllers = []",
    );
    assert_ne!(no_i2c, stock, "the stock descriptor must have had i2c1 to empty");
    std::fs::write(dir.join("stm32f103.soc.toml"), no_i2c).expect("write override");

    let spec = write_spec(
        &dir,
        "unexercised.toml",
        &format!(
            r#"
name        = "Unexercised I2C sensor must fail loudly"
board       = "{}"
firmware    = "{}"
duration_ms = 150
frame_ms    = 5.0

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

[[sensor.register]]
addr  = 0x01
const = [0x00]

[sensor.protocol]
style = "i2c_pointer"
"""

[sensor.inputs]
temperature_c = 40.0

# The trap assertion: the sensor's state field sits INSIDE this window even
# though the firmware never exchanged a byte with it. Pre-fix this was a
# false green; it must now FAIL with the never-exercised wording.
[[assert]]
kind  = "peripheral"
id    = "U2_lm75"
field = "temperature_c"
min   = 35.0
max   = 45.0
"#,
            board.canonicalize().unwrap().display(),
            fw.canonicalize().unwrap().display(),
        ),
    );

    std::env::set_var("HAUKSBEE_MCU_DIR", &dir);
    let result = run(&RunConfig { spec, ..Default::default() });
    std::env::remove_var("HAUKSBEE_MCU_DIR");
    let result = result.expect("ci run");
    let _ = std::fs::remove_dir_all(&dir);

    // The coverage warning reaches the result and every format.
    assert!(
        result
            .coverage_warnings
            .iter()
            .any(|w| w.contains("U2_lm75") && w.contains("models no I2C controller")),
        "the unexercised sensor must reach coverage_warnings: {:?}",
        result.coverage_warnings
    );
    assert!(result.render_human().contains("COVERAGE HOLE"));
    assert!(result.render_junit().contains("COVERAGE HOLE"));
    assert!(result.render_github_annotations().contains("COSIM COVERAGE HOLE"));

    // The peripheral assertion FAILS loudly instead of green-passing on the
    // sensor's untouched default state.
    let per = result
        .results
        .iter()
        .find(|r| r.kind == "peripheral")
        .expect("the peripheral assertion ran");
    assert!(
        !per.passed,
        "an unexercised sensor must fail its assertion: {}",
        per.detail
    );
    assert!(
        per.detail.contains("NEVER exercised"),
        "the failure must say why: {}",
        per.detail
    );
    assert!(!result.passed(), "the run is RED, not a false green");
}
