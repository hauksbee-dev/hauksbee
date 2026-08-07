//! Regression fixtures for QEMU ADC injection and I2C/SPI byte callbacks
//! (05-cosim-fidelity §5.1/§5.2), against a REAL Espressif QEMU guest.
//!
//! Both features ride the RAM mailbox (`qemu::mailbox`), which is a firmware
//! contract, stated honestly as such: Espressif QEMU models neither the SAR
//! ADC nor a host hook for I2C/SPI byte traffic, so the backend exchanges
//! counts and bus transactions through fixed RTC-slow-memory words instead.
//!
//! No mailbox-v2-aware firmware image ships in the repo yet (this machine has
//! no Xtensa cross-toolchain to build one), so these tests play the FIRMWARE'S
//! half of the contract themselves through the backend's debug accessors,
//! which drive the same QMP/gdbstub transport the firmware words travel over.
//! What is genuinely end-to-end here: a real QEMU process boots the real
//! blinky flash image, guest RAM carries the contract words, the backend's
//! per-chunk service loop reads/writes them over the live control channels,
//! and the byte events surface through the same `on_i2c`/`on_spi` trait
//! callbacks the simavr/Renode backends use. What is NOT exercised: guest
//! instructions reading the slots (that needs the contract-aware firmware).
//!
//! Before the fix these fail structurally: `set_analog_in` was a no-op (the
//! count never appears in guest RAM) and `on_i2c`/`on_spi` discarded the
//! callback (no event ever fires, RSP_SEQ never advances).
//!
//! Skips gracefully (with the reason printed) when Espressif QEMU or the
//! blinky flash image is absent.

#![cfg(feature = "qemu")]

use hauksbee_mcu::qemu::{is_available, mailbox, QemuArch};
use hauksbee_mcu::{I2cEvent, Mcu, QemuBackend, SpiEvent};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

/// Serialize the QEMU-spawning tests (each boots its own instance; running
/// them back-to-back keeps wall-clock behaviour predictable).
fn qemu_test_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn flash_image() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/firmware/esp32_blinky/flash.bin");
    if p.exists() {
        Some(p.canonicalize().unwrap_or(p))
    } else {
        None
    }
}

macro_rules! qemu_or_skip {
    ($mcu:ident) => {
        if !is_available(QemuArch::Xtensa) {
            eprintln!("SKIP: Espressif QEMU (qemu-system-xtensa) not installed");
            return;
        }
        let Some(fw) = flash_image() else {
            eprintln!(
                "SKIP: flash.bin not built; run ./build.sh in testdata/firmware/esp32_blinky"
            );
            return;
        };
        let mut $mcu = QemuBackend::esp32(&fw).expect("boot Espressif QEMU esp32");
    };
}

/// §5.1: an injected analog voltage must land as a firmware-visible 12-bit
/// count in the channel's mailbox slot, with the channel's mask bit raised.
#[test]
fn qemu_adc_injection_lands_in_guest_ram() {
    let _guard = qemu_test_lock();
    qemu_or_skip!(mcu);

    // Scheduler contract: one set_analog_in per chunk, then run the chunk.
    let volts = 2.0;
    mcu.set_analog_in(0, volts);
    mcu.set_analog_in(3, 3.3);
    mcu.run_micros(5_000).expect("run chunk");

    let expected = ((volts / mailbox::ADC_FULL_SCALE_VOLTS) * f64::from(mailbox::ADC_MAX_COUNT))
        .round() as u32;
    let got = mcu
        .debug_read_u32(mailbox::adc_channel_word(0))
        .expect("read ADC slot 0");
    assert_eq!(
        got, expected,
        "channel 0: 2.0 V must appear as count {expected} in the mailbox slot; \
         got {got}. Zero here is the pre-§5.1 no-op set_analog_in."
    );
    let ch3 = mcu
        .debug_read_u32(mailbox::adc_channel_word(3))
        .expect("read ADC slot 3");
    // 3.3 V (the rail) is above the 3.1 V ATTEN_DB_11 full scale, so it
    // saturates at the top code exactly like the silicon converter does.
    assert_eq!(ch3, mailbox::ADC_MAX_COUNT, "over-range channel 3 clamps");
    let mask = mcu.debug_read_u32(mailbox::ADC_MASK).expect("read mask");
    assert_eq!(mask, 0b1001, "mask must carry exactly channels 0 and 3");

    // Injection is live: a new voltage overwrites the slot next chunk.
    mcu.set_analog_in(0, 0.4);
    mcu.run_micros(5_000).expect("run chunk");
    let updated = mcu
        .debug_read_u32(mailbox::adc_channel_word(0))
        .expect("re-read ADC slot 0");
    let expected2 =
        ((0.4 / mailbox::ADC_FULL_SCALE_VOLTS) * f64::from(mailbox::ADC_MAX_COUNT)).round() as u32;
    assert_eq!(updated, expected2, "count must track the injected voltage");
}

/// §5.2: an I2C transaction submitted through the mailbox cell must surface
/// as the same Start/Write/Read/Stop events the simavr/Renode backends
/// produce, with read replies landing back in guest RAM.
#[test]
fn qemu_i2c_mailbox_surfaces_byte_events() {
    let _guard = qemu_test_lock();
    qemu_or_skip!(mcu);

    let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let reply_counter = Arc::new(Mutex::new(0u8));
    let counter = reply_counter.clone();
    mcu.on_i2c(Box::new(move |ev| {
        let mut log = sink.lock().unwrap();
        match ev {
            I2cEvent::Start { addr, read } => {
                log.push(format!("start {addr:#04x} read={read}"));
                None
            }
            I2cEvent::Write { addr, data } => {
                log.push(format!("write {addr:#04x} {data:#04x}"));
                None
            }
            I2cEvent::Read { addr } => {
                log.push(format!("read {addr:#04x}"));
                let mut c = counter.lock().unwrap();
                *c += 1;
                Some(0xB0 + *c) // 0xB1, 0xB2, ...
            }
            I2cEvent::Stop { addr } => {
                log.push(format!("stop {addr:#04x}"));
                None
            }
        }
    }));

    // Play the firmware's half of the contract: raise the v2 magic, submit a
    // WRITE transaction (addr 0x50, three bytes), and run a chunk.
    mcu.debug_write_u32(mailbox::BUS_MAGIC, mailbox::BUS_MAGIC_VALUE)
        .expect("raise BUS_MAGIC");
    mcu.debug_write_bytes(mailbox::I2C_REQ_DATA, &[0x10, 0x20, 0x30])
        .expect("payload");
    mcu.debug_write_u32(mailbox::I2C_REQ_OP, mailbox::I2C_OP_WRITE)
        .expect("op");
    mcu.debug_write_u32(mailbox::I2C_REQ_ADDR, 0x50)
        .expect("addr");
    mcu.debug_write_u32(mailbox::I2C_REQ_LEN, 3).expect("len");
    mcu.debug_write_u32(mailbox::I2C_REQ_SEQ, 1).expect("seq");
    mcu.run_micros(5_000).expect("run chunk");

    assert_eq!(
        events.lock().unwrap().as_slice(),
        [
            "start 0x50 read=false",
            "write 0x50 0x10",
            "write 0x50 0x20",
            "write 0x50 0x30"
        ],
        "the write burst must surface as Start + one Write per byte \
         (no events at all is the pre-§5.2 discarded callback)"
    );
    assert_eq!(
        mcu.debug_read_u32(mailbox::I2C_RSP_SEQ).expect("rsp seq"),
        1,
        "the response sequence must acknowledge the serviced request"
    );

    // A READ transaction: two bytes, replies must land in the response cell.
    // A write→read turnaround on the SAME address is a repeated START, no
    // Stop in between (a register-read slave must not see its transaction
    // boundary mid-read), mirroring the Renode bridge's ensure_mode.
    mcu.debug_write_u32(mailbox::I2C_REQ_OP, mailbox::I2C_OP_READ)
        .expect("op");
    mcu.debug_write_u32(mailbox::I2C_REQ_LEN, 2).expect("len");
    mcu.debug_write_u32(mailbox::I2C_REQ_SEQ, 2).expect("seq");
    mcu.run_micros(5_000).expect("run chunk");

    {
        let log = events.lock().unwrap();
        assert_eq!(
            &log[4..],
            ["start 0x50 read=true", "read 0x50", "read 0x50"],
            "same-address write→read must be a repeated START (no Stop), \
             then one Read per byte"
        );
    }
    let rsp = mcu.debug_read_u32(mailbox::I2C_RSP_DATA).expect("rsp data");
    assert_eq!(
        rsp & 0xFFFF,
        0xB2B1,
        "the modeled reply bytes (0xB1, 0xB2) must land in the response cell \
         little-endian; got {rsp:#010x}"
    );
    assert_eq!(mcu.debug_read_u32(mailbox::I2C_RSP_SEQ).unwrap(), 2);

    // STOP closes the open transaction.
    mcu.debug_write_u32(mailbox::I2C_REQ_OP, mailbox::I2C_OP_STOP)
        .expect("op");
    mcu.debug_write_u32(mailbox::I2C_REQ_LEN, 0).expect("len");
    mcu.debug_write_u32(mailbox::I2C_REQ_SEQ, 3).expect("seq");
    mcu.run_micros(5_000).expect("run chunk");
    assert_eq!(
        events.lock().unwrap().last().map(String::as_str),
        Some("stop 0x50")
    );
    assert_eq!(mcu.debug_read_u32(mailbox::I2C_RSP_SEQ).unwrap(), 3);
}

/// §5.2: an SPI burst submitted through the mailbox cell must fire one
/// [`SpiEvent`] per byte and return the MISO bytes; a deselect op surfaces the
/// same `deselect` event Renode's FinishTransmission produces.
#[test]
fn qemu_spi_mailbox_surfaces_byte_events() {
    let _guard = qemu_test_lock();
    qemu_or_skip!(mcu);

    let events: Arc<Mutex<Vec<(u8, bool)>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    mcu.on_spi(Box::new(move |ev: SpiEvent| {
        sink.lock().unwrap().push((ev.mosi, ev.deselect));
        !ev.mosi // reply: bitwise-NOT of MOSI, easy to assert
    }));

    mcu.debug_write_u32(mailbox::BUS_MAGIC, mailbox::BUS_MAGIC_VALUE)
        .expect("raise BUS_MAGIC");
    mcu.debug_write_bytes(mailbox::SPI_REQ_DATA, &[0x01, 0x80, 0xAA])
        .expect("mosi burst");
    mcu.debug_write_u32(mailbox::SPI_REQ_OP, mailbox::SPI_OP_TRANSFER)
        .expect("op");
    mcu.debug_write_u32(mailbox::SPI_REQ_LEN, 3).expect("len");
    mcu.debug_write_u32(mailbox::SPI_REQ_SEQ, 1).expect("seq");
    mcu.run_micros(5_000).expect("run chunk");

    assert_eq!(
        events.lock().unwrap().as_slice(),
        [(0x01, false), (0x80, false), (0xAA, false)],
        "the burst must surface one on_spi byte event per MOSI byte \
         (no events is the pre-§5.2 no-op on_spi)"
    );
    let rsp = mcu.debug_read_u32(mailbox::SPI_RSP_DATA).expect("rsp data");
    assert_eq!(
        rsp & 0x00FF_FFFF,
        u32::from_le_bytes([!0x01u8, !0x80u8, !0xAAu8, 0]),
        "MISO bytes must land in the response cell; got {rsp:#010x}"
    );
    assert_eq!(mcu.debug_read_u32(mailbox::SPI_RSP_SEQ).unwrap(), 1);

    // Deselect: fires the deselect event (mosi meaningless), no reply bytes.
    mcu.debug_write_u32(mailbox::SPI_REQ_OP, mailbox::SPI_OP_DESELECT)
        .expect("op");
    mcu.debug_write_u32(mailbox::SPI_REQ_LEN, 0).expect("len");
    mcu.debug_write_u32(mailbox::SPI_REQ_SEQ, 2).expect("seq");
    mcu.run_micros(5_000).expect("run chunk");
    assert_eq!(events.lock().unwrap().last(), Some(&(0x00, true)));
    assert_eq!(mcu.debug_read_u32(mailbox::SPI_RSP_SEQ).unwrap(), 2);
}

/// Bit-identical when off: with callbacks registered but
/// the firmware never raising BUS_MAGIC (every firmware that exists today),
/// the backend must not touch the bus cells, no events, no acknowledgement.
#[test]
fn qemu_bus_mailbox_is_inert_without_magic() {
    let _guard = qemu_test_lock();
    qemu_or_skip!(mcu);

    let fired: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
    let (fi, fs) = (fired.clone(), fired.clone());
    mcu.on_i2c(Box::new(move |_| {
        *fi.lock().unwrap() += 1;
        None
    }));
    mcu.on_spi(Box::new(move |_| {
        *fs.lock().unwrap() += 1;
        0
    }));

    // A fully-formed request in the cell, but no BUS_MAGIC: must be ignored.
    mcu.debug_write_u32(mailbox::I2C_REQ_OP, mailbox::I2C_OP_WRITE)
        .expect("op");
    mcu.debug_write_u32(mailbox::I2C_REQ_ADDR, 0x50)
        .expect("addr");
    mcu.debug_write_u32(mailbox::I2C_REQ_LEN, 1).expect("len");
    mcu.debug_write_u32(mailbox::I2C_REQ_SEQ, 1).expect("seq");
    mcu.debug_write_u32(mailbox::SPI_REQ_OP, mailbox::SPI_OP_TRANSFER)
        .expect("op");
    mcu.debug_write_u32(mailbox::SPI_REQ_LEN, 1).expect("len");
    mcu.debug_write_u32(mailbox::SPI_REQ_SEQ, 1).expect("seq");
    for _ in 0..3 {
        mcu.run_micros(5_000).expect("run chunk");
    }

    assert_eq!(
        *fired.lock().unwrap(),
        0,
        "no callback may fire while the firmware has not raised BUS_MAGIC"
    );
    assert_eq!(
        mcu.debug_read_u32(mailbox::I2C_RSP_SEQ).unwrap(),
        0,
        "the I2C response cell must stay untouched"
    );
    assert_eq!(
        mcu.debug_read_u32(mailbox::SPI_RSP_SEQ).unwrap(),
        0,
        "the SPI response cell must stay untouched"
    );
}
