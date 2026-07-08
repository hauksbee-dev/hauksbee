//! simavr-backed AVR emulation.
//!
//! [`AvrMcu`] wraps a `simavr` `avr_t` instance and exposes the generic
//! [`Mcu`] trait.  All Tarski-specific logic lives in higher layers; this
//! module only deals in plain IRQ hooks and byte streams. It is the only
//! cycle-exact backend: simavr's C hooks fire synchronously inside `avr_run`,
//! so every reported edge carries its true `avr->cycle` stamp.
//!
//! Long-form how-and-why: docs/how-and-why/hauksbee-mcu/avr.md.

use crate::ffi;
use crate::traits::{I2cEvent, Mcu, McuState, PinId, SpiEvent};
use anyhow::{bail, Result};
use std::ffi::CString;
use std::path::Path;
use std::ptr;
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// simavr IOCTL helpers (re-implemented from C macros)
// ---------------------------------------------------------------------------

const fn avr_ioctl_def(a: u8, b: u8, c: u8, d: u8) -> u32 {
    ((a as u32) << 24) | ((b as u32) << 16) | ((c as u32) << 8) | (d as u32)
}

const fn uart_getirq(name: u8) -> u32 {
    avr_ioctl_def(b'u', b'a', b'r', name)
}

const fn ioport_getirq(name: u8) -> u32 {
    avr_ioctl_def(b'i', b'o', b'g', name)
}

/// `AVR_IOCTL_IOPORT_SET_EXTERNAL(name)`: install a persistent external drive
/// on a port's input pins (simavr's `p->external.pull_mask/pull_value`). See
/// [`drive_ioport_input`] for why a bare per-bit IRQ raise is not enough.
const fn ioport_set_external(name: u8) -> u32 {
    avr_ioctl_def(b'i', b'o', b'p', name)
}

const ADC_GETIRQ: u32 = avr_ioctl_def(b'a', b'd', b'c', b'0');
const TWI_GETIRQ: u32 = avr_ioctl_def(b't', b'w', b'i', 0);
const SPI_GETIRQ: u32 = avr_ioctl_def(b's', b'p', b'i', 0);

const UART_IRQ_INPUT: i32 = 0;
const UART_IRQ_OUTPUT: i32 = 1;
const TWI_IRQ_INPUT: i32 = 0;
const TWI_IRQ_OUTPUT: i32 = 1;
const SPI_IRQ_INPUT: i32 = 0; // MISO (from peripheral into MCU)
const SPI_IRQ_OUTPUT: i32 = 1; // MOSI (from MCU to peripheral)

/// Index into the ioport IRQ array for the whole PORT register. This is NOT a
/// fixed constant: simavr's `IOPORT_IRQ_*` enum reorders between versions (the
/// addition of `IOPORT_IRQ_PIN_ALL_IN` shifts this from 10 to 11), so it is
/// sourced from whatever simavr the build linked (via bindgen) rather than
/// hardcoded — a hardcoded 11 subscribed to the wrong IRQ on an older simavr,
/// making GPIO output read as "never driven" (GREEN on one host, RED on another).
const IOPORT_IRQ_REG_PORT: i32 = ffi::IOPORT_IRQ_REG_PORT as i32;

/// Index for the DDR (data-direction) register; bindgen-sourced for the same
/// version-skew reason as `IOPORT_IRQ_REG_PORT`. Subscribed so a
/// `pinMode(OUTPUT)` (a DDR write) is recorded as "ever configured output" —
/// observation only, see `make_ddr_hook`.
const IOPORT_IRQ_DIRECTION_ALL: i32 = ffi::IOPORT_IRQ_DIRECTION_ALL as i32;

/// Drive an external digital level onto one input pin, PERSISTENTLY.
///
/// Two simavr mechanisms compose here, and both are required:
///
/// 1. The per-bit ioport IRQ raise updates the PIN register immediately, so a
///    drive issued from inside the port-output hook lands before the
///    firmware's next instruction (the 74HC165 readback path, 05 §1.5).
/// 2. The `SET_EXTERNAL` ioctl records the level in the port's
///    `external.pull_mask/pull_value`. Without it the drive is a one-shot:
///    simavr's `avr_ioport_update_irqs` runs after EVERY firmware PORT or DDR
///    write to the port and re-derives every input pin's level from pull
///    state — so a pin whose PORT bit the firmware left at 1 (the classic
///    open-drain "release = input with pull-up" idiom of soft-I2C masters)
///    snaps back to the internal pull-up's 1 on the very next SCL toggle,
///    stomping the responder's ACK/data bit before the firmware reads it.
///    The external-pull entry is simavr's own model of "an external device
///    holds this line", which is exactly what a responder-driven pin is; it
///    takes precedence over the internal pull-up in `update_irqs`, the same
///    way a real (stronger) external driver beats the ~35k internal pull-up.
///
/// `ext_drive` is the engine-maintained per-port (mask, value) shadow of the
/// external state — the ioctl REPLACES the whole port's pull bytes, so the
/// merged state must be resent, not just the changed bit. Later drives to the
/// same pin (responder updates, chunk-boundary `set_digital_in` syncs) simply
/// overwrite: last external writer owns the line.
///
/// `avr` must be the live core pointer (taken from `SharedState`).
unsafe fn drive_ioport_input(
    avr: *mut ffi::avr_t,
    ext_drive: &mut std::collections::HashMap<char, (u8, u8)>,
    pin: PinId,
    high: bool,
) {
    if avr.is_null() {
        return;
    }
    let (mask, value) = ext_drive.entry(pin.port).or_insert((0, 0));
    *mask |= 1 << pin.bit;
    if high {
        *value |= 1 << pin.bit;
    } else {
        *value &= !(1 << pin.bit);
    }
    // avr_ioport_external_t is a bitfield struct over one unsigned long:
    // name:7 | mask:8 | value:8, LSB-first on every LP64 target we build for.
    let mut ext: u64 = ((pin.port as u8 as u64) & 0x7f)
        | ((*mask as u64) << 7)
        | ((*value as u64) << 15);
    unsafe {
        ffi::avr_ioctl(
            avr,
            ioport_set_external(pin.port as u8),
            &mut ext as *mut u64 as *mut std::os::raw::c_void,
        );
        let irq = ffi::avr_io_getirq(avr, ioport_getirq(pin.port as u8), pin.bit as i32);
        if !irq.is_null() {
            ffi::avr_raise_irq(irq, high as u32);
        }
    }
}

// TWI message condition flags (from avr_twi.h)
const TWI_COND_START: u32 = 1 << 0;
const TWI_COND_STOP: u32 = 1 << 1;
const TWI_COND_ADDR: u32 = 1 << 2;
const TWI_COND_ACK: u32 = 1 << 3;
const TWI_COND_WRITE: u32 = 1 << 4;
const TWI_COND_READ: u32 = 1 << 5;

// ---------------------------------------------------------------------------
// Callback state (written inside C callbacks, read from Rust)
// ---------------------------------------------------------------------------

type PinChangeCb = Box<dyn FnMut(PinId, bool, u64) + Send>;
type UartCb = Box<dyn FnMut(u8) + Send>;
type I2cCb = Box<dyn FnMut(I2cEvent) -> Option<u8> + Send>;
type SpiCb = Box<dyn FnMut(SpiEvent) -> u8 + Send>;
/// A synchronous GPIO-input responder. Given the output pin edge the firmware
/// just produced, it returns a list of input pins to drive (and their levels)
/// *immediately*, within the same `run_micros` call, before the firmware's next
/// instruction. This is what lets a firmware bit-bang a clock and `digitalRead`
/// the resulting serial-out bit in the SAME tight loop (the 74HC165 readback):
/// the output-pin hook fires on the SCLK/PL edge, the responder computes the
/// next QH bit, and the bit is raised onto the MISO ioport input IRQ here — so
/// the very next `digitalRead(MISO)` sees it. Resolving the readback per output
/// edge (not once per analog chunk) is the read-direction analogue of the
/// edge-driven 74HC595 write path.
type InputResponderCb = Box<dyn FnMut(PinId, bool) -> Vec<(PinId, bool)> + Send>;

struct Callbacks {
    on_pin_change: Option<PinChangeCb>,
    on_uart: Option<UartCb>,
    on_i2c: Option<I2cCb>,
    on_spi: Option<SpiCb>,
    /// Synchronous input responder, driven from the same port hook as
    /// `on_pin_change` (see [`InputResponderCb`]).
    input_responder: Option<InputResponderCb>,
}

/// Per-port state tracked for edge detection.
struct PortState {
    /// Current port byte value.
    current: u8,
    /// Current DDR mask (1 = output). Tracks the *latest* direction, not an
    /// accumulation, so a pin set OUTPUT then back to INPUT (open-drain release /
    /// bus hand-off) reads as input again rather than stuck "output". METADATA
    /// ONLY — it never enables a circuit driver or fires a pin-change; it just
    /// lets a higher layer tell an output-low-held pin (driven LOW) from one the
    /// firmware never configured (floating). Keeping it out of the drive path is
    /// deliberate: an earlier version that drove the circuit from DDR edges
    /// latched open-drain pins low and clamped SPI nets, so this is read-only.
    output_dir: u8,
}

/// State shared between the Rust owner and C IRQ callbacks.
struct SharedState {
    /// Raw AVR pointer, needed to call avr_raise_irq from inside callbacks.
    avr_ptr: *mut ffi::avr_t,

    /// Port register values indexed by port letter.
    /// We only track ports that have registered hooks.
    port_state: std::collections::HashMap<char, PortState>,

    /// Per-port (mask, value) shadow of simavr's external input-drive state
    /// (`external.pull_mask/pull_value`), maintained by [`drive_ioport_input`]
    /// so each SET_EXTERNAL ioctl can resend the port's full merged state.
    ext_drive: std::collections::HashMap<char, (u8, u8)>,

    /// Active I2C transaction accumulator.
    twi_addr: u8,
    twi_active: bool,
    /// True once the current transaction's address byte carried the R/W=read
    /// bit, so subsequent master-read clocks pull bytes from the slave.
    twi_read: bool,

    /// User-installed callbacks.
    callbacks: Callbacks,
}

// SAFETY: avr_ptr is only used while the AvrMcu is alive, from the same thread
// that runs simavr.  We never alias it across threads.
unsafe impl Send for SharedState {}

// ---------------------------------------------------------------------------
// C IRQ callback implementations
// ---------------------------------------------------------------------------

unsafe extern "C" fn uart_output_hook(
    _irq: *mut ffi::avr_irq_t,
    value: u32,
    param: *mut std::os::raw::c_void,
) {
    let state = unsafe { &*(param as *const Arc<Mutex<SharedState>>) };
    if let Ok(mut s) = state.lock() {
        if let Some(cb) = &mut s.callbacks.on_uart {
            cb(value as u8);
        }
    }
}

/// Per-port hook: detects bit-level edges and calls the pin-change callback.
///
/// We can't have closures as C function pointers, so we use a macro to
/// generate one hook per port letter.
macro_rules! make_port_hook {
    ($fn_name:ident, $port_char:literal) => {
        unsafe extern "C" fn $fn_name(
            _irq: *mut ffi::avr_irq_t,
            value: u32,
            param: *mut std::os::raw::c_void,
        ) {
            let state = unsafe { &*(param as *const Arc<Mutex<SharedState>>) };
            if let Ok(mut s) = state.lock() {
                let new_val = value as u8;
                let prev_val = s
                    .port_state
                    .get(&$port_char)
                    .map(|ps| ps.current)
                    .unwrap_or(0);

                if new_val != prev_val {
                    let changed = new_val ^ prev_val;
                    // Snapshot the avr pointer so the synchronous input
                    // responder can raise an ioport input IRQ while we still
                    // hold the `s` borrow (avr_ptr is Copy).
                    let avr = s.avr_ptr;
                    // The IRQ fires synchronously inside `avr_run`, so `avr->cycle`
                    // here is the EXACT cycle of this edge. Stamping it lets the
                    // scheduler replay a sub-µs SCLK burst in true order rather
                    // than collapsing it to a level (numerical lore #8).
                    let cycle = if avr.is_null() {
                        0
                    } else {
                        unsafe { (*avr).cycle }
                    };
                    // Fire callback for each bit that changed.
                    for bit in 0u8..8 {
                        if (changed >> bit) & 1 == 0 {
                            continue;
                        }
                        let high = (new_val >> bit) & 1 != 0;
                        let pin = PinId { port: $port_char, bit };
                        if let Some(cb) = &mut s.callbacks.on_pin_change {
                            cb(pin, high, cycle);
                        }
                        // Synchronous input drive: the responder may push a
                        // serial-out bit back onto an MCU input pin (e.g. the
                        // 74HC165 QH -> MISO) so the firmware reads it on its
                        // next instruction, within this same run. Split-borrow
                        // the state so the drive can update the external-pull
                        // shadow while the responder closure stays borrowed.
                        let st = &mut *s;
                        if let Some(resp) = &mut st.callbacks.input_responder {
                            for (in_pin, in_high) in resp(pin, high) {
                                unsafe {
                                    drive_ioport_input(
                                        avr,
                                        &mut st.ext_drive,
                                        in_pin,
                                        in_high,
                                    );
                                }
                            }
                        }
                    }
                    s.port_state
                        .entry($port_char)
                        .or_insert(PortState {
                            current: 0,
                            output_dir: 0,
                        })
                        .current = new_val;
                }
            }
        }
    };
}

/// Per-port DDR (direction) hook: records which bits have ever been configured
/// as outputs. Observation only — it does NOT fire `on_pin_change` and does NOT
/// touch the circuit, so it cannot latch open-drain pins or clamp bus nets. The
/// boot-state panel uses it to distinguish a `pinMode(OUTPUT)` pin held LOW from
/// a pin the firmware never configured (genuinely floating).
macro_rules! make_ddr_hook {
    ($fn_name:ident, $port_char:literal) => {
        unsafe extern "C" fn $fn_name(
            _irq: *mut ffi::avr_irq_t,
            value: u32,
            param: *mut std::os::raw::c_void,
        ) {
            let state = unsafe { &*(param as *const Arc<Mutex<SharedState>>) };
            if let Ok(mut s) = state.lock() {
                let new_ddr = value as u8;
                // Latest direction wins (not accumulated): a pin released back to
                // input clears its bit, so an open-drain / handed-off bus pin
                // does not read as a permanent output.
                s.port_state
                    .entry($port_char)
                    .or_insert(PortState {
                        current: 0,
                        output_dir: 0,
                    })
                    .output_dir = new_ddr;
            }
        }
    };
}

make_port_hook!(port_hook_a, 'A');
make_port_hook!(port_hook_b, 'B');
make_port_hook!(port_hook_c, 'C');
make_port_hook!(port_hook_d, 'D');
make_port_hook!(port_hook_e, 'E');
make_port_hook!(port_hook_f, 'F');
make_port_hook!(port_hook_g, 'G');
make_port_hook!(port_hook_h, 'H');

make_ddr_hook!(ddr_hook_a, 'A');
make_ddr_hook!(ddr_hook_b, 'B');
make_ddr_hook!(ddr_hook_c, 'C');
make_ddr_hook!(ddr_hook_d, 'D');
make_ddr_hook!(ddr_hook_e, 'E');
make_ddr_hook!(ddr_hook_f, 'F');
make_ddr_hook!(ddr_hook_g, 'G');
make_ddr_hook!(ddr_hook_h, 'H');

/// Map a port letter to its pre-generated hook function pointer.
fn port_hook_fn(
    port: char,
) -> Option<unsafe extern "C" fn(*mut ffi::avr_irq_t, u32, *mut std::os::raw::c_void)> {
    match port {
        'A' => Some(port_hook_a),
        'B' => Some(port_hook_b),
        'C' => Some(port_hook_c),
        'D' => Some(port_hook_d),
        'E' => Some(port_hook_e),
        'F' => Some(port_hook_f),
        'G' => Some(port_hook_g),
        'H' => Some(port_hook_h),
        _ => None,
    }
}

/// Map a port letter to its pre-generated DDR (direction) hook function pointer.
fn ddr_hook_fn(
    port: char,
) -> Option<unsafe extern "C" fn(*mut ffi::avr_irq_t, u32, *mut std::os::raw::c_void)> {
    match port {
        'A' => Some(ddr_hook_a),
        'B' => Some(ddr_hook_b),
        'C' => Some(ddr_hook_c),
        'D' => Some(ddr_hook_d),
        'E' => Some(ddr_hook_e),
        'F' => Some(ddr_hook_f),
        'G' => Some(ddr_hook_g),
        'H' => Some(ddr_hook_h),
        _ => None,
    }
}

/// TWI (I2C) hook: intercepts Wire/TWI transactions from the firmware.
///
/// On each event:
///  - START+ADDR: record address, send ACK, fire I2cEvent::Start.
///  - WRITE byte: accumulate, send ACK, fire I2cEvent::Write.
///  - STOP: close transaction, fire I2cEvent::Stop.
unsafe extern "C" fn twi_hook(
    _irq: *mut ffi::avr_irq_t,
    value: u32,
    param: *mut std::os::raw::c_void,
) {
    let state = unsafe { &*(param as *const Arc<Mutex<SharedState>>) };
    if let Ok(mut s) = state.lock() {
        // avr_twi_msg_t bitfield (from avr_twi.h):
        //   bits [7:0]   = unused
        //   bits [15:8]  = msg  (condition flags)
        //   bits [23:16] = addr (address byte, including R/W bit)
        //   bits [31:24] = data
        let msg_flags = (value >> 8) & 0xFF;
        let addr_byte = ((value >> 16) & 0xFF) as u8;
        let data_byte = ((value >> 24) & 0xFF) as u8;

        let avr = s.avr_ptr;

        if msg_flags & (TWI_COND_START | TWI_COND_ADDR) != 0 {
            // New transaction.
            let addr7 = addr_byte >> 1;
            let read_flag = (addr_byte & 1) != 0;
            s.twi_addr = addr7;
            s.twi_active = true;
            s.twi_read = read_flag;

            // Fire user callback and use its return value (if any); for START
            // events the return value is meaningless, but we keep the API uniform.
            if let Some(cb) = &mut s.callbacks.on_i2c {
                let _ = cb(I2cEvent::Start {
                    addr: addr7,
                    read: read_flag,
                });
            }

            // Send ACK so the firmware's Wire library doesn't stall.
            if !avr.is_null() {
                let twi_in = ffi::avr_io_getirq(avr, TWI_GETIRQ, TWI_IRQ_INPUT);
                if !twi_in.is_null() {
                    let ack = ffi::avr_twi_irq_msg(TWI_COND_ACK as u8, addr7, 1);
                    ffi::avr_raise_irq(twi_in, ack);
                }
            }
        } else if msg_flags & TWI_COND_READ != 0 && s.twi_active {
            // Master-read clock: the firmware wants a byte from the slave. Ask
            // the handler for the reply and inject it back into the TWI receiver
            // with READ|ACK so the firmware's Wire library completes the read.
            let addr7 = s.twi_addr;
            let reply_byte = if let Some(cb) = &mut s.callbacks.on_i2c {
                cb(I2cEvent::Read { addr: addr7 }).unwrap_or(0xFF)
            } else {
                0xFF
            };
            if !avr.is_null() {
                let twi_in = ffi::avr_io_getirq(avr, TWI_GETIRQ, TWI_IRQ_INPUT);
                if !twi_in.is_null() {
                    let reply = ffi::avr_twi_irq_msg(
                        (TWI_COND_READ | TWI_COND_ACK) as u8,
                        addr7,
                        reply_byte,
                    );
                    ffi::avr_raise_irq(twi_in, reply);
                }
            }
        } else if msg_flags & TWI_COND_WRITE != 0 && s.twi_active {
            // Data byte written by firmware.
            let addr7 = s.twi_addr;
            let _reply_byte = if let Some(cb) = &mut s.callbacks.on_i2c {
                cb(I2cEvent::Write {
                    addr: addr7,
                    data: data_byte,
                })
                .unwrap_or(0)
            } else {
                0
            };

            // ACK the byte.
            if !avr.is_null() {
                let twi_in = ffi::avr_io_getirq(avr, TWI_GETIRQ, TWI_IRQ_INPUT);
                if !twi_in.is_null() {
                    let ack = ffi::avr_twi_irq_msg(TWI_COND_ACK as u8, addr7, 1);
                    ffi::avr_raise_irq(twi_in, ack);
                }
            }
        } else if msg_flags & TWI_COND_STOP != 0 && s.twi_active {
            s.twi_active = false;
            s.twi_read = false;
            let addr7 = s.twi_addr;
            if let Some(cb) = &mut s.callbacks.on_i2c {
                let _ = cb(I2cEvent::Stop { addr: addr7 });
            }
        }
    }
}

/// SPI hook: fires on each byte clocked out of the MCU's SPI peripheral.
unsafe extern "C" fn spi_output_hook(
    _irq: *mut ffi::avr_irq_t,
    value: u32,
    param: *mut std::os::raw::c_void,
) {
    let state = unsafe { &*(param as *const Arc<Mutex<SharedState>>) };
    if let Ok(mut s) = state.lock() {
        let mosi = value as u8;
        let avr = s.avr_ptr;

        // The SPI IRQ fires synchronously inside `avr_run`, so `avr->cycle` here
        // is the EXACT cycle of this byte transfer, the same clock the pin-edge
        // hook stamps. Carrying it lets the scheduler interleave the byte stream
        // with the CS-pin edge stream in true order for real CS framing (05 §2).
        let cycle = if avr.is_null() {
            0
        } else {
            unsafe { (*avr).cycle }
        };

        let miso = if let Some(cb) = &mut s.callbacks.on_spi {
            cb(SpiEvent {
                mosi,
                deselect: false,
                cycle,
            })
        } else {
            0xFF
        };

        // Inject MISO byte back into the SPI peripheral.
        if !avr.is_null() {
            let spi_in = ffi::avr_io_getirq(avr, SPI_GETIRQ, SPI_IRQ_INPUT);
            if !spi_in.is_null() {
                ffi::avr_raise_irq(spi_in, miso as u32);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// AvrMcu
// ---------------------------------------------------------------------------

/// simavr-backed implementation of [`Mcu`].
///
/// Create with [`AvrMcu::new`]:
/// ```rust,no_run
/// use hauksbee_mcu::{AvrMcu, Mcu};
/// let mut mcu = AvrMcu::new("atmega328p", 16_000_000).unwrap();
/// mcu.load_firmware(std::path::Path::new("firmware.hex")).unwrap();
/// ```
pub struct AvrMcu {
    /// Raw simavr instance.  Non-null after construction.
    avr: *mut ffi::avr_t,

    /// Shared callback state between Rust owner and C hooks.
    state: Arc<Mutex<SharedState>>,

    /// Raw pointer to the leaked `Box<Arc<Mutex<SharedState>>>` handed to C.
    /// Reclaimed in `Drop`.
    callback_ptr: *mut std::os::raw::c_void,

    /// Clock frequency in Hz.
    frequency_hz: u64,

    /// Ports whose IRQ hooks are currently registered.
    hooked_ports: Vec<char>,

    /// True once firmware has been loaded.
    firmware_loaded: bool,
}

// SAFETY: We never share AvrMcu across threads simultaneously; it is Send
// because SharedState is Send and avr_ptr is only touched from the owning thread.
unsafe impl Send for AvrMcu {}

impl AvrMcu {
    /// Create a new, uninitialised AVR MCU.
    ///
    /// `mcu_name` is a simavr MCU identifier such as `"atmega328p"`,
    /// `"atmega2560"`, etc.  `frequency` is the clock in Hz.
    ///
    /// Call [`Mcu::load_firmware`] before executing.
    pub fn new(mcu_name: &str, frequency: u64) -> Result<Self> {
        let mcu_cstr = CString::new(mcu_name)?;

        let avr = unsafe { ffi::avr_make_mcu_by_name(mcu_cstr.as_ptr()) };
        if avr.is_null() {
            bail!("simavr does not know MCU type '{}'", mcu_name);
        }

        unsafe {
            ffi::avr_init(avr);
            (*avr).frequency = frequency as u32;
            // Route simavr's own logging through hauksbee's debug channel. By
            // default simavr writes AVR_LOG lines straight to fd 2 — including
            // `avr_sadly_crashed`'s crash dump, which the persona panel saw leak
            // into user-facing CI output when a boot assert ran without firmware.
            // AVR_LOG is gated on `avr->log`, so setting it to LOG_NONE (0)
            // silences the emulator's internal chatter unless the same
            // `HAUKSBEE_DEBUG` switch that opens the solver's debug channel is set
            // (any non-empty value), in which case simavr's default verbosity is
            // left untouched for whoever is debugging the co-sim.
            let debug_on = std::env::var_os("HAUKSBEE_DEBUG")
                .map(|v| !v.is_empty())
                .unwrap_or(false);
            if !debug_on {
                (*avr).set_log(0);
            }
            // simavr defaults the rails to 3.3V; standard Arduino-class
            // parts run at 5V and the ADC full scale follows AVcc. The
            // co-sim layer can override via set_rails().
            (*avr).vcc = 5000;
            (*avr).avcc = 5000;
            (*avr).aref = 5000;
        }

        let state = Arc::new(Mutex::new(SharedState {
            avr_ptr: avr,
            port_state: std::collections::HashMap::new(),
            ext_drive: std::collections::HashMap::new(),
            twi_addr: 0,
            twi_active: false,
            twi_read: false,
            callbacks: Callbacks {
                on_pin_change: None,
                on_uart: None,
                on_i2c: None,
                on_spi: None,
                input_responder: None,
            },
        }));

        let leaked = Box::into_raw(Box::new(state.clone()));
        let callback_ptr = leaked as *mut std::os::raw::c_void;

        // Register UART output hook immediately (it's always wanted).
        unsafe {
            let uart_irq = ffi::avr_io_getirq(avr, uart_getirq(b'0'), UART_IRQ_OUTPUT);
            if !uart_irq.is_null() {
                ffi::avr_irq_register_notify(uart_irq, Some(uart_output_hook), callback_ptr);
            }
        }

        Ok(Self {
            avr,
            state,
            callback_ptr,
            frequency_hz: frequency,
            hooked_ports: Vec::new(),
            firmware_loaded: false,
        })
    }

    /// Convenience constructor for an ATmega328P at 16 MHz (Arduino Nano/Uno).
    pub fn atmega328p_16mhz() -> Result<Self> {
        Self::new("atmega328p", 16_000_000)
    }

    /// Set the supply/reference rails in volts (defaults are 5V). The ADC
    /// full scale follows AVcc/ARef, so a 3.3V board must set these.
    pub fn set_rails(&mut self, vcc: f64, avcc: f64, aref: f64) {
        unsafe {
            (*self.avr).vcc = (vcc * 1000.0).round() as u32;
            (*self.avr).avcc = (avcc * 1000.0).round() as u32;
            (*self.avr).aref = (aref * 1000.0).round() as u32;
        }
    }

    /// Register GPIO port hooks for the listed ports.
    ///
    /// Automatically called when [`on_pin_change`] is first set, but you can
    /// call this manually to pre-register ports before setting the callback.
    pub fn register_port_hooks(&mut self, ports: &[char]) {
        for &port in ports {
            if self.hooked_ports.contains(&port) {
                continue;
            }
            if let Some(hook_fn) = port_hook_fn(port) {
                unsafe {
                    let irq = ffi::avr_io_getirq(
                        self.avr,
                        ioport_getirq(port as u8),
                        IOPORT_IRQ_REG_PORT,
                    );
                    if !irq.is_null() {
                        ffi::avr_irq_register_notify(irq, Some(hook_fn), self.callback_ptr);
                        self.hooked_ports.push(port);
                    }
                }
            }
            // Observation-only DDR hook: records "ever configured as output" so a
            // pin held output-LOW is distinguishable from a never-configured
            // (floating) one. It never drives the circuit (see make_ddr_hook).
            if let Some(ddr_fn) = ddr_hook_fn(port) {
                unsafe {
                    let irq = ffi::avr_io_getirq(
                        self.avr,
                        ioport_getirq(port as u8),
                        IOPORT_IRQ_DIRECTION_ALL,
                    );
                    if !irq.is_null() {
                        ffi::avr_irq_register_notify(irq, Some(ddr_fn), self.callback_ptr);
                    }
                }
            }
        }
    }

    /// Register the TWI (I2C) hook.
    ///
    /// Called automatically when [`on_i2c`] is first set.
    pub fn register_twi_hook(&mut self) {
        unsafe {
            let twi_out = ffi::avr_io_getirq(self.avr, TWI_GETIRQ, TWI_IRQ_OUTPUT);
            if !twi_out.is_null() {
                ffi::avr_irq_register_notify(twi_out, Some(twi_hook), self.callback_ptr);
            }
        }
    }

    /// Register the SPI hook.
    ///
    /// Called automatically when [`on_spi`] is first set.
    pub fn register_spi_hook(&mut self) {
        unsafe {
            let spi_out = ffi::avr_io_getirq(self.avr, SPI_GETIRQ, SPI_IRQ_OUTPUT);
            if !spi_out.is_null() {
                ffi::avr_irq_register_notify(spi_out, Some(spi_output_hook), self.callback_ptr);
            }
        }
    }

    /// Load a `.hex` file by parsing it and flashing the MCU.
    fn load_hex(&mut self, path: &Path) -> Result<()> {
        let hex_cstr = CString::new(
            path.to_str()
                .ok_or_else(|| anyhow::anyhow!("non-UTF-8 firmware path"))?,
        )?;

        unsafe {
            let mut boot_base: u32 = 0;
            let mut boot_size: u32 = 0;
            let boot = ffi::read_ihex_file(hex_cstr.as_ptr(), &mut boot_size, &mut boot_base);
            if boot.is_null() {
                bail!("read_ihex_file failed for '{}'", path.display());
            }
            ptr::copy_nonoverlapping(
                boot,
                (*self.avr).flash.add(boot_base as usize),
                boot_size as usize,
            );
            libc::free(boot as *mut libc::c_void);
            (*self.avr).pc = boot_base;
            (*self.avr).codeend = (*self.avr).flashend;
        }

        Ok(())
    }

    /// Load an `.elf` file using simavr's ELF loader.
    fn load_elf(&mut self, path: &Path) -> Result<()> {
        // Arch gate: refuse a non-AVR ELF before simavr maps it into flash and
        // silently runs garbage. Raw images without an ELF header are skipped.
        crate::elf::validate_arch(path, crate::elf::EM_AVR, "atmega (AVR)")?;

        let elf_cstr = CString::new(
            path.to_str()
                .ok_or_else(|| anyhow::anyhow!("non-UTF-8 firmware path"))?,
        )?;

        unsafe {
            let mut fp = std::mem::zeroed::<ffi::elf_firmware_t>();
            let rc = ffi::elf_read_firmware(elf_cstr.as_ptr(), &mut fp);
            if rc != 0 {
                bail!(
                    "elf_read_firmware failed (rc={}) for '{}'",
                    rc,
                    path.display()
                );
            }
            ffi::avr_load_firmware(self.avr, &mut fp);
        }

        Ok(())
    }

    /// Inject a single byte into UART0 RX.
    fn uart_inject_byte(&mut self, byte: u8) {
        unsafe {
            let irq = ffi::avr_io_getirq(self.avr, uart_getirq(b'0'), UART_IRQ_INPUT);
            if !irq.is_null() {
                ffi::avr_raise_irq(irq, byte as u32);
            }
        }
    }

    /// Drive an individual input pin externally (indices 0-7 within the port).
    /// Same persistent-drive path as the synchronous responder
    /// ([`drive_ioport_input`]): the chunk-boundary net-voltage sync is also
    /// "the outside world holds this pin", so it must survive the firmware's
    /// PORT/DDR writes the same way, and the two writers share one external
    /// state (last writer owns the line).
    fn set_pin_raw(&mut self, port: char, bit: u8, high: bool) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let ext = &mut s.ext_drive;
        unsafe {
            drive_ioport_input(self.avr, ext, PinId { port, bit }, high);
        }
    }

    /// Read the current cycle counter directly from the avr_t struct.
    fn raw_cycle(&self) -> u64 {
        unsafe { (*self.avr).cycle }
    }
}

impl Mcu for AvrMcu {
    fn load_firmware(&mut self, path: &Path) -> Result<()> {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();

        match ext.as_str() {
            "hex" => self.load_hex(path)?,
            "elf" => self.load_elf(path)?,
            other => bail!(
                "unsupported firmware extension '.{}'; use .hex or .elf",
                other
            ),
        }

        self.firmware_loaded = true;
        Ok(())
    }

    fn run_cycles(&mut self, n: u64) -> Result<u64> {
        let start = self.raw_cycle();
        let target = start + n;

        loop {
            let current = self.raw_cycle();
            if current >= target {
                break;
            }
            let status = unsafe { ffi::avr_run(self.avr) };
            if status == ffi::cpu_Done as i32 || status == ffi::cpu_Crashed as i32 {
                break;
            }
        }

        Ok(self.raw_cycle() - start)
    }

    fn run_micros(&mut self, us: u64) -> Result<()> {
        let freq = self.frequency_hz;
        let cycles = us * freq / 1_000_000;
        self.run_cycles(cycles)?;
        Ok(())
    }

    fn frequency(&self) -> u64 {
        self.frequency_hz
    }

    fn current_cycle(&self) -> u64 {
        // Direct read of `avr->cycle`: cheaper than building a full McuState and
        // exact when called from inside the pin-change hook (same run slice).
        self.raw_cycle()
    }

    fn set_digital_in(&mut self, pin: PinId, high: bool) {
        self.set_pin_raw(pin.port, pin.bit, high);
    }

    fn pins_configured_output(&self) -> Vec<PinId> {
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = Vec::new();
        for (&port, ps) in &s.port_state {
            for bit in 0u8..8 {
                if (ps.output_dir >> bit) & 1 != 0 {
                    out.push(PinId { port, bit });
                }
            }
        }
        out
    }

    fn set_analog_in(&mut self, channel: u8, volts: f64) {
        let millivolts = (volts * 1000.0).round() as u32;
        unsafe {
            let irq = ffi::avr_io_getirq(self.avr, ADC_GETIRQ, channel as i32);
            if !irq.is_null() {
                ffi::avr_raise_irq(irq, millivolts);
            }
        }
    }

    fn on_pin_change(&mut self, cb: Box<dyn FnMut(PinId, bool, u64) + Send>) {
        {
            let mut s = self.state.lock().unwrap();
            s.callbacks.on_pin_change = Some(cb);
        }
        // Auto-register hooks for the standard ATmega328P ports.
        // Callers working with other MCUs can call register_port_hooks directly.
        let ports: Vec<char> = ['A', 'B', 'C', 'D'].to_vec();
        self.register_port_hooks(&ports);
    }

    fn on_input_responder(
        &mut self,
        responder: Box<dyn FnMut(PinId, bool) -> Vec<(PinId, bool)> + Send>,
    ) {
        {
            let mut s = self.state.lock().unwrap();
            s.callbacks.input_responder = Some(responder);
        }
        // The responder fires from the per-port output hook, so the standard
        // ATmega328P ports must be hooked even if `on_pin_change` was never set.
        let ports: Vec<char> = ['A', 'B', 'C', 'D'].to_vec();
        self.register_port_hooks(&ports);
    }

    fn uart_write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.uart_inject_byte(b);
        }
    }

    fn on_uart(&mut self, cb: Box<dyn FnMut(u8) + Send>) {
        let mut s = self.state.lock().unwrap();
        s.callbacks.on_uart = Some(cb);
    }

    fn on_i2c(&mut self, cb: Box<dyn FnMut(I2cEvent) -> Option<u8> + Send>) {
        {
            let mut s = self.state.lock().unwrap();
            s.callbacks.on_i2c = Some(cb);
        }
        self.register_twi_hook();
    }

    fn on_spi(&mut self, cb: Box<dyn FnMut(SpiEvent) -> u8 + Send>) {
        {
            let mut s = self.state.lock().unwrap();
            s.callbacks.on_spi = Some(cb);
        }
        self.register_spi_hook();
    }

    fn state(&self) -> McuState {
        unsafe {
            let pc = (*self.avr).pc;
            let cycles = (*self.avr).cycle;
            let sleeping = (*self.avr).state == ffi::cpu_Sleeping as i32;
            McuState {
                pc,
                cycles,
                sleeping,
            }
        }
    }
}

impl Drop for AvrMcu {
    fn drop(&mut self) {
        unsafe {
            if !self.avr.is_null() {
                ffi::avr_terminate(self.avr);
            }
            if !self.callback_ptr.is_null() {
                let _ = Box::from_raw(self.callback_ptr as *mut Arc<Mutex<SharedState>>);
            }
        }
    }
}
