//! A crystal slower to start than the firmware's fixed start-up delay.
//!
//! The RP2040's `XOSC_STATUS.STABLE` is a counter: it rises after
//! `STARTUP.DELAY` x 256 crystal cycles whether or not the oscillation has
//! built up (RP2040 datasheet s2.16.3). Firmware that programs 1 ms against a
//! crystal that needs 10 ms switches the core onto a clock that is not there
//! yet; the readiness model records that read and halts the core, so GP25 never
//! rises and [`Mcu::clock_fault`] names the oscillator, the wait and the need.
//! A 64 ms delay against the same crystal runs normally, and so does the 1 ms
//! delay against a crystal whose start-up nobody stated (the 1 ms class
//! default the RP2040 reference design is proven at).
//!
//! Fixtures: `testdata/firmware/rp2040_xosc_startup` (`xosc_wait_1ms.elf`,
//! `xosc_wait_64ms.elf`).

use hauksbee_mcu::{ClockFault, CrystalStartup, Mcu, PinId, RenodeBackend, RenodeConfig};
use std::sync::{Arc, Mutex};

const LED_PIN: PinId = PinId { port: '0', bit: 25 };

/// Boot `elf` with crystal Y1 of `startup_us` (`stated` from a card or not),
/// run 100 ms, and return whether GP25 rose and the recorded clock fault.
fn run(elf: &str, startup_us: u64, stated: bool) -> Option<(bool, Option<ClockFault>)> {
    if !crate::support::renode_or_skip() {
        return None;
    }
    let Some(path) = crate::support::firmware(&format!("rp2040_xosc_startup/{elf}")) else {
        eprintln!("SKIP: rp2040_xosc_startup/{elf} not present");
        return None;
    };
    let config = RenodeConfig::rp2040()
        .with_external_clock_present(true)
        .with_crystal_startup(Some(CrystalStartup {
            reference: "Y1".into(),
            startup_us,
            stated,
        }));
    let mut mcu = RenodeBackend::new(config).expect("spawn Renode RP2040");
    mcu.load_firmware(&path).expect("load firmware");
    let rose = Arc::new(Mutex::new(false));
    let seen = rose.clone();
    mcu.on_pin_change(Box::new(move |pin, high, _| {
        if pin == LED_PIN && high {
            *seen.lock().unwrap() = true;
        }
    }));
    for _ in 0..100 {
        mcu.run_micros(1_000).expect("run chunk");
    }
    let rose = *rose.lock().unwrap();
    Some((rose, mcu.clock_fault()))
}

#[test]
fn a_1ms_wait_on_a_10ms_crystal_halts_with_a_clock_fault() {
    let Some((rose, fault)) = run("xosc_wait_1ms.elf", 10_000, true) else {
        return;
    };
    assert!(
        !rose,
        "the core must not run on past a crystal that has not started"
    );
    let fault = fault.expect("the premature STABLE read is recorded");
    assert_eq!(fault.oscillator, "xosc");
    assert!(
        (900.0..2_000.0).contains(&fault.waited_us),
        "the firmware waited about 1 ms: {fault:?}"
    );
    assert!((fault.needed_us - 10_000.0).abs() < 1e-6, "{fault:?}");
}

#[test]
fn a_64ms_wait_on_a_10ms_crystal_runs() {
    let Some((rose, fault)) = run("xosc_wait_64ms.elf", 10_000, true) else {
        return;
    };
    assert!(
        rose,
        "the crystal started inside the 64 ms delay: GP25 rises"
    );
    assert_eq!(fault, None);
}

#[test]
fn a_1ms_wait_on_the_assumed_1ms_crystal_runs() {
    let Some((rose, fault)) = run("xosc_wait_1ms.elf", 1_000, false) else {
        return;
    };
    assert!(rose, "the SDK delay meets the class default: GP25 rises");
    assert_eq!(fault, None);
}
