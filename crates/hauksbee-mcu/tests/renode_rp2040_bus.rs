//! RP2040 bus-slave bridging, live: I2C round-trips, SPI cannot.
//!
//! # I2C: proven end to end
//!
//! `I2C.RP2040I2C` is a `SimpleContainer<II2CPeripheral>` that dispatches to the
//! child registered at the slave address, which is exactly the shape hauksbee's
//! generated bridge peripheral needs. The test below drives stock pico-sdk
//! firmware (`testdata/firmware/rp2040_i2c_probe`) that routes GP4/GP5 to
//! `GPIO_FUNC_I2C`, writes a register index to 0x48 and reads two bytes back,
//! printing them. The host bridge answers 0xAB, 0xCD and the firmware prints
//! them, so both directions are established, not just that the bridge attached.
//! This clears a higher bar than the nRF52840 bus test, which verifies
//! registration only for want of a bus firmware fixture.
//!
//! # SPI: structurally impossible with the vendored model, and why
//!
//! `SPI.PL022` in `db/mcu/rp2040/peripherals/rp2040_spi.cs` declares
//! `NullRegistrationPointPeripheralContainer<ISPIPeripheral>` as its base class
//! but never touches `RegisteredPeripheral`: grep the file, `Transmit` does not
//! appear in it. Its `Step()` bit-bangs the transfer onto GPIO pins
//! (`SetMultiplePins(txPins, ...)`, `SetMultiplePins(clockPins, ...)`) and
//! samples MISO with `ReadMultiplePins(rxPins)`, which is how it interworks with
//! the PIO block. A slave registered at the null registration point, which is
//! what hauksbee's SPI bridge is, is therefore never called: the firmware clocks
//! bits at real pins and reads back whatever the GPIO model has, which is 0.
//!
//! Observed exactly that way: `bytes=00 00`, bridge saw no MOSI byte, and the
//! bit-banged transfer is slow enough that one 2-byte transfer consumed the
//! whole run window. Hence `[soc.spi] controllers = []` in the descriptor, and
//! hence `spi_bridge_probe` below is `#[ignore]`d rather than deleted: it is the
//! reproduction, and un-ignoring it is the check for the day upstream dispatches
//! to the registered peripheral (or hauksbee grows a pin-level SPI bridge).

#![cfg(feature = "renode")]

use hauksbee_mcu::renode::is_available;
use hauksbee_mcu::traits::I2cEvent;
use hauksbee_mcu::{Mcu, RenodeBackend, RenodeConfig};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

const SLAVE_ADDR: u8 = 0x48;

fn firmware(dir: &str, name: &str) -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/firmware")
        .join(dir)
        .join(name);
    p.exists().then(|| p.canonicalize().unwrap_or(p))
}

#[test]
fn rp2040_i2c_bridge_round_trips_with_pico_sdk_firmware() {
    if !is_available() {
        eprintln!("SKIP: Renode not installed");
        return;
    }
    let Some(elf) = firmware("rp2040_i2c_probe", "i2c_probe.elf") else {
        eprintln!("SKIP: rp2040_i2c_probe/i2c_probe.elf not present");
        return;
    };

    // Straight from the descriptor: this also proves the bridge registers on
    // EVERY controller it names (a typo, or a name the platform does not carry,
    // fails registration), which is the nRF52840 test's lesson about a second
    // controller only showing up once two are configured.
    let config = RenodeConfig::rp2040();
    assert_eq!(
        config.i2c_controllers,
        vec!["i2c0".to_string(), "i2c1".to_string()],
        "descriptor must name the platform's two I2C controllers"
    );
    let mut mcu = RenodeBackend::new(config).expect("spawn Renode RP2040");
    assert!(mcu.i2c_bus_modeled(), "RP2040 models I2C (i2c0/i2c1)");

    let uart: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let uart_sink = uart.clone();
    mcu.on_uart(Box::new(move |b| uart_sink.lock().unwrap().push(b)));

    // What the bridge saw, so "the firmware printed zeroes" can be told apart
    // from "the bridge was never reached".
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let served = Arc::new(Mutex::new(0usize));
    let counter = served.clone();
    mcu.set_i2c_slave_addresses(&[SLAVE_ADDR]);
    mcu.on_i2c(Box::new(move |event| match event {
        I2cEvent::Read { addr } => {
            log.lock().unwrap().push(format!("read {addr:#04X}"));
            let mut n = counter.lock().unwrap();
            let byte = if *n % 2 == 0 { 0xAB } else { 0xCD };
            *n += 1;
            Some(byte)
        }
        I2cEvent::Write { addr, data } => {
            log.lock()
                .unwrap()
                .push(format!("write {addr:#04X} {data:#04X}"));
            None
        }
        I2cEvent::Start { addr, read } => {
            log.lock()
                .unwrap()
                .push(format!("start {addr:#04X} r={read}"));
            None
        }
        I2cEvent::Stop { addr } => {
            log.lock().unwrap().push(format!("stop {addr:#04X}"));
            None
        }
    }));

    mcu.load_firmware(&elf).expect("load pico-sdk I2C ELF");
    for _ in 0..20 {
        mcu.run_micros(50_000).expect("run chunk");
    }

    let text = String::from_utf8_lossy(&uart.lock().unwrap()).to_string();
    let events = seen.lock().unwrap().clone();

    assert!(
        text.contains("hauksbee rp2040 i2c: main reached"),
        "the I2C firmware must boot at all; got: {text:?}"
    );
    assert!(
        !events.is_empty(),
        "the bridge slave at {SLAVE_ADDR:#04X} saw no traffic, so RP2040I2C did \
         not route to it. UART said: {text:?}"
    );
    assert!(
        text.contains("bytes=AB CD"),
        "the firmware must read back the two bytes the host bridge served. \
         Bridge saw {events:?}; UART said: {text:?}"
    );
}

/// The recorded SPI failure. See the module docs for the root cause; this exists
/// so the claim is reproducible and so the fix has a check waiting for it. The
/// run window is short on purpose: the bit-banged transfer is slow, and this
/// test's job is to show the bridge sees nothing, not to wait for progress.
#[test]
#[ignore = "SPI.PL022 in the vendored RP2040 model bit-bangs onto GPIO pins and \
            never dispatches to its registered ISPIPeripheral, so hauksbee's SPI \
            bridge cannot be reached. See the module docs."]
fn rp2040_spi_bridge_probe() {
    if !is_available() {
        eprintln!("SKIP: Renode not installed");
        return;
    }
    let Some(elf) = firmware("rp2040_spi_probe", "spi_probe.elf") else {
        eprintln!("SKIP: rp2040_spi_probe/spi_probe.elf not present");
        return;
    };

    let mut config = RenodeConfig::rp2040();
    config.spi_controllers = vec!["spi0".to_string()];
    let mut mcu = RenodeBackend::new(config).expect("spawn Renode RP2040");

    let uart: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let uart_sink = uart.clone();
    mcu.on_uart(Box::new(move |b| uart_sink.lock().unwrap().push(b)));

    let mosi: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let seen = mosi.clone();
    let served = Arc::new(Mutex::new(0usize));
    let counter = served.clone();
    mcu.on_spi(Box::new(move |event| {
        if event.deselect {
            return 0;
        }
        seen.lock().unwrap().push(event.mosi);
        let mut n = counter.lock().unwrap();
        let byte = if *n % 2 == 0 { 0x5A } else { 0xA5 };
        *n += 1;
        byte
    }));

    mcu.load_firmware(&elf).expect("load pico-sdk SPI ELF");
    for _ in 0..4 {
        mcu.run_micros(50_000).expect("run chunk");
    }

    let text = String::from_utf8_lossy(&uart.lock().unwrap()).to_string();
    let observed = mosi.lock().unwrap().clone();
    assert!(
        observed.contains(&0x9F),
        "the bridge must see the firmware's MOSI byte 0x9F; saw {observed:?}. \
         UART said: {text:?}"
    );
    assert!(
        text.contains("bytes=5A A5"),
        "the firmware must read back what the host bridge clocked in. \
         Bridge saw {observed:?}; UART said: {text:?}"
    );
}
