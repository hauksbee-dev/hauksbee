//! simavr-backed AVR emulation.
//!
//! [`AvrMcu`] wraps a `simavr` `avr_t` instance and exposes the generic
//! [`Mcu`] trait.  All Tarski-specific logic lives in higher layers; this
//! module only deals in plain IRQ hooks and byte streams.

use crate::ffi;
use crate::traits::{I2cEvent, McuState, Mcu, PinId, SpiEvent};
use anyhow::{Result, bail};
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

const ADC_GETIRQ: u32 = avr_ioctl_def(b'a', b'd', b'c', b'0');
const TWI_GETIRQ: u32 = avr_ioctl_def(b't', b'w', b'i', 0);
const SPI_GETIRQ: u32 = avr_ioctl_def(b's', b'p', b'i', 0);

const UART_IRQ_INPUT: i32 = 0;
const UART_IRQ_OUTPUT: i32 = 1;
const TWI_IRQ_INPUT: i32 = 0;
const TWI_IRQ_OUTPUT: i32 = 1;
const SPI_IRQ_INPUT: i32 = 0;   // MISO (from peripheral into MCU)
const SPI_IRQ_OUTPUT: i32 = 1;  // MOSI (from MCU to peripheral)

/// Index into ioport IRQ array for the whole PORT register.
const IOPORT_IRQ_REG_PORT: i32 = 11;

// TWI message condition flags (from avr_twi.h)
const TWI_COND_START: u32 = 1 << 0;
const TWI_COND_STOP: u32 = 1 << 1;
const TWI_COND_ADDR: u32 = 1 << 2;
const TWI_COND_ACK: u32 = 1 << 3;
const TWI_COND_WRITE: u32 = 1 << 4;

// ---------------------------------------------------------------------------
// Callback state (written inside C callbacks, read from Rust)
// ---------------------------------------------------------------------------

type PinChangeCb = Box<dyn FnMut(PinId, bool) + Send>;
type UartCb = Box<dyn FnMut(u8) + Send>;
type I2cCb = Box<dyn FnMut(I2cEvent) -> Option<u8> + Send>;
type SpiCb = Box<dyn FnMut(SpiEvent) -> u8 + Send>;

struct Callbacks {
    on_pin_change: Option<PinChangeCb>,
    on_uart: Option<UartCb>,
    on_i2c: Option<I2cCb>,
    on_spi: Option<SpiCb>,
}

/// Per-port state tracked for edge detection.
struct PortState {
    /// Current port byte value.
    current: u8,
}

/// State shared between the Rust owner and C IRQ callbacks.
struct SharedState {
    /// Raw AVR pointer, needed to call avr_raise_irq from inside callbacks.
    avr_ptr: *mut ffi::avr_t,

    /// Port register values indexed by port letter.
    /// We only track ports that have registered hooks.
    port_state: std::collections::HashMap<char, PortState>,

    /// Active I2C transaction accumulator.
    twi_addr: u8,
    twi_active: bool,

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
                    // Fire callback for each bit that changed.
                    if let Some(cb) = &mut s.callbacks.on_pin_change {
                        let changed = new_val ^ prev_val;
                        for bit in 0u8..8 {
                            if (changed >> bit) & 1 != 0 {
                                let high = (new_val >> bit) & 1 != 0;
                                cb(PinId { port: $port_char, bit }, high);
                            }
                        }
                    }
                    s.port_state
                        .entry($port_char)
                        .or_insert(PortState { current: 0 })
                        .current = new_val;
                }
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

            // Fire user callback and use its return value (if any) — for START
            // events the return value is meaningless, but we keep the API uniform.
            if let Some(cb) = &mut s.callbacks.on_i2c {
                let _ = cb(I2cEvent::Start { addr: addr7, read: read_flag });
            }

            // Send ACK so the firmware's Wire library doesn't stall.
            if !avr.is_null() {
                let twi_in = ffi::avr_io_getirq(avr, TWI_GETIRQ, TWI_IRQ_INPUT);
                if !twi_in.is_null() {
                    let ack = ffi::avr_twi_irq_msg(TWI_COND_ACK as u8, addr7, 1);
                    ffi::avr_raise_irq(twi_in, ack);
                }
            }
        } else if msg_flags & TWI_COND_WRITE != 0 && s.twi_active {
            // Data byte written by firmware.
            let addr7 = s.twi_addr;
            let reply_byte = if let Some(cb) = &mut s.callbacks.on_i2c {
                cb(I2cEvent::Write { addr: addr7, data: data_byte }).unwrap_or(0)
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
            let _ = reply_byte; // ACK path; READ path would inject a byte differently
        } else if msg_flags & TWI_COND_STOP != 0 && s.twi_active {
            s.twi_active = false;
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

        let miso = if let Some(cb) = &mut s.callbacks.on_spi {
            cb(SpiEvent { mosi })
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
/// use galvani_mcu::{AvrMcu, Mcu};
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
            twi_addr: 0,
            twi_active: false,
            callbacks: Callbacks {
                on_pin_change: None,
                on_uart: None,
                on_i2c: None,
                on_spi: None,
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
        let elf_cstr = CString::new(
            path.to_str()
                .ok_or_else(|| anyhow::anyhow!("non-UTF-8 firmware path"))?,
        )?;

        unsafe {
            let mut fp = std::mem::zeroed::<ffi::elf_firmware_t>();
            let rc = ffi::elf_read_firmware(elf_cstr.as_ptr(), &mut fp);
            if rc != 0 {
                bail!("elf_read_firmware failed (rc={}) for '{}'", rc, path.display());
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

    /// Set an individual pin via the ioport IRQ (indices 0-7 within the port).
    fn set_pin_raw(&mut self, port: char, bit: u8, high: bool) {
        unsafe {
            let irq = ffi::avr_io_getirq(self.avr, ioport_getirq(port as u8), bit as i32);
            if !irq.is_null() {
                ffi::avr_raise_irq(irq, high as u32);
            }
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
            other => bail!("unsupported firmware extension '.{}' — use .hex or .elf", other),
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

    fn set_digital_in(&mut self, pin: PinId, high: bool) {
        self.set_pin_raw(pin.port, pin.bit, high);
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

    fn on_pin_change(&mut self, cb: Box<dyn FnMut(PinId, bool) + Send>) {
        {
            let mut s = self.state.lock().unwrap();
            s.callbacks.on_pin_change = Some(cb);
        }
        // Auto-register hooks for the standard ATmega328P ports.
        // Callers working with other MCUs can call register_port_hooks directly.
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
            McuState { pc, cycles, sleeping }
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
