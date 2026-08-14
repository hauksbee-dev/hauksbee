//! Live proof that ESP32 GPIO output no longer requires cooperating firmware.
//!
//! The fixture is an ordinary ESP-IDF app: it never reads or writes Hauksbee's
//! RTC-slow-RAM mailbox. This test therefore passes only when the emulator's
//! real GPIO_OUT state is retained and the backend capability-probes that path.
//! Ordinary all-targets runs skip honestly on the supported upstream mailbox
//! build. `scripts/test-qemu-gpio-source-patch.sh` sets the fail-closed
//! `HAUKSBEE_REQUIRE_PATCHED_QEMU=1` acceptance mode after verifying the exact
//! source install.

#![cfg(feature = "qemu")]

use hauksbee_mcu::qemu::{is_available, mailbox, GpioOutputObservation, QemuArch, QemuBackend};
use hauksbee_mcu::{Mcu, PinId};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

fn fixture() -> Option<PathBuf> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/firmware/esp32_native_gpio/flash.bin");
    path.exists().then(|| path.canonicalize().unwrap_or(path))
}

#[test]
fn unmodified_esp_idf_gpio_reaches_the_backend_without_a_mailbox() {
    let required = std::env::var("HAUKSBEE_REQUIRE_PATCHED_QEMU").as_deref() == Ok("1");
    if !is_available(QemuArch::Xtensa) {
        assert!(
            !required,
            "HAUKSBEE_REQUIRE_PATCHED_QEMU=1, but qemu-system-xtensa is not installed"
        );
        eprintln!("SKIP: Espressif qemu-system-xtensa is not installed");
        return;
    }
    let Some(flash) = fixture() else {
        assert!(
            !required,
            "HAUKSBEE_REQUIRE_PATCHED_QEMU=1, but the committed no-mailbox flash fixture is missing"
        );
        eprintln!("SKIP: build testdata/firmware/esp32_native_gpio/flash.bin with ./build.sh");
        return;
    };

    let mut mcu = QemuBackend::esp32(&flash).expect("boot ordinary ESP-IDF fixture");
    if mcu.gpio_output_observation() != GpioOutputObservation::PeripheralRegisters {
        assert!(
            !required,
            "HAUKSBEE_REQUIRE_PATCHED_QEMU=1, but the selected binary lacks the paired \
             gpio-out/gpio-enable capability"
        );
        eprintln!(
            "SKIP: selected Espressif QEMU is the supported mailbox-fallback build; \
             set HAUKSBEE_QEMU_XTENSA to the reviewed patched binary and \
             HAUKSBEE_REQUIRE_PATCHED_QEMU=1 for the F11 acceptance gate"
        );
        return;
    }

    // The negative proof: neither the old output word nor its opt-in magic is
    // present. Any edge below therefore came from GPIO MMIO, not hidden fixture
    // cooperation.
    assert_eq!(mcu.debug_read_u32(mailbox::GPIO_OUT).unwrap(), 0);
    assert_eq!(mcu.debug_read_u32(mailbox::MAGIC).unwrap(), 0);
    assert!(
        mcu.drive_direction_observable(),
        "the patched capability must retain GPIO ENABLE as well as OUT"
    );

    let edges: Arc<Mutex<Vec<(PinId, bool)>>> = Arc::new(Mutex::new(Vec::new()));
    let seen = edges.clone();
    mcu.on_pin_change(Box::new(move |pin, high, _cycle| {
        seen.lock().unwrap().push((pin, high));
    }));

    // Boot under the conservative 50 ms floor, then oversample the fixture's
    // 100 ms GPIO4 toggle. First peripheral/UART activity retires the floor
    // without relying on mailbox MAGIC.
    for _ in 0..50 {
        mcu.run_micros(20_000).expect("advance patched QEMU");
    }

    let got = edges.lock().unwrap();
    assert!(
        got.iter()
            .any(|(pin, high)| pin.port == '0' && pin.bit == 2 && *high),
        "GPIO2's steady real-driver HIGH never reached the backend: {got:?}"
    );
    assert!(
        got.iter()
            .any(|(pin, high)| pin.port == '0' && pin.bit == 4 && *high)
            && got
                .iter()
                .any(|(pin, high)| pin.port == '0' && pin.bit == 4 && !*high),
        "GPIO4 must produce both real peripheral levels: {got:?}"
    );
    assert!(
        mcu.boot_complete(),
        "real GPIO/UART activity must retire the boot floor without mailbox MAGIC"
    );
    let outputs = mcu.pins_configured_output();
    assert!(
        outputs.contains(&PinId { port: '0', bit: 2 })
            && outputs.contains(&PinId { port: '0', bit: 4 }),
        "GPIO2 and GPIO4 must be observed as real configured outputs: {outputs:?}"
    );
    assert_eq!(mcu.debug_read_u32(mailbox::GPIO_OUT).unwrap(), 0);
    assert_eq!(mcu.debug_read_u32(mailbox::MAGIC).unwrap(), 0);
}
