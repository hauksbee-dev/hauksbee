//! The STM32F072 example descriptor: the tier-B walkthrough's own proof.
//!
//! `db/mcu/examples/stm32f072.soc.toml` is the file
//! `docs/extending/add-a-microcontroller.md` walks a reader through writing, for
//! a part hauksbee does not ship on a Renode platform that already exists. This
//! suite is what the guide points at when it says a contribution needs a
//! two-sided test, so it has to hold itself to that bar.
//!
//! Two layers, deliberately separated:
//!
//!   1. Descriptor validation with NO emulator. Loads, validates, and pins the
//!      register offsets that make the difference between observing GPIO and
//!      observing nothing. Runs in milliseconds on every machine, which is the
//!      loop a descriptor author actually iterates in.
//!   2. A live Renode boot, two-sided: the firmware that drives PC6 and prints
//!      on USART1 produces edges and bytes, and the firmware built from the same
//!      source that drives nothing produces neither. Without the negative half,
//!      a bridge that invented edges would pass.
//!
//! The live half skips when Renode is absent or the fixture ELFs are not built,
//! and says which, so a skip is never mistaken for a pass.

#![cfg(feature = "renode")]

use hauksbee_mcu::renode::is_available;
use hauksbee_mcu::{Mcu, PinId, RenodeBackend, RenodeConfig};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// The example descriptor, read through the same `include_str!` the shipped
/// built-ins use, so a syntax error is a compile error rather than a test that
/// silently reads nothing.
const F072: &str = include_str!("../db/mcu/examples/stm32f072.soc.toml");

fn firmware(name: &str) -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/firmware/stm32f072_blinky")
        .join(name);
    if p.exists() {
        Some(p.canonicalize().unwrap_or(p))
    } else {
        None
    }
}

// ── (1) No emulator needed ───────────────────────────────────────────────────

/// The example loads and carries the values the walkthrough claims it does.
///
/// Every assertion here is a value that fails SILENTLY when it is wrong: a bad
/// `odr_offset` reads a register that is not the output data register and
/// reports zero edges forever, and a bad `dir` offset reports a mask of zero,
/// which suppresses every edge instead of erroring.
#[test]
fn f072_example_descriptor_loads_with_the_verified_offsets() {
    let c = RenodeConfig::from_soc_toml(F072).expect("the example descriptor must load");

    assert_eq!(c.machine, "f072");
    assert_eq!(c.platform, "@platforms/cpus/stm32f072.repl");
    assert_eq!(c.cpu, "sysbus.cpu");
    assert_eq!(c.uart.as_deref(), Some("sysbus.usart1"));
    assert_eq!(c.mcu_label, "STM32F072 (ARM Cortex-M0)");
    // A stock platform, so no support bundle: tier B's whole point.
    assert_eq!(c.support_bundle, None);
    // A single-line platform path, not inline `.repl` source.
    assert!(!c.platform.contains('\n'));

    // Six ports, A..F, all at the F0/F4 layout. 0x0C here would be the F1's
    // offset and would observe the wrong register on this part.
    let letters: Vec<char> = c.ports.iter().map(|p| p.letter).collect();
    assert_eq!(letters, vec!['A', 'B', 'C', 'D', 'E', 'F']);
    for port in &c.ports {
        assert_eq!(
            port.odr_offset, 0x14,
            "port {} must read ODR at the F0/F4 offset",
            port.letter
        );
        assert_eq!(port.width, 16);
        let dir = port
            .dir
            .expect("every F072 port carries a verified MODER map");
        assert_eq!(dir.offset, 0x00);
        assert_eq!(dir.encoding, hauksbee_mcu::renode::DirEncoding::Moder);
        assert_eq!(port.peripheral, format!("gpioPort{}", port.letter));
    }

    // The ADC map: eight external channels, fed in millivolts because the
    // model's SetDefaultValue takes millivolts.
    assert_eq!(c.adc_channels.len(), 8);
    for (i, ch) in c.adc_channels.iter().enumerate() {
        assert_eq!(ch.channel, i as u8);
        assert_eq!(ch.max_count, 4095);
        assert!((ch.full_scale_volts - 3.3).abs() < 1e-9);
        match &ch.inject {
            hauksbee_mcu::renode::AdcInject::MonitorCommand(cmd) => {
                assert!(
                    cmd.contains("{millivolts}"),
                    "channel {i} must be fed in millivolts, not raw counts: {cmd}"
                );
                assert!(cmd.ends_with(&format!(" {i}")), "channel {i} feed: {cmd}");
            }
            other => panic!("channel {i} must use a Monitor feed, got {other:?}"),
        }
    }

    // Unverified buses stay EMPTY. Naming a controller whose model never
    // dispatches to a registered slave makes a bound sensor answer zeroes and
    // read as working; empty makes it a loud coverage warning instead.
    assert!(c.i2c_controllers.is_empty());
    assert!(c.spi_controllers.is_empty());
    assert_eq!(c.spi_extra_repl, None);
}

/// The example resolves through the product path when it is installed as an
/// override, which is how a reader of the walkthrough actually uses it: no
/// recompile, no entry in the embedded built-in list.
///
/// `stm32f072` is resolved by no other test in this binary, so the
/// `HAUKSBEE_MCU_DIR` write here cannot collide with a parallel resolve of a
/// different part.
#[test]
fn f072_example_resolves_from_an_override_dir() {
    let dir = std::env::temp_dir().join(format!(
        "hauksbee-f072-example-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("stm32f072.soc.toml"), F072).unwrap();

    // SAFETY (edition 2021): set_var is safe.
    std::env::set_var("HAUKSBEE_MCU_DIR", &dir);
    let resolved = hauksbee_mcu::SocConfig::resolve("renode:stm32f072");
    std::env::remove_var("HAUKSBEE_MCU_DIR");
    std::fs::remove_dir_all(&dir).ok();

    match resolved.expect("an installed example must resolve") {
        hauksbee_mcu::SocConfig::Renode(c) => assert_eq!(c.machine, "f072"),
        #[allow(unreachable_patterns)]
        other => panic!("expected a Renode config, got {other:?}"),
    }
}

// ── (2) Live Renode, two-sided ───────────────────────────────────────────────

/// Bring up an F072 from the example descriptor with `name`d firmware, or skip
/// with the reason.
macro_rules! f072_or_skip {
    ($mcu:ident, $elf:literal) => {
        if !is_available() {
            eprintln!("SKIP: Renode not installed (hauksbee install renode)");
            return;
        }
        let Some(elf) = firmware($elf) else {
            eprintln!(
                "SKIP: {} not built (run make in testdata/firmware/stm32f072_blinky)",
                $elf
            );
            return;
        };
        let config = RenodeConfig::from_soc_toml(F072).expect("load the example descriptor");
        let mut $mcu = RenodeBackend::new(config).expect("spawn Renode with the F072 platform");
        $mcu.load_firmware(&elf).expect("load the fixture ELF");
    };
}

/// The positive half: driving firmware produces the edges and bytes the
/// descriptor's offsets are supposed to expose.
#[test]
fn f072_driving_firmware_toggles_pc6_and_prints() {
    f072_or_skip!(mcu, "blinky.elf");

    let pc6 = PinId { port: 'C', bit: 6 };
    let edges: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
    let counter = edges.clone();
    mcu.on_pin_change(Box::new(move |pin, _high, _cycle| {
        if pin == pc6 {
            *counter.lock().unwrap() += 1;
        }
    }));
    let uart: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = uart.clone();
    mcu.on_uart(Box::new(move |b| sink.lock().unwrap().push(b)));

    for _ in 0..12 {
        mcu.run_micros(50_000).expect("run a 50 ms chunk");
    }

    let n = *edges.lock().unwrap();
    assert!(
        n >= 2,
        "PC6 must be seen toggling through the mapped ODR offset; got {n} edges"
    );
    let text = String::from_utf8_lossy(&uart.lock().unwrap()).to_string();
    assert!(
        text.contains("hello from stm32f072"),
        "USART1 must reach the host; got {text:?}"
    );

    // Direction is observable because every port carries a MODER map, and the
    // pins the firmware configured are exactly the ones reported.
    assert!(mcu.drive_direction_observable());
    let outputs = mcu.pins_configured_output();
    let has = |port: char, bit: u8| outputs.iter().any(|p| p.port == port && p.bit == bit);
    assert!(has('C', 6), "PC6 is a configured output; got {outputs:?}");
    assert!(has('A', 5), "PA5 is a configured output; got {outputs:?}");
    // PA9/PA10 are USART1 in ALTERNATE FUNCTION mode. `moder` counts only
    // 0b01, so they must NOT appear: an AF pin can be an input function.
    assert!(
        !has('A', 9) && !has('A', 10),
        "alternate-function pins are not general-purpose drives; got {outputs:?}"
    );
    // And a pin nothing touched stays out.
    assert!(!has('B', 0), "untouched pins must not read as outputs");
}

/// The negative half, and the reason this file is a proof rather than a demo:
/// firmware built from the same source that configures nothing and drives
/// nothing must produce NO edges, NO configured outputs and NO UART bytes. A
/// bridge that fabricated activity would pass the test above and fail here.
#[test]
fn f072_quiet_firmware_produces_nothing() {
    f072_or_skip!(mcu, "quiet.elf");

    let edges: Arc<Mutex<Vec<PinId>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = edges.clone();
    mcu.on_pin_change(Box::new(move |pin, _high, _cycle| {
        sink.lock().unwrap().push(pin);
    }));
    let uart: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let bytes = uart.clone();
    mcu.on_uart(Box::new(move |b| bytes.lock().unwrap().push(b)));

    for _ in 0..12 {
        mcu.run_micros(50_000).expect("run a 50 ms chunk");
    }

    assert!(
        edges.lock().unwrap().is_empty(),
        "a firmware that drives nothing must produce no edges; got {:?}",
        edges.lock().unwrap()
    );
    assert!(
        uart.lock().unwrap().is_empty(),
        "a firmware that prints nothing must produce no UART bytes; got {:?}",
        String::from_utf8_lossy(&uart.lock().unwrap())
    );
    assert!(
        mcu.pins_configured_output().is_empty(),
        "a firmware that configures no pin must report no configured outputs; got {:?}",
        mcu.pins_configured_output()
    );
}

/// ADC injection, both directions of the claim: an engine-pushed voltage
/// reaches the converter and the firmware's own ADC read returns the matching
/// 12-bit code, at two voltages on one running machine, and on a second channel
/// without disturbing the first.
///
/// This is the first shipped Renode part whose ADC map rides a STOCK platform's
/// own converter model rather than a vendored one, so the claim is worth
/// checking rather than assuming.
#[test]
fn f072_adc_injection_reaches_the_firmware() {
    f072_or_skip!(mcu, "blinky.elf");

    let uart: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = uart.clone();
    mcu.on_uart(Box::new(move |b| sink.lock().unwrap().push(b)));

    // Boot far enough to be in the command loop.
    for _ in 0..3 {
        mcu.run_micros(50_000).expect("run a 50 ms chunk");
    }

    // Ask for one conversion per fed voltage. 'a' reads channel 0, 'b' reads
    // channel 3, and the firmware answers "adc<ch>=<8 hex digits>".
    let ask = |mcu: &mut RenodeBackend, cmd: u8| -> String {
        uart.lock().unwrap().clear();
        mcu.uart_write(&[cmd]);
        for _ in 0..4 {
            mcu.run_micros(50_000).expect("run a 50 ms chunk");
        }
        String::from_utf8_lossy(&uart.lock().unwrap()).to_string()
    };

    mcu.set_analog_in(0, 1.65);
    let half = ask(&mut mcu, b'a');
    assert!(
        half.contains("adc0=00000800"),
        "1.65 V of a 3.3 V full scale is count 2048 (0x800); got {half:?}"
    );

    mcu.set_analog_in(0, 0.825);
    let quarter = ask(&mut mcu, b'a');
    assert!(
        quarter.contains("adc0=00000400"),
        "0.825 V is count 1024 (0x400); got {quarter:?}"
    );

    mcu.set_analog_in(3, 3.3);
    let full = ask(&mut mcu, b'b');
    assert!(
        full.contains("adc3=00000fff"),
        "full scale on channel 3 is count 4095 (0xfff); got {full:?}"
    );

    // Channel 0 kept its own value: the channels are not one shared sample.
    let still = ask(&mut mcu, b'a');
    assert!(
        still.contains("adc0=00000400"),
        "feeding channel 3 must not disturb channel 0; got {still:?}"
    );
}
