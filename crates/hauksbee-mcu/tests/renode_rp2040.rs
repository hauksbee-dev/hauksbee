//! RP2040 co-sim, proven against real pico-sdk firmware.
//!
//! # What changed, and why this is no longer skip-gated
//!
//! This test used to skip with "the installed Renode ships no rp2040 platform",
//! which was true and is still true: Renode 1.16.1 carries no RP2040, and
//! neither does Renode `master`. The RP2040 peripheral models now travel with
//! hauksbee instead (`db/mcu/rp2040/`, vendored MIT, compiled by Renode at run
//! time through the support-bundle mechanism in `src/renode/support.rs`), so the
//! platform no longer depends on what the local Renode install happens to
//! contain. The only remaining gate is whether Renode itself is installed.
//!
//! # The firmware is real
//!
//! Both images are stock pico-sdk 2.1.1 builds linked for flash, committed under
//! `testdata/firmware/rp2040_*`, with sources and build instructions beside
//! them. They boot through the real RP2040 boot ROM image, run the SDK's own
//! `runtime_init` (resets, XOSC, both PLLs, the clock muxes, the timer, the ROM
//! function table) and reach `main`. Nothing here is a hand-assembled register
//! poke: the previous revision of this file built a 32-byte Thumb image that
//! wrote SIO directly, which proved the register offsets and nothing about
//! whether an SDK firmware can boot at all.
//!
//! # Two-sided by construction
//!
//! `rp2040_blink_uart` prints over UART0 and toggles GP25.
//! `rp2040_quiet` is the same SDK runtime and the same UART chatter with the
//! GPIO calls removed. The toggle assertion must pass on the first and fail on
//! the second, so a green result cannot come from the harness inventing edges.

#![cfg(feature = "renode")]

use hauksbee_mcu::renode::is_available;
use hauksbee_mcu::{Mcu, PinId, RenodeBackend, RenodeConfig};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// GP25 is the Pico's on-board LED pin, and the pin both firmwares agree on:
/// one drives it, the other deliberately does not.
const LED_PIN: PinId = PinId { port: '0', bit: 25 };

fn firmware(dir: &str, name: &str) -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/firmware")
        .join(dir)
        .join(name);
    p.exists().then(|| p.canonicalize().unwrap_or(p))
}

/// Bring up an RP2040 with one of the bundled pico-sdk images, or skip.
///
/// Renode has to compile ~380 KB of C# for the support bundle before the
/// platform parses, which takes a few seconds on first machine creation. That
/// fits inside the backend's 60 s setup timeout on an idle host, so a slow
/// first test is not a hang. It does NOT fit under heavy contention: measured at
/// load average 248, bring-up overran the former 30 s budget, both in parallel
/// and with `--test-threads=1`, a different test each run. The production setup
/// allowance is still finite, and runtime polling returns to the tighter 30 s
/// bound. Read a startup red here against the host's load first.
macro_rules! rp2040_or_skip {
    ($name:ident, $dir:literal, $elf:literal) => {
        if !is_available() {
            eprintln!("SKIP: Renode not installed");
            return;
        }
        let Some(elf) = firmware($dir, $elf) else {
            eprintln!(
                "SKIP: {}/{} not present (see its CMakeLists.txt to rebuild)",
                $dir, $elf
            );
            return;
        };
        let mut $name = RenodeBackend::new(RenodeConfig::rp2040()).expect("spawn Renode RP2040");
        $name.load_firmware(&elf).expect("load pico-sdk ELF");
    };
}

/// Boot evidence: the SDK's `stdio_init_all` + `printf` reach the host, which
/// means the whole runtime_init chain (resets, XOSC, PLLs, clock muxes, UART
/// clock, IO_BANK0 function select for GP0) completed and `main` ran.
#[test]
fn rp2040_sdk_firmware_reaches_main_and_prints() {
    rp2040_or_skip!(mcu, "rp2040_blink_uart", "blink_uart.elf");

    let uart: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = uart.clone();
    mcu.on_uart(Box::new(move |b| sink.lock().unwrap().push(b)));

    for _ in 0..6 {
        mcu.run_micros(50_000).expect("run chunk");
    }

    let text = String::from_utf8_lossy(&uart.lock().unwrap()).to_string();
    assert!(
        text.contains("hauksbee rp2040: main reached"),
        "the pico-sdk runtime must reach main() and print over UART0; got: {text:?}"
    );
    // And the loop is running, not merely entered once: the banner is followed by
    // the per-iteration lines.
    assert!(
        text.contains("led on 0"),
        "main()'s loop must run past the banner; got: {text:?}"
    );
}

/// The positive half of the two-sided proof: real firmware driving GP25 through
/// the SDK's `gpio_put` produces engine-facing edges, synthesised from the
/// backend's poll of SIO `GPIO_OUT`.
#[test]
fn rp2040_led_toggles_are_observed() {
    rp2040_or_skip!(mcu, "rp2040_blink_uart", "blink_uart.elf");

    let edges: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
    let counter = edges.clone();
    mcu.on_pin_change(Box::new(move |pin, _high, _cycle| {
        if pin == LED_PIN {
            *counter.lock().unwrap() += 1;
        }
    }));

    // The firmware's period is sleep_ms(20) per half cycle, so 25 edges per
    // 500 ms of virtual time at the limit. Poll boundaries are 20 ms, i.e. the
    // same order as the toggle period, so some edges coalesce inside a chunk;
    // the assertion is "clearly toggling", not an exact count.
    for _ in 0..25 {
        mcu.run_micros(20_000).expect("run chunk");
    }

    let n = *edges.lock().unwrap();
    assert!(
        n >= 4,
        "GP25 is driven by gpio_put() on every loop iteration, so the SIO \
         GPIO_OUT poll must synthesise several edges in 500 ms of virtual time; \
         got {n}"
    );
}

/// The negative half. Same runtime, same UART traffic, no GPIO calls: the toggle
/// assertion above must have nothing to fire on. Without this, a harness that
/// invented edges (or a GPIO_OUT read that returned a stuck non-zero) would sail
/// through the positive test.
#[test]
fn rp2040_quiet_firmware_produces_no_led_edges() {
    rp2040_or_skip!(mcu, "rp2040_quiet", "quiet.elf");

    let uart: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let uart_sink = uart.clone();
    mcu.on_uart(Box::new(move |b| uart_sink.lock().unwrap().push(b)));

    let seen: Arc<Mutex<Vec<PinId>>> = Arc::new(Mutex::new(Vec::new()));
    let pins = seen.clone();
    mcu.on_pin_change(Box::new(move |pin, _high, _cycle| {
        pins.lock().unwrap().push(pin);
    }));

    for _ in 0..25 {
        mcu.run_micros(20_000).expect("run chunk");
    }

    // The control must actually have RUN; a firmware that failed to boot would
    // also produce no edges, and would pass this test for the wrong reason.
    let text = String::from_utf8_lossy(&uart.lock().unwrap()).to_string();
    assert!(
        text.contains("quiet 0"),
        "the control firmware must boot and print, otherwise 'no edges' proves \
         nothing; got: {text:?}"
    );

    let observed = seen.lock().unwrap().clone();
    assert!(
        !observed.contains(&LED_PIN),
        "the control firmware never calls gpio_put, so no GP25 edge may be \
         reported; got {observed:?}"
    );
}

/// Drive direction, live. The descriptor claims SIO `GPIO_OE` (offset 0x20, one
/// bit per pin) is readable; this is what makes a held-LOW output
/// distinguishable from a floating input. The firmware calls
/// `gpio_set_dir(25, GPIO_OUT)` and touches nothing else, so GP25 must appear in
/// the output set and its neighbours must not.
#[test]
fn rp2040_reports_drive_direction_from_gpio_oe() {
    rp2040_or_skip!(mcu, "rp2040_blink_uart", "blink_uart.elf");

    assert!(
        mcu.drive_direction_observable(),
        "the RP2040 descriptor carries a dir map for the SIO bank, so direction \
         must be reported as observable"
    );

    // Boot far enough for main() to have configured the pin.
    for _ in 0..6 {
        mcu.run_micros(50_000).expect("run chunk");
    }

    let outputs = mcu.pins_configured_output();
    assert!(
        outputs.contains(&LED_PIN),
        "GP25 is set to GPIO_OUT by the firmware, so GPIO_OE bit 25 must decode \
         to an output; got {outputs:?}"
    );
    assert!(
        !outputs.contains(&PinId { port: '0', bit: 26 }),
        "GP26 is never configured, so it must not read as an output; got {outputs:?}"
    );
}

/// Sanity on the descriptor's own honesty claims, so the capability table in
/// `rp2040.soc.toml` cannot drift away from the shipped config without a test
/// noticing. These are cheap and need no Renode.
#[test]
fn rp2040_descriptor_matches_its_documented_tiers() {
    let cfg = RenodeConfig::rp2040();
    assert_eq!(cfg.support_bundle.as_deref(), Some("rp2040"));
    assert!(
        cfg.platform.contains("{support}"),
        "the platform reference must resolve inside the unpacked bundle"
    );
    assert!(
        cfg.extra_setup.iter().any(|c| c.contains("bootrom.elf")),
        "the boot ROM must be loaded: the SDK runtime calls into its function table"
    );
    // I2C is documented as proven; SPI as impossible with the vendored PL022
    // (see renode_rp2040_bus.rs). A listed controller silently installs a
    // bridge, so an SPI entry appearing here is a regression in honesty, not a
    // feature.
    assert_eq!(cfg.i2c_controllers, vec!["i2c0", "i2c1"]);
    assert!(
        cfg.spi_controllers.is_empty(),
        "SPI cannot be bridged through this platform's PL022 model"
    );
    // ADC0..ADC3 are mapped; input 4 (the on-die temperature sensor) must not
    // be, because it is not a circuit node.
    assert_eq!(cfg.adc_channels.len(), 4);
    let channels: Vec<u8> = cfg.adc_channels.iter().map(|m| m.channel).collect();
    assert_eq!(channels, vec![0, 1, 2, 3]);

    let bank = cfg.ports.first().expect("one SIO bank");
    assert_eq!(bank.peripheral, "sio");
    assert_eq!(bank.odr_offset, 0x10, "SIO GPIO_OUT");
    assert_eq!(bank.width, 30);
    let dir = bank.dir.expect("SIO GPIO_OE dir map");
    assert_eq!(dir.offset, 0x20, "SIO GPIO_OE");
}

/// An unknown bundle name must be refused when the descriptor loads, not when
/// Renode has already been spawned.
#[test]
fn unknown_support_bundle_is_refused_at_load() {
    let src = include_str!("../db/mcu/rp2040.soc.toml")
        .replace("support_bundle = \"rp2040\"", "support_bundle = \"rp2041\"");
    let err = RenodeConfig::from_soc_toml(&src).expect_err("must refuse an unknown bundle");
    let msg = err.to_string();
    assert!(
        msg.contains("rp2041") && msg.contains("rp2040"),
        "the error must name both the bad bundle and what is available; got {msg:?}"
    );
}
