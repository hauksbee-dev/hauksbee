//! I2C peripheral interception proof: ESP32 / Espressif-QEMU backend.
//!
//! This is the QEMU analogue of `i2c_sensor_cosim.rs`, which already passes on
//! AVR/simavr. What carries these tests is the EMULATED-DEVICE path: the ESP32
//! machine ships a tmp105 at 0x48 on i2c0, the scheduler pushes the modeled
//! LM75's temperature into it via `set_i2c_device_temperature` each chunk, and
//! the firmware reads it through its real I2C controller. That is the §5.3
//! preferred shape (a real peripheral emulation over a mailbox contract).
//!
//! The backend's `on_i2c` byte-callback hook is wired separately through the
//! RAM-mailbox bus contract (05-cosim-fidelity §5.2, regression-tested in
//! `hauksbee-mcu/tests/qemu_bus_mailbox.rs`); it does not participate here
//! because this firmware drives its real I2C controller, whose byte traffic
//! Espressif QEMU does not surface to the host.
//!
//! ## What is being tested
//!
//! The firmware `testdata/firmware/esp32_i2c_thermostat/flash.bin` runs on an
//! Espressif-QEMU-emulated ESP32. It polls an LM75 at I2C address 0x48 over the
//! ESP32 hardware I2C0 peripheral (GPIO21=SDA, GPIO22=SCL) and drives GPIO5 HIGH
//! when the reported temperature is >= 30 C, LOW otherwise. hauksbee attaches an
//! in-process `Lm75` slave to an `I2cBus` and registers it with the co-sim.
//! When the bridge is complete:
//!   - At 20 C the FLAG net must be LOW  (< 1 V).
//!   - At 40 C the FLAG net must be HIGH (> 2 V; the ESP32 GPIO drives 3.3 V
//!     through a 4k7 pull-down to GND on the FLAG net).
//!   - Across the sweep [10, 25, 29, 31, 35, 50, 28, 15] the GPIO must track
//!     the 30 C threshold exactly.
//!
//! ## Board layout
//!
//! Minimal board: U1 ESP32-WROOM-32, power/ground, I2C bus pair on GPIO21/GPIO22
//! (pads from the devkit module footprint), and a FLAG net on GPIO5 with a 4k7
//! pull-down. The LM75 is intercepted in-process; the SDA/SCL pads are wired for
//! board realism only (matching the AVR test pattern).
//!
//! ## Firmware
//!
//! `testdata/firmware/esp32_i2c_thermostat/flash.bin`, built with
//! `./build.sh` (requires esp-idf v5.x). Source in that directory.
//!
//! ## Running
//!
//! ```
//! cargo test -p hauksbee-engine --features qemu i2c_qemu -- --nocapture
//! ```
//! Requires Espressif QEMU installed at `~/.hauksbee-qemu-esp/`.

#![cfg(feature = "qemu")]

use hauksbee_engine::HauksbeeEngine;
use hauksbee_mcu::qemu::{is_available, QemuArch};
use std::path::PathBuf;

/// Minimal board: U1 ESP32-WROOM-32, power/ground on pads 1/2, I2C bus on
/// pads for GPIO21/GPIO22, and GPIO5 ("FLAG") with a 4k7 pull-down to ground.
/// The hauksbee QEMU backend observes pin levels through the `hauksbee_gpio_out`
/// mailbox word written by the firmware (same mechanism as the blinky demo).
const BOARD: &str = r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+3V3")
  (net 3 "FLAG")
  (net 4 "SDA")
  (net 5 "SCL")
  (net 6 "GPIO2_OUT")

  (module Package_RF:ESP32-WROOM-32 (layer F.Cu)
    (at 100 100)
    (fp_text reference U1 (at 0 0) (layer F.SilkS))
    (fp_text value ESP32-WROOM-32 (at 0 2) (layer F.Fab))
    (pad 1  smd rect (at -3 0) (net 1 "GND"))
    (pad 2  smd rect (at -3 1) (net 2 "+3V3"))
    (pad 24 smd rect (at 3 0) (net 6 "GPIO2_OUT"))
    (pad 29 smd rect (at 3 1) (net 3 "FLAG"))
    (pad 33 smd rect (at 3 2) (net 4 "SDA"))
    (pad 34 smd rect (at 3 3) (net 5 "SCL"))
    (pad 38 smd rect (at -3 2) (net 1 "GND"))
    (pad 39 smd rect (at -3 3) (net 2 "+3V3"))
  )

  (module Resistor_THT:R_Axial_DIN0207_L6.3mm_D2.5mm (layer F.Cu)
    (at 110 100)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value 4k7 (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (net 3 "FLAG"))
    (pad 2 thru_hole circle (at 2 0) (net 1 "GND"))
  )
)
"#;

fn flash_image() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/firmware/esp32_i2c_thermostat/flash.bin")
}

/// Fail when the QEMU/MCU transport broke during the run: a stale FLAG voltage
/// must not make an integration test green.
fn assert_transport_valid(engine: &HauksbeeEngine) {
    assert!(
        engine.scheduler().analog_valid(),
        "QEMU/MCU transport failed during the run; stale FLAG voltage must not \
         make this integration test green: {:?}",
        engine.scheduler().failed_window_diagnoses()
    );
}

/// Run the firmware for `ms` milliseconds against an LM75 fixed at `temp_c`.
/// Returns the final "FLAG" net voltage.
fn run_at_temp(temp_c: f64, ms: u32) -> f64 {
    let (mut engine, _bus) = crate::support::lm75_engine(BOARD, &flash_image(), temp_c);
    let n = usize::try_from(ms / 5).expect("frame count fits").max(1); // 5 ms frames
    let last_flag = crate::support::last_flag(&mut engine, 5e-3, n);
    assert_transport_valid(&engine);
    last_flag
}

/// The core proof: the ESP32 firmware reads the LM75 via I2C and drives GPIO5
/// / "FLAG" HIGH when >= 30 C, LOW otherwise.
///
/// The modeled temperature reaches the firmware through the machine's own
/// emulated tmp105 (`set_i2c_device_temperature` each chunk); if that push
/// path breaks, the firmware reads 0xFF bytes, decodes them as ~-0.125 C, and
/// never sets the flag, so this fails with a clear assertion message.
#[test]
fn esp32_i2c_firmware_drives_gpio_from_temperature() {
    hauksbee_testkit::skip_if!(
        !is_available(QemuArch::Xtensa),
        "Espressif QEMU (qemu-system-xtensa) not installed"
    );
    let fw = flash_image();
    assert!(
        fw.exists(),
        "build the firmware first: ./build.sh in testdata/firmware/esp32_i2c_thermostat ({fw:?})"
    );

    // Below threshold: FLAG must be LOW.
    let cold = run_at_temp(20.0, 1000);
    assert!(
        cold < 1.0,
        "ESP32 at 20 C: FLAG net should be LOW (< 1 V); got {cold:.3} V."
    );

    // Above threshold: FLAG must be HIGH.
    let hot = run_at_temp(40.0, 1000);
    assert!(
        hot > 2.0,
        "ESP32 at 40 C: FLAG net should be HIGH (> 2 V); got {hot:.3} V. \
         This is the primary proof: the QEMU I2C bridge must deliver real \
         LM75 bytes so the firmware drives GPIO5 HIGH."
    );
}

/// Sweep test: GPIO must track the 30 C threshold across the full sweep.
#[test]
fn esp32_i2c_gpio_follows_temperature_sweep() {
    hauksbee_testkit::skip_if!(
        !is_available(QemuArch::Xtensa),
        "Espressif QEMU (qemu-system-xtensa) not installed"
    );
    let fw = flash_image();
    assert!(fw.exists(), "firmware missing: {fw:?}");
    crate::support::assert_flag_follows_lm75_sweep("ESP32", 1.5, |t| run_at_temp(t, 800));
}

/// Live mutation test: raise temperature from cold to hot mid-run (400 ms each,
/// enough for multiple I2C poll cycles at the 50 ms rate) and assert the flag
/// transitions. This probes whether the I2C bridge delivers fresh bytes on
/// every transaction (not cached from spawn).
#[test]
fn esp32_i2c_temperature_change_is_live() {
    hauksbee_testkit::skip_if!(
        !is_available(QemuArch::Xtensa),
        "Espressif QEMU (qemu-system-xtensa) not installed"
    );
    let fw = flash_image();
    assert!(fw.exists(), "firmware missing: {fw:?}");
    let engine = crate::support::assert_lm75_change_is_live(BOARD, &fw, 80, "QEMU");
    assert_transport_valid(&engine);
}
