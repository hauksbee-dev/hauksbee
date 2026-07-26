//! Renode-backed MCU emulation.
//!
//! [`RenodeBackend`] drives an external, headless Renode process over its
//! Monitor TCP protocol and a UART socket terminal, exposing the same generic
//! [`Mcu`](crate::traits::Mcu) trait the simavr [`AvrMcu`](crate::avr::AvrMcu)
//! backend implements. The engine's lockstep contract is unchanged: it calls
//! `run_micros`, exchanges GPIO/ADC/UART state, and the backend translates that
//! into Monitor commands.
//!
//! # Coupling model
//!
//! Renode is *poll-based* for GPIO output (you read a port's output-data
//! register) and *push-based* for GPIO input (`OnGPIO <pin> <bool>`). The
//! generic trait is callback-based, so after every `run_micros` chunk this
//! backend reads each configured port's ODR, diffs it against the previous
//! snapshot, and synthesises per-bit edge callbacks. That mirrors exactly how
//! the simavr backend's port hook detects bit edges, so the engine scheduler
//! sees identical behaviour regardless of backend.
//!
//! # Lockstep primitive
//!
//! `emulation RunFor "<seconds>"` advances virtual time by a precise bounded
//! amount and blocks until it elapses. With `SetGlobalAdvanceImmediately true`
//! Renode runs as fast as the host allows rather than pacing to wall-clock,
//! which is what we want when the analog solver, not wall time, sets the pace.
//!
//! Long-form how-and-why: docs/how-and-why/hauksbee-mcu/renode.md.

mod monitor;
mod process;
mod uart;

pub use process::{find_renode, is_available};

use crate::traits::{I2cEvent, Mcu, McuState, PinId, SpiEvent};
use anyhow::{bail, Context, Result};
use monitor::Monitor;
use serde::{Deserialize, Serialize};
use process::RenodeProcess;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use uart::UartSocket;

type I2cCb = Box<dyn FnMut(I2cEvent) -> Option<u8> + Send>;
type SpiCb = Box<dyn FnMut(SpiEvent) -> u8 + Send>;

/// How a single GPIO port is addressed inside Renode.
///
/// This is the per-port register-offset data (05-cosim-fidelity §5.5): the
/// STM32F1-vs-F4 ODR-offset footgun lives here as an explicit `odr_offset`
/// field, not as logic scattered through the backend, so a new part declares
/// "where do I read output state" as data. `Serialize`/`Deserialize` make it a
/// file-load target for W5 without any loader landing now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortMap {
    /// Logical port letter the engine uses in [`PinId`] (e.g. `'C'`).
    pub letter: char,
    /// Renode peripheral name, e.g. `"gpioPortC"`.
    pub peripheral: String,
    /// Byte offset of the output-data register within the peripheral.
    /// STM32F1 GPIO ODR is at 0x0C; many Cortex-M GPIO blocks differ.
    pub odr_offset: u32,
    /// Number of bits in this port (usually 16 on STM32, 32 on nRF52).
    pub width: u8,
    /// Where and how to read this port's DIRECTION/MODE register, if the
    /// platform model supports reading it back. `None` keeps the old
    /// conservative behavior: every ODR bit change is reported as a drive and
    /// direction stays unobservable ([`Mcu::drive_direction_observable`] false).
    ///
    /// A WRONG dir map silently corrupts the co-sim worse than no map (a mask
    /// that reads as 0 suppresses every edge), so a descriptor only carries one
    /// once the register offset AND the Renode model's read-back are verified
    /// against a live machine, see the per-part notes in `db/mcu/*.soc.toml`.
    #[serde(default)]
    pub dir: Option<DirMap>,
}

/// How a port's direction/mode register is read and decoded to a per-pin
/// "configured as output" mask. Families differ enough that the encoding is an
/// enum, not a bit-width parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirMap {
    /// Byte offset of the direction/mode register within the peripheral (for
    /// [`DirEncoding::Stm32f1CrlCrh`] this is CRL; CRH is read at `offset + 4`).
    pub offset: u32,
    /// How the register value maps to an output mask.
    pub encoding: DirEncoding,
}

/// Per-family decoding of a direction/mode register into a "1 = configured as
/// output" pin mask.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirEncoding {
    /// STM32F4/L4/F7-style MODER: 2 bits per pin, `0b01` = general-purpose
    /// output. AF mode (`0b10`) is deliberately NOT counted: an AF pin may be
    /// an input function, and the boot-state consumers of this mask reason
    /// about firmware GPIO drives.
    Moder,
    /// STM32F1-style CRL/CRH: 4 bits per pin, CRL at `offset` covers pins 0-7,
    /// CRH at `offset + 4` covers pins 8-15. The low 2 bits of each nibble are
    /// MODE; any non-`0b00` MODE is an output (GP or AF, push-pull or
    /// open-drain; the F1 encodes AF *outputs* here, unlike MODER).
    Stm32f1CrlCrh,
    /// One bit per pin, 1 = output (nRF52 `DIR`, RP2040 SIO `GPIO_OE`).
    DirBits,
}

/// Decode a direction/mode register read into a "1 = output" pin mask.
/// `low` is the word at `DirMap::offset`; `high` is the word at `offset + 4`
/// (only meaningful for [`DirEncoding::Stm32f1CrlCrh`], pass 0 otherwise).
/// Bits at or above `width` are cleared so a narrow bank never reports
/// phantom pins.
fn decode_dir_mask(encoding: DirEncoding, low: u32, high: u32, width: u8) -> u32 {
    let mut mask = 0u32;
    match encoding {
        DirEncoding::Moder => {
            for pin in 0..16u32 {
                if (low >> (2 * pin)) & 0b11 == 0b01 {
                    mask |= 1 << pin;
                }
            }
        }
        DirEncoding::Stm32f1CrlCrh => {
            for pin in 0..8u32 {
                if (low >> (4 * pin)) & 0b11 != 0 {
                    mask |= 1 << pin;
                }
                if (high >> (4 * pin)) & 0b11 != 0 {
                    mask |= 1 << (pin + 8);
                }
            }
        }
        DirEncoding::DirBits => mask = low,
    }
    if width < 32 {
        mask &= (1u32 << width) - 1;
    }
    mask
}

/// How a modeled ADC voltage is delivered into the Renode machine for one
/// engine ADC channel (05-cosim-fidelity §5.1).
///
/// Both variants ride the same Monitor TCP channel the backend already uses
/// for its ODR diffing, injection is one Monitor command per chunk, issued
/// while the machine is paused between `RunFor` steps, so the firmware sees
/// the new count before its next instruction runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AdcInject {
    /// Run a Renode Monitor command each chunk, with `{count}` replaced by the
    /// modeled ADC count and `{millivolts}` by the integer millivolt value.
    ///
    /// This is the peripheral-model path: a platform whose `.repl` carries a
    /// modeled ADC accepts its own feed API (e.g. Renode's `Analog.STM32_ADC`
    /// on the F0/L0 family takes `sysbus.adc SetDefaultValue {count}`), and
    /// the firmware then reads the count through its real ADC registers.
    MonitorCommand(String),
    /// Write the modeled count into a fixed word each chunk via
    /// `sysbus WriteDoubleWord <addr> <count>`: the ADC result RAM word (an
    /// address the firmware reads) or a memory-backed data register. This is
    /// the Monitor/RAM path of 05 §5.1; the write-direction twin of the ODR
    /// poll.
    MemoryWord(u32),
}

/// Maps one engine ADC channel to its Renode injection recipe (05 §5.1).
///
/// Not `Eq`: `full_scale_volts` is an `f64`. This is `PartialEq` for the config
/// round-trip proof (05 §5.5) and folds into `RenodeConfig::adc_channels` as
/// plain data, so a W5 descriptor can carry ADC recipes with no backend change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdcChannelMap {
    /// Engine-facing ADC channel index ([`Mcu::set_analog_in`]'s `channel`).
    pub channel: u8,
    /// How the count reaches the guest.
    pub inject: AdcInject,
    /// Voltage corresponding to `max_count` (e.g. 3.3 for a 3V3-referenced
    /// converter). Injected volts are clamped to `[0, full_scale_volts]`.
    pub full_scale_volts: f64,
    /// Count at full scale (4095 for a 12-bit converter).
    pub max_count: u32,
}

/// Per-MCU Renode configuration: enough to bring up a machine and wire it.
///
/// This is the whole per-part surface as plain data (05-cosim-fidelity §5.5):
/// the platform-description reference, GPIO port maps with their register
/// offsets, the UART/I2C/SPI controller names, and the ADC injection recipes are
/// all struct fields a constructor fills, not logic in the backend. Adding a
/// part is filling this struct (see [`RenodeConfig::rp2040`], the first config
/// written directly against the struct shape). `Serialize`/`Deserialize` make it
/// the file-load target for W5's data-driven MCU descriptor; no loader lands now
/// (the constructors below stay the source of truth for the built-in parts).
///
/// Not `Eq` because [`AdcChannelMap`] carries an `f64`; `PartialEq` backs the
/// round-trip bit-identity proof in the tests.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RenodeConfig {
    /// Human-readable machine name used in the Monitor prompt (e.g. `"f103"`).
    pub machine: String,
    /// Platform description loaded with `machine LoadPlatformDescription`,
    /// e.g. `"@platforms/cpus/stm32f103.repl"`.
    pub platform: String,
    /// CPU peripheral path for state queries, e.g. `"sysbus.cpu"`.
    pub cpu: String,
    /// UART peripheral to bridge to a host socket, e.g. `"sysbus.usart1"`.
    /// `None` disables the UART bridge.
    pub uart: Option<String>,
    /// GPIO ports to bridge, in engine-facing order.
    pub ports: Vec<PortMap>,
    /// Clock frequency in Hz reported by [`Mcu::frequency`]. Renode models the
    /// platform's own clocking; this is advisory for the engine's bookkeeping.
    pub frequency_hz: u64,
    /// Extra Monitor commands run verbatim after platform load and before the
    /// firmware is started (e.g. attaching a button so a GPIO input has a
    /// receiver). Each string is one Monitor command.
    pub extra_setup: Vec<String>,
    /// Extra Monitor commands run verbatim AFTER the firmware ELF is loaded
    /// (e.g. setting the CPU PC to a boot symbol the ELF entry does not point
    /// at, as the SiFive FE310 Zephyr demo needs `cpu PC vinit`). Empty for the
    /// common case where the ELF entry is correct. The literal `{cpu}` token is
    /// substituted with this config's `cpu` path so commands can be SoC-generic.
    pub post_load_setup: Vec<String>,
    /// Renode I2C controller names that can host engine-provided I2C slaves.
    /// The STM32F103 thermostat firmware uses `i2c1`.
    pub i2c_controllers: Vec<String>,
    /// Renode SPI controller names that can host engine-provided SPI slaves.
    /// The STM32F103 SPI ADC firmware uses `spi1`.
    pub spi_controllers: Vec<String>,
    /// Extra Monitor commands run AFTER the SPI1 peripheral is added to the
    /// platform (if any). Empty for most configurations.
    pub spi_extra_repl: Option<String>,
    /// ELF `e_machine` the platform's CPU executes (`EM_ARM` for the Cortex-M
    /// STM32 / nRF52 platforms, `EM_RISCV` for the SiFive FE310). Used by the
    /// firmware-load arch gate to refuse a wrong-ISA ELF before `sysbus LoadELF`
    /// runs it as garbage. See [`crate::elf`].
    pub expected_e_machine: u16,
    /// Human-readable MCU/board name for arch-mismatch error messages.
    pub mcu_label: String,
    /// Per-channel ADC injection recipes (05 §5.1). Empty means ADC injection
    /// is a LOUD drop (a once-per-channel stderr warning), never a silent one.
    ///
    /// The stock constructors leave this empty on purpose: the Renode 1.16
    /// platform descriptions for the F103 / F4-Discovery / nRF52840 / FE310
    /// model no ADC peripheral at all, and Renode's `Analog.STM32_ADC` speaks
    /// the F0/L0 register layout, registering it at an F1 address would let
    /// firmware "read" a peripheral whose registers are laid out wrong, which
    /// is fake fidelity ("refuse rather than fake", 00-MASTER-PLAN §5). A
    /// board/test that knows where its counts must land (a modeled ADC's feed
    /// command, or the RAM result word its firmware reads) supplies the map.
    pub adc_channels: Vec<AdcChannelMap>,
}

impl RenodeConfig {
    // ── Built-in parts (06 §2) ──────────────────────────────────────────────
    //
    // These are named accessors over the shipped `db/mcu/*.soc.toml` descriptors:
    // the register offsets, platform paths, and port maps that used to be
    // hand-written here now live in the TOML (the single source of truth,
    // embedded via `include_str!` so the binary stays self-contained; the
    // mcp4728.toml precedent). Each descriptor's header comment carries the
    // hard-won knowledge (the F1-vs-F4 ODR footgun, the RP2040 SIO adaptation,
    // the F4 SPI-redefinition trap, the FE310 PRCI/vinit bring-up). A fresh part
    // is added purely as data via [`crate::SocConfig::resolve`], these
    // constructors exist only for the in-tree callers that name a part directly.
    //
    // `.expect` is correct here: a shipped descriptor failing to load is a build
    // bug the `tests/soc_descriptors.rs` equivalence suite catches, never a
    // runtime condition.

    /// STM32F103C8 "blue pill". See `db/mcu/stm32f103.soc.toml`.
    pub fn stm32f103() -> Self {
        Self::from_soc_toml(include_str!("../../db/mcu/stm32f103.soc.toml"))
            .expect("built-in stm32f103.soc.toml is valid")
    }

    /// STM32F4 Discovery (STM32F407). See `db/mcu/stm32f4_discovery.soc.toml`.
    pub fn stm32f4_discovery() -> Self {
        Self::from_soc_toml(include_str!("../../db/mcu/stm32f4_discovery.soc.toml"))
            .expect("built-in stm32f4_discovery.soc.toml is valid")
    }

    /// nRF52840. See `db/mcu/nrf52840.soc.toml`.
    pub fn nrf52840() -> Self {
        Self::from_soc_toml(include_str!("../../db/mcu/nrf52840.soc.toml"))
            .expect("built-in nrf52840.soc.toml is valid")
    }

    /// SiFive FE310 (HiFive1) RISC-V. See `db/mcu/sifive_fe310.soc.toml`.
    pub fn sifive_fe310() -> Self {
        Self::from_soc_toml(include_str!("../../db/mcu/sifive_fe310.soc.toml"))
            .expect("built-in sifive_fe310.soc.toml is valid")
    }

    /// RP2040 (Raspberry Pi Pico). See `db/mcu/rp2040.soc.toml`, that file
    /// carries the full SIO-footgun and verification-status notes.
    pub fn rp2040() -> Self {
        Self::from_soc_toml(include_str!("../../db/mcu/rp2040.soc.toml"))
            .expect("built-in rp2040.soc.toml is valid")
    }

    /// Add one ADC channel injection recipe (05 §5.1). Chainable.
    pub fn with_adc_channel(mut self, map: AdcChannelMap) -> Self {
        self.adc_channels.push(map);
        self
    }
}

/// Convert a modeled voltage to an ADC count against a converter's full scale.
/// Clamps to `[0, max_count]`; a non-positive full scale yields 0 (a broken map
/// must read as "stuck at zero", not NaN-poisoned).
fn adc_count(volts: f64, full_scale_volts: f64, max_count: u32) -> u32 {
    if !(full_scale_volts > 0.0) {
        return 0;
    }
    let frac = (volts / full_scale_volts).clamp(0.0, 1.0);
    // Multiply by 2^n (= max_count + 1), not the top code 2^n-1, then clamp to
    // the top code; the LSB = Vref/2^n transfer function. Multiplying by
    // (2^n-1) systematically under-reads by up to ~1 LSB (the same fix applied
    // to the MCP3008 model in peripherals/spi.rs).
    ((frac * (f64::from(max_count) + 1.0)).round() as u32).min(max_count)
}

/// Render the Monitor command that delivers `count` for one channel's recipe.
/// `millivolts` is the already-clamped voltage (the caller applies the
/// channel's `[0, full_scale_volts]` clamp so BOTH placeholders stay inside
/// the converter's contract, not just `{count}`).
fn render_adc_inject(inject: &AdcInject, count: u32, millivolts: u64) -> String {
    match inject {
        AdcInject::MonitorCommand(template) => template
            .replace("{count}", &count.to_string())
            .replace("{millivolts}", &millivolts.to_string()),
        AdcInject::MemoryWord(addr) => {
            format!("sysbus WriteDoubleWord 0x{addr:X} 0x{count:X}")
        }
    }
}

/// Allocate two distinct free TCP ports, holding both listeners until both
/// numbers are read so the OS cannot reissue one to the other. Renode binds
/// each shortly after we release them.
fn free_port_pair() -> Result<(u16, u16)> {
    let a = std::net::TcpListener::bind(("127.0.0.1", 0)).context("allocating monitor TCP port")?;
    let b = std::net::TcpListener::bind(("127.0.0.1", 0)).context("allocating uart TCP port")?;
    let pa = a.local_addr()?.port();
    let pb = b.local_addr()?.port();
    // Distinct by construction (both listeners are bound simultaneously), but
    // assert to make any future regression loud rather than silent.
    anyhow::ensure!(pa != pb, "port allocator returned a collision");
    Ok((pa, pb))
    // listeners drop here, releasing both ports for Renode to bind.
}

/// Per-connection socket read/write timeout for both bridges. A genuinely stuck
/// peer trips this and the connection handler fails loudly rather than hanging
/// the emulation forever.
const BRIDGE_STREAM_TIMEOUT: Duration = Duration::from_secs(5);

/// Generic host-side bridge server shared by the I2C and SPI bridges.
///
/// Both bridges have the same lifecycle: bind an ephemeral loopback port, accept
/// connections on a background thread, hand each connection to a protocol-specific
/// handler, and tear the thread down on drop. The only differences are the
/// callback type `C` (`I2cCb` vs `SpiCb`) and the per-connection handler, so that
/// is all the bridge-specific code that remains; everything else lives here.
///
/// The callback is stored behind `Arc<Mutex<C>>` so the engine can swap it with
/// [`BridgeServer::replace_callback`] when a new board is bound without tearing
/// down the Renode peripheral that points at this port.
struct BridgeServer<C> {
    port: u16,
    callback: Arc<Mutex<C>>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl<C: Send + 'static> BridgeServer<C> {
    /// Start a bridge server. `label` names the bridge in error logs (e.g.
    /// `"I2C"`). `handler` services one accepted connection; it owns any
    /// per-thread state (the I2C bridge threads a small transaction state
    /// machine through it). A handler error is logged loudly and the connection
    /// dropped, a broken bridge must be visible, never silently absorbed.
    fn start<H>(label: &'static str, cb: C, mut handler: H) -> Result<Self>
    where
        H: FnMut(&mut TcpStream, &Arc<Mutex<C>>) -> Result<()> + Send + 'static,
    {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .with_context(|| format!("allocating Renode {label} bridge port"))?;
        let port = listener.local_addr()?.port();
        listener
            .set_nonblocking(true)
            .with_context(|| format!("setting Renode {label} bridge listener nonblocking"))?;

        let callback = Arc::new(Mutex::new(cb));
        let thread_callback = Arc::clone(&callback);
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = Arc::clone(&shutdown);

        let thread = thread::spawn(move || {
            while !thread_shutdown.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        // The listener is nonblocking; on some platforms (macOS)
                        // the accepted socket inherits that, which would make the
                        // handler's `read_exact` return WouldBlock instead of
                        // blocking for the timeout. Put it back into blocking mode
                        // with explicit read/write timeouts so a genuinely stuck
                        // peer trips the timeout and fails loudly, while a normal
                        // request blocks until its bytes arrive.
                        let _ = stream.set_nonblocking(false);
                        let _ = stream.set_read_timeout(Some(BRIDGE_STREAM_TIMEOUT));
                        let _ = stream.set_write_timeout(Some(BRIDGE_STREAM_TIMEOUT));
                        if let Err(e) = handler(&mut stream, &thread_callback) {
                            // FAIL LOUD: surface bridge/socket failures instead of
                            // letting them masquerade as valid bus traffic.
                            eprintln!("renode {label} bridge connection error: {e:#}");
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(BridgeServer {
            port,
            callback,
            shutdown,
            thread: Some(thread),
        })
    }

    fn port(&self) -> u16 {
        self.port
    }

    fn replace_callback(&self, cb: C) {
        let mut guard = self.callback.lock().unwrap_or_else(|e| e.into_inner());
        *guard = cb;
    }
}

impl<C> Drop for BridgeServer<C> {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        // Wake the accept loop out of its blocking-ish poll so it observes the
        // shutdown flag promptly, then join.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[derive(Default)]
struct I2cBridgeState {
    active: Option<(u8, I2cBridgeMode)>,
    /// STM32F1 two-byte-receive prefetch quirk (host-side policy).
    ///
    /// Renode 1.16.1's `STM32F4_I2C` controller model (used by the stock
    /// `stm32f103.repl`) asks the slave for bytes exactly ONCE per read
    /// transaction, `Read()` at the address phase, and never asks again when
    /// its receive fifo drains. Firmware running the RM0008 two-byte receive
    /// sequence (POS/ACK, then two DR reads gated on RxNE, see
    /// `testdata/firmware/stm32_i2c_thermostat/main.c`) would therefore time
    /// out waiting for RxNE on the second byte: the model's fifo holds only
    /// the single byte that one `Read()` returned. With this flag set, a
    /// `read_count == 1` request fetches TWO bytes from the slave callback and
    /// returns both, so the pending two-byte receive is filled while every
    /// byte still comes from the host model.
    ///
    /// The flag is deliberately OFF by default: on any platform whose
    /// controller model asks per byte, a genuine single-byte read must consume
    /// exactly one byte from the slave (a stateful slave's register pointer
    /// advances per byte served) and return exactly one byte. It is enabled
    /// per backend from the platform description (see
    /// [`platform_needs_i2c_single_read_prefetch`]).
    ///
    /// WHY `read_count == 1` IS THE ONLY POSSIBLE KEY (proven against the
    /// pinned model source, renode-infrastructure @ add012af; the exact
    /// submodule of the Renode v1.16.1 tag, `STM32F4_I2C.cs`):
    ///
    ///   - `DataWrite`, `State.AwaitingAddress`, read bit set:
    ///     `dataToReceive = new Queue<byte>(selectedSlave.Read());`; the ONE
    ///     call to the slave, with no argument, and
    ///     `II2CPeripheral.Read(int count = 1)`, so every STM32F1 read
    ///     transaction reaches this bridge as a single `read_count == 1`
    ///     request regardless of how many DR reads the firmware will perform.
    ///   - `DataRead()` only dequeues that local fifo (it logs "Tried to read
    ///     from an empty fifo" and returns 0 when it drains); it never calls
    ///     `selectedSlave.Read()` again, and RxNE is `dataToReceive.Any()`.
    ///   - The model never calls `FinishTransmission()` on the slave, and
    ///     `StopWrite` touches only controller-side state, a read
    ///     transaction produces NO bridge traffic after the address phase.
    ///
    /// A standalone one-byte read and the RM0008 two-byte receive are
    /// therefore byte-identical on the wire (one `READ count=1` message, then
    /// silence), so no discriminator, count, framing, or STOP boundary, can
    /// separate them here. Serving both correctly at once is impossible with
    /// this controller model; the platform gate plus the
    /// `HAUKSBEE_RENODE_I2C_SINGLE_READ_PREFETCH` override (see
    /// [`resolve_single_read_prefetch`]) are the entire available policy
    /// surface. The F1 default favours the two-byte receive because the
    /// alternative is a firmware hang (RxNE never sets for the second DR
    /// read), whereas the over-fetch only skews slaves that carry read state
    /// across transactions AND are read one byte at a time without a
    /// preceding pointer write; such firmware opts out with the env override.
    single_read_prefetch: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum I2cBridgeMode {
    Read,
    Write,
}

/// I2C bridge op codes, shared by the Rust handler and the generated C# source.
const I2C_OP_WRITE: u8 = 1;
const I2C_OP_READ: u8 = 2;
const I2C_OP_FINISH: u8 = 3;

/// Maximum number of bytes a single I2C bridge read request may ask for.
/// Matches the payload cap so both wire limits are named and obvious.
const I2C_BRIDGE_MAX_READ: usize = 4096;
/// Maximum payload size accepted from a single I2C bridge request.
const I2C_BRIDGE_MAX_PAYLOAD: usize = 4096;
/// Fixed I2C bridge request header: op(1) + addr(1) + read_count(4) + payload_len(4).
const I2C_BRIDGE_HEADER_LEN: usize = 10;

impl I2cBridgeState {
    /// Service one I2C bridge connection end to end, including writing the
    /// length-prefixed response back to Renode.
    ///
    /// FAIL LOUD: a clean EOF *before* any request header is benign (the
    /// drop-time wake-up connection, or a probe) and returns `Ok`. Any failure
    /// after that, a truncated header/payload, an oversized payload, a response
    /// write that does not land, or an undefined op code, returns `Err` so the
    /// server logs it rather than letting a broken bridge look like quiet,
    /// valid bus traffic.
    ///
    /// Note: a `Read` for which the callback returns `None` is NOT a bridge
    /// failure, `None` is the model layer's legitimate "no slave here / NACK",
    /// and `0xFF` is the level a real open-drain I2C bus floats to, so that byte
    /// is the faithful thing to clock back.
    fn handle_stream(
        &mut self,
        stream: &mut TcpStream,
        callback: &Arc<Mutex<I2cCb>>,
        trace: bool,
    ) -> Result<()> {
        // Read the 10-byte request header with explicit byte counting so we can
        // distinguish two different EOF cases:
        //   0 bytes read then EOF → benign: this is the drop-time wake-up
        //     connection / a probe; return Ok and let the accept loop continue.
        //   1..9 bytes read then EOF → TRUNCATED header: real corruption; fail
        //     loud so the broken bridge surfaces as an error rather than being
        //     silently swallowed as an Ok.
        //   10 bytes read → proceed normally.
        let mut header = [0u8; I2C_BRIDGE_HEADER_LEN];
        let mut bytes_read: usize = 0;
        loop {
            match stream.read(&mut header[bytes_read..]) {
                Ok(0) => {
                    if bytes_read == 0 {
                        // Clean EOF before any bytes: benign probe/wake-up.
                        return Ok(());
                    }
                    // EOF mid-header: truncated request.
                    anyhow::bail!(
                        "truncated Renode I2C bridge request header: got {bytes_read} of {I2C_BRIDGE_HEADER_LEN} bytes"
                    );
                }
                Ok(n) => {
                    bytes_read += n;
                    if bytes_read == I2C_BRIDGE_HEADER_LEN {
                        break;
                    }
                }
                Err(e) => {
                    return Err(e).context("reading Renode I2C bridge request header");
                }
            }
        }
        let op = header[0];
        let addr = header[1];
        let read_count = be_u32(&header[2..6]) as usize;
        let payload_len = be_u32(&header[6..10]) as usize;
        anyhow::ensure!(
            payload_len <= I2C_BRIDGE_MAX_PAYLOAD,
            "Renode I2C bridge payload too large: {payload_len}"
        );
        anyhow::ensure!(
            read_count <= I2C_BRIDGE_MAX_READ,
            "Renode I2C bridge read too large: {read_count}"
        );
        let mut payload = vec![0u8; payload_len];
        if payload_len != 0 {
            stream
                .read_exact(&mut payload)
                .context("reading Renode I2C bridge request payload")?;
        }
        if trace {
            eprintln!(
                "renode-i2c op={op} addr=0x{addr:02X} read_count={read_count} payload={payload:02X?}"
            );
        }

        let response = {
            let mut cb = callback.lock().unwrap_or_else(|e| e.into_inner());
            match op {
                I2C_OP_WRITE => {
                    self.ensure_mode(addr, I2cBridgeMode::Write, &mut cb);
                    for data in payload {
                        let _ = cb(I2cEvent::Write { addr, data });
                    }
                    Vec::new()
                }
                I2C_OP_READ => {
                    self.ensure_mode(addr, I2cBridgeMode::Read, &mut cb);
                    // See `single_read_prefetch`: with the flag off (every
                    // platform but the gated STM32F1s) a single-byte read
                    // consumes exactly one byte from the slave callback and
                    // returns exactly one byte. With it on, EVERY count==1
                    // request over-fetches, deliberately: the field docs
                    // prove the wire carries no signal that could separate a
                    // standalone one-byte read from the first half of the
                    // RM0008 two-byte receive under Renode 1.16.1's
                    // STM32F4_I2C, so a finer key does not exist.
                    let fetch_count = if read_count == 1 && self.single_read_prefetch {
                        2
                    } else {
                        read_count
                    };
                    let mut response = Vec::with_capacity(fetch_count);
                    for _ in 0..fetch_count {
                        response.push(cb(I2cEvent::Read { addr }).unwrap_or(0xFF));
                    }
                    if trace {
                        eprintln!("renode-i2c response addr=0x{addr:02X} bytes={response:02X?}");
                    }
                    response
                }
                I2C_OP_FINISH => {
                    if let Some((active_addr, _)) = self.active.take() {
                        let _ = cb(I2cEvent::Stop { addr: active_addr });
                    }
                    Vec::new()
                }
                other => bail!("Renode I2C bridge: unknown op code 0x{other:02X}"),
            }
        };

        write_be_u32(stream, response.len() as u32)
            .context("I2C bridge: failed to write response length to Renode")?;
        stream
            .write_all(&response)
            .context("I2C bridge: failed to write response payload to Renode")?;
        Ok(())
    }

    fn ensure_mode(&mut self, addr: u8, mode: I2cBridgeMode, cb: &mut I2cCb) {
        if self.active != Some((addr, mode)) {
            if mode == I2cBridgeMode::Write {
                if let Some((active_addr, _)) = self.active.take() {
                    let _ = cb(I2cEvent::Stop { addr: active_addr });
                }
            } else if let Some((active_addr, _)) = self.active {
                if active_addr != addr {
                    let _ = cb(I2cEvent::Stop { addr: active_addr });
                    self.active = None;
                }
            }
            let _ = cb(I2cEvent::Start {
                addr,
                read: mode == I2cBridgeMode::Read,
            });
            self.active = Some((addr, mode));
        }
    }
}

fn be_u32(bytes: &[u8]) -> u32 {
    u32::from(bytes[0]) << 24
        | u32::from(bytes[1]) << 16
        | u32::from(bytes[2]) << 8
        | u32::from(bytes[3])
}

fn write_be_u32(stream: &mut TcpStream, value: u32) -> std::io::Result<()> {
    stream.write_all(&[
        (value >> 24) as u8,
        (value >> 16) as u8,
        (value >> 8) as u8,
        value as u8,
    ])
}

// ─────────────────────────────────────────────────────────────────────────────
// SPI bridge
// ─────────────────────────────────────────────────────────────────────────────

/// SPI bridge op codes, shared by the Rust handler and the generated C# source.
const SPI_OP_TRANSMIT: u8 = 1;
const SPI_OP_FINISH: u8 = 2;

/// Handle one SPI bridge connection. The Renode-side C# `HauksbeeSpiBridge`
/// connects once per call. Protocol:
///   op=1 (Transmit): read 1 MOSI byte, call callback with deselect=false, write 1 MISO byte.
///   op=2 (FinishTransmission): call callback with deselect=true so the SpiBus
///         slave's deselect() fires promptly (resets the MCP3008 seq counter etc.)
///         rather than waiting for the next chunk boundary.
///
/// FAIL LOUD: a clean EOF *before* any op byte is benign (the drop-time wake-up
/// connection, or a probe), so it returns `Ok`. Any failure *after* committing to
/// an op, a truncated MOSI byte, a MISO write that does not land, or an op code
/// the protocol does not define, returns `Err` so the server logs it instead of
/// the firmware silently reading a plausible-but-fake bus byte.
fn handle_spi_stream(
    stream: &mut TcpStream,
    callback: &Arc<Mutex<SpiCb>>,
    cycle: &Arc<AtomicU64>,
    trace: bool,
) -> Result<()> {
    let mut op_buf = [0u8; 1];
    match stream.read_exact(&mut op_buf) {
        Ok(()) => {}
        // Empty connection (no bytes at all): the peer connected and closed
        // without a request. Benign, do not treat as a bridge failure.
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
        Err(e) => return Err(e).context("reading SPI bridge op byte"),
    }
    // Coarse chunk virtual-cycle stamp, shared from the backend thread (05 §2.2).
    let cyc = cycle.load(Ordering::Relaxed);
    match op_buf[0] {
        SPI_OP_TRANSMIT => {
            // Transmit: read one MOSI byte, return one MISO byte.
            let mut mosi_buf = [0u8; 1];
            stream
                .read_exact(&mut mosi_buf)
                .context("SPI bridge: socket closed before MOSI byte arrived")?;
            let mosi = mosi_buf[0];
            let miso = {
                let mut cb = callback.lock().unwrap_or_else(|e| e.into_inner());
                cb(SpiEvent {
                    mosi,
                    deselect: false,
                    cycle: cyc,
                })
            };
            if trace {
                eprintln!("renode-spi mosi=0x{mosi:02X} miso=0x{miso:02X}");
            }
            stream
                .write_all(&[miso])
                .context("SPI bridge: failed to write MISO byte back to Renode")?;
            Ok(())
        }
        SPI_OP_FINISH => {
            // FinishTransmission: CS deassert. Call the callback with deselect=true
            // so the SpiBus slave state machine (Mcp3008 seq counter etc.) resets
            // immediately, not at the next chunk boundary.
            if trace {
                eprintln!("renode-spi FinishTransmission");
            }
            let mut cb = callback.lock().unwrap_or_else(|e| e.into_inner());
            let _ = cb(SpiEvent {
                mosi: 0,
                deselect: true,
                cycle: cyc,
            });
            Ok(())
        }
        other => bail!("SPI bridge: unknown op code 0x{other:02X}"),
    }
}

/// Whether every port the backend will poll carries a direction-register map.
/// `active` is the engine's wired-ports hint (`None` = every configured port
/// is polled). This is the whole `drive_direction_observable` rule for the
/// Renode backend, factored out so it is testable without spawning a process.
fn dir_covers_ports(config: &RenodeConfig, active: Option<&[char]>) -> bool {
    let covered = |letter: char| {
        config
            .ports
            .iter()
            .any(|p| p.letter == letter && p.dir.is_some())
    };
    match active {
        Some(active) => active.iter().all(|&l| covered(l)),
        None => config.ports.iter().all(|p| p.dir.is_some()),
    }
}

/// Renode-backed [`Mcu`].
pub struct RenodeBackend {
    config: RenodeConfig,
    // Field order matters for drop: monitor and uart close before the process
    // is killed.
    monitor: Monitor,
    uart: Option<UartSocket>,
    _process: RenodeProcess,

    /// Last-read ODR per port letter, for edge synthesis. For a port with a
    /// dir map this stores the *output-masked* value (ODR & dir), so the diff
    /// basis matches what is reported.
    last_odr: HashMap<char, u32>,
    /// Last decoded "configured as output" mask per port letter, from that
    /// port's direction register (see [`DirMap`]). Only ports with a dir map
    /// get entries. Cached at each poll so the `&self` trait surface
    /// (`pins_configured_output`) can report it without a Monitor round-trip.
    last_dir: HashMap<char, u32>,
    /// If set, only these port letters are polled each chunk (the ports the
    /// engine actually wired). `None` means poll every configured port.
    active_ports: Option<Vec<char>>,
    /// Pin-change callback.
    on_pin_change: Option<Box<dyn FnMut(PinId, bool, u64) + Send>>,
    /// UART byte callback.
    on_uart: Option<Box<dyn FnMut(u8) + Send>>,
    /// 7-bit I2C addresses the engine attached to this MCU.
    i2c_slave_addresses: Vec<u8>,
    /// Host-side bridge serving Renode I2C peripheral callbacks.
    i2c_bridge: Option<BridgeServer<I2cCb>>,
    /// Per-controller SPI bridge servers: (controller_name, BridgeServer).
    /// Each entry owns a distinct TCP port and BridgeServer so transfers on
    /// "spi2" and "spi3" route to different callbacks without cross-talk.
    spi_bridges: Vec<(String, BridgeServer<SpiCb>)>,
    /// Temp `.cs` files generated for bridge peripherals, removed on drop so we
    /// do not litter `std::env::temp_dir()` across many test runs.
    bridge_source_files: Vec<PathBuf>,
    /// True once the `spi_extra_repl` fragment has been loaded into Renode.
    /// The fragment defines ALL the SPI controller peripherals at once, so it
    /// only needs to be loaded on the first bridge installation.
    spi_extra_repl_loaded: bool,
    firmware_loaded: bool,
    /// Virtual time advanced so far, in cycles-equivalent (frequency * seconds).
    cycles: u64,
    /// Coarse virtual-cycle stamp shared with the SPI bridge thread. The bridge
    /// services byte transfers on its own thread (the TCP server), so it cannot
    /// read `cycles` directly; this atomic is updated each time `cycles` advances
    /// and read when constructing a [`SpiEvent`], giving the byte the poll-boundary
    /// virtual time. Coarse by construction (all bytes in a slice share it), which
    /// is exactly the tier `cycle_exact()` reports false for (05 §2.2 QEMU-like).
    spi_cycle: Arc<AtomicU64>,
    /// When `HAUKSBEE_RENODE_TRACE=1`, the path of Renode's log file to which
    /// function-name trace lines are written. `None` when tracing is disabled.
    trace_log_path: Option<PathBuf>,
    /// Channels already warned about as un-mapped for ADC injection, so the
    /// loud drop prints once per channel instead of once per chunk.
    adc_unmapped_warned: std::collections::HashSet<u8>,
}

impl RenodeBackend {
    /// Spawn a Renode process, bring up the machine from `config`, and connect
    /// the Monitor and (optional) UART socket.
    pub fn new(config: RenodeConfig) -> Result<Self> {
        // Allocate both ports while holding both listeners, so the OS cannot
        // hand the same ephemeral port to two consecutive calls.
        let (monitor_port, uart_port) = free_port_pair()?;
        let process = RenodeProcess::spawn(monitor_port)?;

        let mut monitor = Monitor::connect(
            ("127.0.0.1", monitor_port),
            RenodeProcess::startup_timeout(),
        )?;
        monitor.set_timeout(Duration::from_secs(30));

        // Bring up the machine.
        let mach = monitor.command(&format!("mach create \"{}\"", config.machine))?;
        if monitor_failed(&mach) {
            bail!("Renode failed to create machine \"{}\": {mach}", config.machine);
        }
        let plat = monitor.command(&format!(
            "machine LoadPlatformDescription {}",
            config.platform
        ))?;
        if monitor_failed(&plat) {
            bail!("Renode failed to load platform {}: {plat}", config.platform);
        }

        // Run at host speed: the analog solver sets the pace, not wall time.
        let adv = monitor.command("emulation SetGlobalAdvanceImmediately true")?;
        if monitor_failed(&adv) {
            bail!("Renode failed to set global advance immediately: {adv}");
        }

        // Optional UART bridge: a server socket terminal on the pre-allocated
        // port (distinct from the monitor port by construction).
        let mut uart = None;
        if let Some(usart) = &config.uart {
            let term = monitor.command(&format!(
                "emulation CreateServerSocketTerminal {uart_port} \"hauksbee_uart\" false"
            ))?;
            if monitor_failed(&term) {
                bail!("Renode failed to create UART terminal on port {uart_port}: {term}");
            }
            let conn = monitor.command(&format!("connector Connect {usart} hauksbee_uart"))?;
            if monitor_failed(&conn) {
                bail!("Renode failed to connect UART {usart}: {conn}");
            }
            uart = Some(UartSocket::connect(uart_port, Duration::from_secs(10))?);
        }

        // Any platform-specific extra setup (e.g. attaching a button).
        for cmd in &config.extra_setup {
            let resp = monitor.command(cmd)?;
            if monitor_failed(&resp) {
                bail!("Renode extra setup command failed ({cmd}): {resp}");
            }
        }

        let last_odr = config.ports.iter().map(|p| (p.letter, 0u32)).collect();

        Ok(RenodeBackend {
            config,
            monitor,
            uart,
            _process: process,
            last_odr,
            last_dir: HashMap::new(),
            active_ports: None,
            on_pin_change: None,
            on_uart: None,
            i2c_slave_addresses: Vec::new(),
            i2c_bridge: None,
            spi_bridges: Vec::new(),
            bridge_source_files: Vec::new(),
            spi_extra_repl_loaded: false,
            firmware_loaded: false,
            cycles: 0,
            spi_cycle: Arc::new(AtomicU64::new(0)),
            trace_log_path: None,
            adc_unmapped_warned: std::collections::HashSet::new(),
        })
    }

    /// Convenience constructor for the STM32F103 blue pill.
    pub fn stm32f103() -> Result<Self> {
        Self::new(RenodeConfig::stm32f103())
    }

    /// Read one port's output-data register from the system bus.
    fn read_odr(&mut self, port: &PortMap) -> u32 {
        let cmd = format!(
            "sysbus.{} ReadDoubleWord 0x{:X}",
            port.peripheral, port.odr_offset
        );
        match self.monitor.command(&cmd) {
            Ok(resp) => parse_hex_or_dec(&resp)
                .unwrap_or_else(|| *self.last_odr.get(&port.letter).unwrap_or(&0)),
            Err(_) => *self.last_odr.get(&port.letter).unwrap_or(&0),
        }
    }

    /// Read one port's direction/mode register and decode it to a "1 = output"
    /// mask, updating the `last_dir` cache. `None` when the port has no dir
    /// map. A failed Monitor read falls back to the cached mask (same
    /// discipline as `read_odr`), so a transient hiccup never reads as a mass
    /// pin release.
    fn read_dir(&mut self, port: &PortMap) -> Option<u32> {
        let dir = port.dir?;
        let read_word = |monitor: &mut Monitor, offset: u32| -> Option<u32> {
            let cmd = format!("sysbus.{} ReadDoubleWord 0x{:X}", port.peripheral, offset);
            monitor.command(&cmd).ok().and_then(|r| parse_hex_or_dec(&r))
        };
        let low = read_word(&mut self.monitor, dir.offset);
        let high = match dir.encoding {
            DirEncoding::Stm32f1CrlCrh => read_word(&mut self.monitor, dir.offset + 4),
            _ => Some(0),
        };
        let mask = match (low, high) {
            (Some(l), Some(h)) => decode_dir_mask(dir.encoding, l, h, port.width),
            // Read failure: hold the previous mask rather than decoding
            // half-read garbage into a phantom release/drive.
            _ => *self.last_dir.get(&port.letter).unwrap_or(&0),
        };
        self.last_dir.insert(port.letter, mask);
        Some(mask)
    }

    /// Poll the relevant ports' ODRs, diff against the snapshot, fire edges.
    ///
    /// If the engine has hinted which ports are wired (`active_ports`), only
    /// those are queried; otherwise every configured port is polled.
    ///
    /// For a port with a dir map, the ODR is masked by the decoded
    /// output-direction mask before diffing: an ODR bit on a pin the firmware
    /// has NOT configured as an output is not a drive (on STM32/nRF it is
    /// meaningless until the pin becomes an output), so it must not synthesize
    /// a driven-level edge. Ports without a dir map keep the old behavior,
    /// every ODR change is reported, and direction stays unobservable.
    fn poll_gpio_edges(&mut self) {
        let ports: Vec<PortMap> = match &self.active_ports {
            Some(active) => self
                .config
                .ports
                .iter()
                .filter(|p| active.contains(&p.letter))
                .cloned()
                .collect(),
            None => self.config.ports.clone(),
        };
        // Poll boundary virtual time, in cycles-equivalent. Every edge observed
        // this poll shares it: the ODR diff cannot recover intra-slice ordering,
        // so the stamp is coarse and `cycle_exact()` is false for this backend
        // (05 §1.1). Snapshot before the mutable-callback borrow.
        let cyc = self.cycles;
        for port in &ports {
            let dir_mask = self.read_dir(port);
            let odr = self.read_odr(port);
            // Only a configured-output pin's ODR bit is a drive; a port with no
            // dir map reports every ODR bit (mask all-ones), the old behavior.
            let new = odr & dir_mask.unwrap_or(!0);
            let prev = *self.last_odr.get(&port.letter).unwrap_or(&0);
            if new != prev {
                let changed = new ^ prev;
                if let Some(cb) = &mut self.on_pin_change {
                    for bit in 0..port.width {
                        if (changed >> bit) & 1 != 0 {
                            let high = (new >> bit) & 1 != 0;
                            cb(
                                PinId {
                                    port: port.letter,
                                    bit,
                                },
                                high,
                                cyc,
                            );
                        }
                    }
                }
                self.last_odr.insert(port.letter, new);
            }
        }
    }

    /// Drain UART bytes the firmware emitted and dispatch them to the callback.
    fn pump_uart_out(&mut self) {
        if let Some(u) = &mut self.uart {
            let bytes = u.drain();
            let trace = std::env::var_os("HAUKSBEE_RENODE_I2C_TRACE").is_some()
                || std::env::var_os("HAUKSBEE_RENODE_SPI_TRACE").is_some();
            if !bytes.is_empty() && trace {
                eprintln!("renode-uart {}", String::from_utf8_lossy(&bytes));
            }
            if let Some(cb) = &mut self.on_uart {
                for b in bytes {
                    cb(b);
                }
            }
        }
    }

    /// Write a generated C# bridge peripheral to a temp file, `include` it into
    /// the running Renode, and remember the path so it is cleaned up on drop.
    /// Shared by the I2C and SPI bridge installers.
    fn install_bridge_source(&mut self, kind: &str, source: String, port: u16) -> Result<()> {
        let source_path = std::env::temp_dir().join(format!(
            "hauksbee-renode-{kind}-{}-{}.cs",
            std::process::id(),
            port
        ));
        std::fs::write(&source_path, source)
            .with_context(|| format!("writing {}", source_path.display()))?;
        let include = self
            .monitor
            .command(&format!("include \"{}\"", source_path.display()))?;
        if monitor_failed(&include) {
            // Leave the file in place for post-mortem inspection on failure.
            bail!("Renode failed to include {kind} bridge source: {include}");
        }
        self.bridge_source_files.push(source_path);
        Ok(())
    }

    fn install_i2c_bridge(&mut self, cb: I2cCb) -> Result<()> {
        if let Some(bridge) = &self.i2c_bridge {
            bridge.replace_callback(cb);
            return Ok(());
        }

        if self.config.i2c_controllers.is_empty() || self.i2c_slave_addresses.is_empty() {
            return Ok(());
        }

        let trace = std::env::var_os("HAUKSBEE_RENODE_I2C_TRACE").is_some();
        let single_read_prefetch = resolve_single_read_prefetch(
            std::env::var("HAUKSBEE_RENODE_I2C_SINGLE_READ_PREFETCH")
                .ok()
                .as_deref(),
            &self.config.platform,
        );
        let mut state = I2cBridgeState {
            single_read_prefetch,
            ..I2cBridgeState::default()
        };
        let bridge = BridgeServer::start("I2C", cb, move |stream, callback| {
            state.handle_stream(stream, callback, trace)
        })?;
        let source = render_i2c_bridge_source(bridge.port(), &self.i2c_slave_addresses);
        self.install_bridge_source("i2c", source, bridge.port())?;

        for controller in &self.config.i2c_controllers {
            for &addr in &self.i2c_slave_addresses {
                let class_name = format!("HauksbeeI2CBridge_{addr:02X}");
                // The device name must be unique per (controller, address):
                // Renode's Monitor namespace is machine-global, so a name
                // keyed only on the address collides the moment a platform
                // has TWO controllers (nRF52840 twi0+twi1, caught live by
                // tests/renode_nrf52840_bus.rs; the single-controller STM32
                // platforms never exposed it).
                let sanitized: String = controller
                    .chars()
                    .map(|c| if c.is_alphanumeric() { c } else { '_' })
                    .collect();
                let device_name = format!("hauksbee_i2c_{sanitized}_{addr:02x}");
                let repl = format!("{device_name}: I2C.{class_name} @ {controller} 0x{addr:02X}");
                let resp = self.monitor.command(&format!(
                    "machine LoadPlatformDescriptionFromString \"{repl}\""
                ))?;
                if monitor_failed(&resp) {
                    bail!(
                        "Renode failed to register I2C bridge at 0x{addr:02X} on {controller}: {resp}"
                    );
                }
            }
        }

        self.i2c_bridge = Some(bridge);
        Ok(())
    }

    /// Install or replace the SPI bridge for a specific named controller.
    ///
    /// If a bridge already exists for `controller`, the callback is swapped
    /// without tearing down the Renode peripheral. Otherwise a new
    /// `BridgeServer` is created on a fresh TCP port, a uniquely-named C#
    /// class is compiled into Renode, and the peripheral is registered on just
    /// that controller.
    ///
    /// Each controller gets its own C# class name (e.g. `HauksbeeSpiBridge_spi1`,
    /// `HauksbeeSpiBridge_spi2`) to avoid C# class-redefinition errors when
    /// multiple controllers are active simultaneously.
    fn install_spi_bridge_for(&mut self, controller: &str, cb: SpiCb) -> Result<()> {
        // Already bridged for this controller: swap the callback in-place.
        if let Some((_, bridge)) = self.spi_bridges.iter().find(|(c, _)| c == controller) {
            bridge.replace_callback(cb);
            return Ok(());
        }

        if self.config.spi_controllers.is_empty() {
            return Ok(());
        }

        let trace = std::env::var_os("HAUKSBEE_RENODE_SPI_TRACE").is_some();
        let cycle = Arc::clone(&self.spi_cycle);
        let bridge = BridgeServer::start("SPI", cb, move |stream, callback| {
            handle_spi_stream(stream, callback, &cycle, trace)
        })?;

        // Derive a unique C# class name: "HauksbeeSpiBridge_spi2", etc.
        // Replace non-alphanumeric characters with underscores.
        let sanitized: String = controller
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '_' })
            .collect();
        let class_name = format!("HauksbeeSpiBridge_{sanitized}");
        let device_name = format!("hauksbee_spi_{sanitized}");

        let source = render_spi_bridge_source(&class_name, bridge.port());
        self.install_bridge_source(&format!("spi_{sanitized}"), source, bridge.port())?;

        // Load the extra repl fragment (all SPI controller definitions) on the
        // first bridge installation. Subsequent controllers reuse already-loaded
        // platform peripherals.
        if !self.spi_extra_repl_loaded {
            if let Some(extra) = self.config.spi_extra_repl.clone() {
                let escaped = extra.replace('"', "\\\"");
                let resp = self
                    .monitor
                    .command(&format!("machine LoadPlatformDescriptionFromString \"{escaped}\""))?;
                if monitor_failed(&resp) {
                    bail!("Renode failed to add SPI controllers: {resp}");
                }
            }
            self.spi_extra_repl_loaded = true;
        }

        // Register the bridge peripheral on just this controller.
        // SPI uses NullRegistrationPoint: no address, just `@ spi2`.
        let repl = format!("{device_name}: SPI.{class_name} @ {controller}");
        let resp = self
            .monitor
            .command(&format!("machine LoadPlatformDescriptionFromString \"{repl}\""))?;
        if monitor_failed(&resp) {
            bail!("Renode failed to register SPI bridge on {controller}: {resp}");
        }

        self.spi_bridges.push((controller.to_string(), bridge));
        Ok(())
    }
}

impl Drop for RenodeBackend {
    fn drop(&mut self) {
        // Print the execution trace (if enabled) before tearing down Renode,
        // so the log file is fully flushed while Renode is still running.
        self.dump_trace();

        // Tear the bridge servers down first so their accept threads stop before
        // we remove the C# sources they were generated from. (Field-drop order
        // would also do this, but being explicit keeps the intent clear.)
        self.i2c_bridge = None;
        self.spi_bridges.clear(); // stops all per-controller accept threads
        for path in self.bridge_source_files.drain(..) {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn monitor_failed(resp: &str) -> bool {
    let lower = resp.to_lowercase();
    lower.contains("error") || lower.contains("exception") || lower.contains("failed")
}

/// Does this platform's I2C controller model need the single-byte-read
/// prefetch (see [`I2cBridgeState::single_read_prefetch`])?
///
/// The quirk exists for Renode 1.16.1's eager `STM32F4_I2C` model as
/// instantiated by the stock STM32F1 platform description, where the shipped
/// `stm32_i2c_thermostat` firmware runs the RM0008 two-byte receive sequence.
/// Only the STM32F1 platforms get it; everywhere else (including the
/// F4-Discovery, where the bridge wiring was collateral and no in-tree
/// firmware depends on the sequence) a single-byte read is served exactly.
fn platform_needs_i2c_single_read_prefetch(platform: &str) -> bool {
    platform.to_ascii_lowercase().contains("stm32f1")
}

/// The complete prefetch policy: the platform default from
/// [`platform_needs_i2c_single_read_prefetch`], overridable by
/// `HAUKSBEE_RENODE_I2C_SINGLE_READ_PREFETCH=1/0` (any other value falls back
/// to the platform default).
///
/// The override is not a convenience; it is one half of the policy surface.
/// [`I2cBridgeState::single_read_prefetch`]'s docs prove that under Renode
/// 1.16.1's `STM32F4_I2C` a standalone one-byte read and the RM0008 two-byte
/// receive are indistinguishable on the wire, so no per-request heuristic can
/// serve both. `=0` is the documented opt-out for STM32F1 firmware that reads
/// stateful, auto-incrementing slaves one byte at a time (each read then
/// consumes exactly one slave byte, at the cost of hanging any two-byte
/// receive); `=1` opts an out-of-tree platform in when its controller model
/// shares the one-shot `Read()` behaviour.
fn resolve_single_read_prefetch(override_var: Option<&str>, platform: &str) -> bool {
    match override_var {
        Some("1") => true,
        Some("0") => false,
        _ => platform_needs_i2c_single_read_prefetch(platform),
    }
}

fn render_i2c_bridge_source(port: u16, addresses: &[u8]) -> String {
    let mut classes = String::new();
    for addr in addresses {
        classes.push_str(&format!(
            r#"
    public class HauksbeeI2CBridge_{addr:02X} : HauksbeeI2CBridgeBase
    {{
        public HauksbeeI2CBridge_{addr:02X}() : base({port}, 0x{addr:02X}) {{ }}
    }}
"#
        ));
    }

    format!(
        r#"using System;
using System.IO;
using System.Net.Sockets;

namespace Antmicro.Renode.Peripherals.I2C
{{
    public abstract class HauksbeeI2CBridgeBase : II2CPeripheral
    {{
        private readonly int port;
        private readonly byte address;

        protected HauksbeeI2CBridgeBase(int port, byte address)
        {{
            this.port = port;
            this.address = address;
        }}

        public void Write(byte[] data)
        {{
            Request(1, data ?? new byte[0], 0);
        }}

        public byte[] Read(int count)
        {{
            // The host decides how many bytes come back. On the STM32F1
            // platforms it prefetches two bytes for a count==1 request,
            // because Renode 1.16.1's STM32F4_I2C model asks the slave exactly
            // once per read transaction and never re-asks when its fifo
            // drains; the STM32F1 two-byte receive sequence would time out
            // waiting for RxNE otherwise. Everywhere else the response is
            // exactly `count` bytes, so a genuine single-byte read consumes
            // exactly one byte from the host model. Keeping that policy on the
            // Rust side (I2cBridgeState::single_read_prefetch) makes it
            // unit-testable; this class just forwards the true count.
            var response = Request(2, new byte[0], count);
            if(response.Length >= count)
            {{
                return response;
            }}

            var padded = new byte[count];
            for(var i = 0; i < count; i++)
            {{
                padded[i] = i < response.Length ? response[i] : (byte)0xFF;
            }}
            return padded;
        }}

        public void FinishTransmission()
        {{
            Request(3, new byte[0], 0);
        }}

        public void Reset()
        {{
        }}

        private byte[] Request(byte op, byte[] payload, int readCount)
        {{
            try
            {{
                using(var client = new TcpClient("127.0.0.1", port))
                {{
                    client.ReceiveTimeout = 5000;
                    client.SendTimeout = 5000;
                    var stream = client.GetStream();
                    stream.WriteByte(op);
                    stream.WriteByte(address);
                    WriteInt(stream, readCount);
                    WriteInt(stream, payload.Length);
                    if(payload.Length != 0)
                    {{
                        stream.Write(payload, 0, payload.Length);
                    }}

                    var lengthBytes = ReadExact(stream, 4);
                    var length = ReadInt(lengthBytes);
                    return ReadExact(stream, length);
                }}
            }}
            catch(Antmicro.Renode.Exceptions.RecoverableException)
            {{
                throw;
            }}
            catch(Exception e)
            {{
                // FAIL LOUD: a broken bridge (host server down, socket EOF,
                // timeout) must abort the co-sim as a Renode error rather than
                // returning an empty array that the controller would read as a
                // plausible NACK/idle bus.
                throw new Antmicro.Renode.Exceptions.RecoverableException(
                    "Hauksbee I2C bridge request failed: " + e.Message);
            }}
        }}

        private static void WriteInt(Stream stream, int value)
        {{
            stream.WriteByte((byte)((value >> 24) & 0xFF));
            stream.WriteByte((byte)((value >> 16) & 0xFF));
            stream.WriteByte((byte)((value >> 8) & 0xFF));
            stream.WriteByte((byte)(value & 0xFF));
        }}

        private static int ReadInt(byte[] bytes)
        {{
            return (bytes[0] << 24)
                | (bytes[1] << 16)
                | (bytes[2] << 8)
                | bytes[3];
        }}

        private static byte[] ReadExact(Stream stream, int count)
        {{
            var bytes = new byte[count];
            var offset = 0;
            while(offset < count)
            {{
                var read = stream.Read(bytes, offset, count - offset);
                if(read == 0)
                {{
                    throw new IOException("I2C bridge socket closed");
                }}
                offset += read;
            }}
            return bytes;
        }}
    }}
{classes}}}
"#
    )
}

/// Generate C# source for a Renode SPI bridge peripheral.
///
/// `class_name` must be unique per controller (e.g. `"HauksbeeSpiBridge_spi1"`,
/// `"HauksbeeSpiBridge_spi2"`) to avoid C# class-redefinition errors when
/// multiple controllers are active simultaneously.
///
/// The peripheral implements `ISPIPeripheral`. For each `Transmit(byte)` call it
/// connects to the host-side [`BridgeServer`], sends op=1 + the MOSI byte, reads
/// back one MISO byte, and returns it. `FinishTransmission` sends op=2 with no
/// payload; the Rust side maps that to a `deselect` [`SpiEvent`] so the slave
/// state machine resets promptly (the chunk-boundary deselect in the scheduler
/// is the backstop for soft-NSS firmware that never triggers it).
fn render_spi_bridge_source(class_name: &str, port: u16) -> String {
    format!(
        r#"using System;
using System.IO;
using System.Net.Sockets;

namespace Antmicro.Renode.Peripherals.SPI
{{
    public class {class_name} : ISPIPeripheral
    {{
        private readonly int port;

        public {class_name}()
        {{
            this.port = {port};
        }}

        public byte Transmit(byte data)
        {{
            // FAIL LOUD: a broken bridge (host server down, socket EOF mid-byte,
            // timeout) must surface as a Renode emulation error, NOT a plausible
            // 0xFF that the firmware would mistake for a real MISO byte. Throwing
            // here propagates up through STM32SPI as a RecoverableException so the
            // co-sim aborts instead of silently producing fake ADC data.
            try
            {{
                using(var client = new TcpClient("127.0.0.1", port))
                {{
                    client.ReceiveTimeout = 5000;
                    client.SendTimeout = 5000;
                    var stream = client.GetStream();
                    stream.WriteByte(1);    // op: Transmit
                    stream.WriteByte(data); // MOSI byte
                    var miso = new byte[1];
                    var offset = 0;
                    while(offset < 1)
                    {{
                        var n = stream.Read(miso, offset, 1 - offset);
                        if(n == 0)
                        {{
                            throw new Antmicro.Renode.Exceptions.RecoverableException(
                                "Hauksbee SPI bridge: socket closed before MISO byte arrived");
                        }}
                        offset += n;
                    }}
                    return miso[0];
                }}
            }}
            catch(Antmicro.Renode.Exceptions.RecoverableException)
            {{
                throw;
            }}
            catch(Exception e)
            {{
                throw new Antmicro.Renode.Exceptions.RecoverableException(
                    "Hauksbee SPI bridge Transmit failed: " + e.Message);
            }}
        }}

        public void FinishTransmission()
        {{
            // FAIL LOUD: a CS-deassert failure means the bridge socket is dead.
            // The next Transmit would desync (host and emulated firmware out of
            // step), so the right call is to abort the co-sim as a Renode error.
            // This mirrors the I2C bridge's Request() and the SPI Transmit() above.
            try
            {{
                using(var client = new TcpClient("127.0.0.1", port))
                {{
                    client.ReceiveTimeout = 1000;
                    client.SendTimeout = 1000;
                    var stream = client.GetStream();
                    stream.WriteByte(2); // op: FinishTransmission
                }}
            }}
            catch(Antmicro.Renode.Exceptions.RecoverableException)
            {{
                throw;
            }}
            catch(Exception e)
            {{
                throw new Antmicro.Renode.Exceptions.RecoverableException(
                    "Hauksbee SPI bridge FinishTransmission failed: " + e.Message);
            }}
        }}

        public void Reset()
        {{
        }}
    }}
}}
"#
    )
}

impl Mcu for RenodeBackend {
    fn load_firmware(&mut self, path: &Path) -> Result<()> {
        // Renode resolves `@<path>` against ITS OWN working directory, not ours, so
        // a relative firmware path (e.g. `../hunt-boards/.../fw.elf`) makes
        // `sysbus LoadELF` fail with a cryptic "the following methods are
        // available". Canonicalize to an absolute path so the load is cwd-proof.
        let abs = std::fs::canonicalize(path)
            .with_context(|| format!("firmware not found: {}", path.display()))?;
        let p = abs.to_str().context("non-UTF-8 firmware path")?;
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        // Arch gate: refuse a wrong-ISA ELF before Renode's `sysbus LoadELF`
        // maps it and the Cortex-M/RISC-V core runs it as garbage. Raw .bin/.hex
        // carry no recoverable arch, so the gate is a no-op for them (the .bin
        // arm below bails for an unrelated reason; .hex is skipped). See
        // [`crate::elf`].
        crate::elf::validate_arch(path, self.config.expected_e_machine, &self.config.mcu_label)?;

        let resp = match ext.as_str() {
            "elf" | "" => self.monitor.command(&format!("sysbus LoadELF @{p}"))?,
            "hex" => self.monitor.command(&format!("sysbus LoadHEX @{p}"))?,
            "bin" => bail!("raw .bin needs a load address; supply an ELF instead"),
            other => bail!("unsupported firmware extension '.{other}' — use .elf or .hex"),
        };
        if monitor_failed(&resp) {
            bail!("Renode failed to load firmware {p}: {resp}");
        }
        // Post-load Monitor commands (e.g. FE310 needs `cpu PC vinit` and PRCI
        // clock tags after the ELF is loaded). `{cpu}` is substituted with the
        // configured CPU path so commands stay SoC-generic.
        let cpu = self.config.cpu.clone();
        let post = self.config.post_load_setup.clone();
        for cmd in &post {
            let cmd = cmd.replace("{cpu}", &cpu);
            let r = self.monitor.command(&cmd)?;
            if monitor_failed(&r) {
                bail!("Renode post-load command failed ({cmd}): {r}");
            }
        }
        self.firmware_loaded = true;

        // Execution-trace introspection: opt-in via HAUKSBEE_RENODE_TRACE=1.
        // When set, redirect Renode's log to a temp file and enable
        // symbol-based function-name logging so every CPU function entry is
        // recorded. The trace is printed to stderr at the end of the run (see
        // `dump_trace` / `Drop`). Zero overhead when the env var is unset.
        if std::env::var_os("HAUKSBEE_RENODE_TRACE").is_some() {
            let trace_path = std::env::temp_dir().join(format!(
                "hauksbee-renode-trace-{}.log",
                std::process::id()
            ));
            let path_str = trace_path.to_str().unwrap_or("").to_string();
            // `logFile @<path>` redirects Renode's main log to the file.
            // The `@` prefix is Renode's convention for an absolute path.
            let log_cmd = format!("logFile @{path_str}");
            let log_resp = self.monitor.command(&log_cmd)?;
            if monitor_failed(&log_resp) {
                eprintln!("[hauksbee-trace] warning: logFile command failed: {log_resp}");
            }
            // `sysbus.cpu LogFunctionNames true true`:
            //   first bool  = enable function-name logging,
            //   second bool = also log guessed (non-entry) symbols.
            let cpu = self.config.cpu.clone();
            let fn_cmd = format!("{cpu} LogFunctionNames true true");
            let fn_resp = self.monitor.command(&fn_cmd)?;
            if monitor_failed(&fn_resp) {
                eprintln!("[hauksbee-trace] warning: LogFunctionNames command failed: {fn_resp}");
            } else {
                eprintln!("[hauksbee-trace] function tracing enabled → {}", trace_path.display());
                self.trace_log_path = Some(trace_path);
            }
        }

        Ok(())
    }

    fn run_cycles(&mut self, n: u64) -> Result<u64> {
        let seconds = n as f64 / self.config.frequency_hz as f64;
        self.run_seconds(seconds)?;
        Ok(n)
    }

    fn run_micros(&mut self, us: u64) -> Result<()> {
        self.run_seconds(us as f64 / 1_000_000.0)
    }

    fn frequency(&self) -> u64 {
        self.config.frequency_hz
    }

    fn set_digital_in(&mut self, pin: PinId, high: bool) {
        // Find the Renode peripheral for this logical port and drive the pin.
        if let Some(port) = self
            .config
            .ports
            .iter()
            .find(|p| p.letter == pin.port)
            .cloned()
        {
            // Fail loud like set_analog_in and every other Monitor command in
            // this backend: silently discarding the result let a rejected/failed
            // OnGPIO masquerade as "input never changed", diverging the co-sim
            // from the analog solve with zero diagnostic.
            let cmd = format!("sysbus.{} OnGPIO {} {}", port.peripheral, pin.bit, high);
            match self.monitor.command(&cmd) {
                Ok(resp) if !monitor_failed(&resp) => {}
                Ok(resp) => panic!("Renode digital-input drive failed ({cmd}): {resp}"),
                Err(e) => panic!("Renode digital-input drive failed ({cmd}): {e:#}"),
            }
        }
    }

    fn set_analog_in(&mut self, channel: u8, volts: f64) {
        // ADC injection through the Monitor/RAM path (05 §5.1): translate the
        // modeled voltage into a count and deliver it per this channel's
        // config recipe (a modeled-ADC feed command, or a WriteDoubleWord into
        // the result word the firmware reads). The Monitor is idle between
        // `RunFor` chunks, so the write lands before the next chunk executes,
        // the same cadence at which the scheduler pushes ADC voltages.
        let Some(map) = self
            .config
            .adc_channels
            .iter()
            .find(|m| m.channel == channel)
            .cloned()
        else {
            // LOUD drop, once per channel: an unmapped platform must not
            // silently swallow the scheduler's ADC pushes (the pre-05 §5.1
            // behaviour this replaces).
            if self.adc_unmapped_warned.insert(channel) {
                eprintln!(
                    "renode: DROPPING ADC injection for channel {channel} on '{}': \
                     no AdcChannelMap configured (the stock Renode platform models \
                     no ADC; supply RenodeConfig::adc_channels to enable injection)",
                    self.config.machine
                );
            }
            return;
        };
        let count = adc_count(volts, map.full_scale_volts, map.max_count);
        let clamped_mv =
            (volts.clamp(0.0, map.full_scale_volts.max(0.0)) * 1000.0).round() as u64;
        let cmd = render_adc_inject(&map.inject, count, clamped_mv);
        // FAIL LOUD, matching the on_i2c/on_spi bridge discipline: a failed
        // injection means the firmware quietly reads a stale/zero count as if
        // it were real, exactly the fake-data mode this backend refuses.
        match self.monitor.command(&cmd) {
            Ok(resp) if !monitor_failed(&resp) => {}
            Ok(resp) => panic!("Renode ADC injection failed ({cmd}): {resp}"),
            Err(e) => panic!("Renode ADC injection failed ({cmd}): {e:#}"),
        }
    }

    fn on_pin_change(&mut self, cb: Box<dyn FnMut(PinId, bool, u64) + Send>) {
        self.on_pin_change = Some(cb);
    }

    fn current_cycle(&self) -> u64 {
        self.cycles
    }

    fn cycle_exact(&self) -> bool {
        // Poll-based: GPIO edges are observed by diffing ODRs per time slice, so
        // toggles within a slice collapse and the ordering is coarse (05 §1.1).
        false
    }

    fn uart_write(&mut self, bytes: &[u8]) {
        if let Some(u) = &mut self.uart {
            let _ = u.write_bytes(bytes);
        }
    }

    fn on_uart(&mut self, cb: Box<dyn FnMut(u8) + Send>) {
        self.on_uart = Some(cb);
    }

    fn on_i2c(&mut self, cb: Box<dyn FnMut(I2cEvent) -> Option<u8> + Send>) {
        if let Err(e) = self.install_i2c_bridge(cb) {
            panic!("failed to install Renode I2C bridge: {e:#}");
        }
    }

    fn on_spi(&mut self, cb: Box<dyn FnMut(SpiEvent) -> u8 + Send>) {
        // Route to the first configured SPI controller (backward-compat for
        // single-controller setups such as the STM32F103 SPI ADC demo).
        let controller = self
            .config
            .spi_controllers
            .first()
            .cloned()
            .unwrap_or_default();
        if controller.is_empty() {
            return;
        }
        if let Err(e) = self.install_spi_bridge_for(&controller, cb) {
            panic!("failed to install Renode SPI bridge on {controller}: {e:#}");
        }
    }

    fn on_spi_controller(
        &mut self,
        controller: &str,
        cb: Box<dyn FnMut(SpiEvent) -> u8 + Send>,
    ) {
        if let Err(e) = self.install_spi_bridge_for(controller, cb) {
            panic!("failed to install Renode SPI bridge on {controller}: {e:#}");
        }
    }

    fn state(&self) -> McuState {
        // `state` takes &self but the Monitor needs &mut to query; rather than
        // interior mutability we report the cached cycle count, which is what
        // the scheduler uses. PC is read lazily as 0 when unavailable.
        McuState {
            pc: 0,
            cycles: self.cycles,
            sleeping: false,
            // Renode's poll path carries no terminal-CPU signal here;
            // conservatively report "still running" rather than guessing.
            done: false,
            crashed: false,
        }
    }

    fn set_active_ports(&mut self, ports: &[char]) {
        // Keep only ports this backend actually models; ignore the rest.
        let known: Vec<char> = self
            .config
            .ports
            .iter()
            .filter(|p| ports.contains(&p.letter))
            .map(|p| p.letter)
            .collect();
        self.active_ports = Some(known);
    }

    fn pins_configured_output(&self) -> Vec<PinId> {
        // From the per-poll direction-register cache. Latest direction wins
        // (the mask IS the current register value), matching the AVR DDR
        // semantics: a pin released back to input drops out of the set. Ports
        // without a dir map contribute nothing, conservative, and consistent
        // with `drive_direction_observable` reporting false for them.
        let mut out = Vec::new();
        for port in &self.config.ports {
            let Some(&mask) = self.last_dir.get(&port.letter) else {
                continue;
            };
            for bit in 0..port.width {
                if (mask >> bit) & 1 != 0 {
                    out.push(PinId {
                        port: port.letter,
                        bit,
                    });
                }
            }
        }
        out
    }

    fn drive_direction_observable(&self) -> bool {
        // True once every port this backend polls carries a verified dir map:
        // then an empty configured-output set is authoritative ("nothing is an
        // output"), not "cannot tell". If the engine hinted the wired ports,
        // only those must be covered; otherwise every configured port must be.
        dir_covers_ports(&self.config, self.active_ports.as_deref())
    }

    fn set_i2c_slave_addresses(&mut self, addresses: &[u8]) {
        self.i2c_slave_addresses = addresses.to_vec();
        self.i2c_slave_addresses.sort_unstable();
        self.i2c_slave_addresses.dedup();
    }

    fn adc_dropped_channels(&self) -> Vec<u8> {
        // Exactly the channels the loud-drop path in `set_analog_in` recorded:
        // injections the scheduler pushed that this platform had no
        // `AdcChannelMap` for. Sorted so every report surface names them in a
        // deterministic order.
        let mut chans: Vec<u8> = self.adc_unmapped_warned.iter().copied().collect();
        chans.sort_unstable();
        chans
    }

    fn i2c_bus_modeled(&self) -> bool {
        // Mirrors `install_i2c_bridge`'s early return exactly: with no
        // configured controller the bridge is never installed and a bound I2C
        // slave receives no transactions.
        !self.config.i2c_controllers.is_empty()
    }

    fn spi_bus_modeled(&self, _controller: Option<&str>) -> bool {
        // Mirrors `on_spi` / `install_spi_bridge_for`: with an empty controller
        // list neither path installs a bridge, named controller or not.
        !self.config.spi_controllers.is_empty()
    }
}

impl RenodeBackend {
    /// Advance virtual time by `seconds`, then exchange GPIO/UART state.
    fn run_seconds(&mut self, seconds: f64) -> Result<()> {
        if !self.firmware_loaded {
            bail!("no firmware loaded into the Renode machine");
        }

        // Publish the chunk-start virtual cycle to the SPI bridge thread before
        // running: byte transfers serviced DURING this RunFor read this stamp, so
        // every byte in the slice carries the chunk's coarse virtual time (poll
        // tier, `cycle_exact()` is false). Exact intra-slice ordering is not
        // recoverable on a poll backend, matching the pin-edge stamping.
        self.spi_cycle.store(self.cycles, Ordering::Relaxed);

        // `emulation RunFor` self-advances the (paused) machine by the interval
        // and pauses again; it must NOT be preceded by `start`, or Renode
        // rejects it as "already started". RunFor is the whole lockstep step.
        // Format with enough precision for sub-microsecond chunks.
        let resp = self
            .monitor
            .command_with_timeout(
                &format!("emulation RunFor \"{seconds:.9}\""),
                Duration::from_secs(60),
            )
            .context("RunFor")?;
        if monitor_failed(&resp) {
            bail!("Renode RunFor failed: {resp}");
        }

        self.cycles += (seconds * self.config.frequency_hz as f64).round() as u64;

        // Exchange state after the chunk, matching the simavr backend's timing.
        self.poll_gpio_edges();
        self.pump_uart_out();
        Ok(())
    }

    /// Read the CPU program counter live (separate from the cached `state`).
    pub fn read_pc(&mut self) -> Option<u32> {
        let resp = self
            .monitor
            .command(&format!("{} PC", self.config.cpu))
            .ok()?;
        parse_hex_or_dec(&resp)
    }

    /// Return the path of the Renode log file that captures the execution trace,
    /// or `None` when `HAUKSBEE_RENODE_TRACE` is unset. Callers can read this
    /// file after the run to inspect the raw function-name log entries.
    pub fn trace_log_path(&self) -> Option<&std::path::Path> {
        self.trace_log_path.as_deref()
    }

    /// Read the execution trace log (if enabled) and print the function-name
    /// entries to stderr. Filters Renode's log noise and retains only the
    /// "Entering function" lines that indicate CPU function transitions.
    ///
    /// Called automatically from `Drop` so the trace is always printed even
    /// when the run ends via timeout or assertion failure.
    pub fn dump_trace(&self) {
        let Some(path) = &self.trace_log_path else {
            return;
        };
        match std::fs::read_to_string(path) {
            Ok(contents) => {
                let fn_lines: Vec<&str> = contents
                    .lines()
                    .filter(|l| l.contains("Entering function"))
                    .collect();
                eprintln!(
                    "\n[hauksbee-trace] ── execution trace ({} function entries) ──",
                    fn_lines.len()
                );
                for line in &fn_lines {
                    eprintln!("[hauksbee-trace] {line}");
                }
                eprintln!("[hauksbee-trace] ── full log: {} ──", path.display());
            }
            Err(e) => {
                eprintln!("[hauksbee-trace] could not read trace log {}: {e}", path.display());
            }
        }
    }
}

/// Parse a Renode register value, which prints as `0xHEX` or a decimal.
fn parse_hex_or_dec(s: &str) -> Option<u32> {
    let tok = s.split_whitespace().next()?.trim();
    if let Some(hex) = tok.strip_prefix("0x").or_else(|| tok.strip_prefix("0X")) {
        u32::from_str_radix(hex, 16).ok()
    } else {
        tok.parse::<u32>().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_renode_values() {
        assert_eq!(parse_hex_or_dec("0x0000200C"), Some(0x200C));
        assert_eq!(parse_hex_or_dec("0x200C\r\n"), Some(0x200C));
        assert_eq!(parse_hex_or_dec("8192"), Some(8192));
        assert_eq!(parse_hex_or_dec("garbage"), None);
    }

    #[test]
    fn stm32f103_config_shape() {
        let c = RenodeConfig::stm32f103();
        assert_eq!(c.ports.len(), 7);
        assert!(c
            .ports
            .iter()
            .any(|p| p.letter == 'C' && p.odr_offset == 0x0C));
        assert_eq!(c.uart.as_deref(), Some("sysbus.usart1"));
        // Stock platforms model no ADC → no injection map, loud-drop path.
        assert!(c.adc_channels.is_empty());
        // Direction: every F1 port maps CRL/CRH at offset 0x00 (CRH read at
        // offset + 4 by the encoding), verified against Renode 1.16.1's
        // STM32F1GPIOPort read-back.
        for p in &c.ports {
            assert_eq!(
                p.dir,
                Some(DirMap {
                    offset: 0x00,
                    encoding: DirEncoding::Stm32f1CrlCrh
                }),
                "port {} must carry the F1 CRL/CRH dir map",
                p.letter
            );
        }
    }

    /// The per-family direction decoders, pinned against the reference-manual
    /// encodings the shipped descriptors rely on.
    #[test]
    fn dir_mask_decoding() {
        // MODER (F4/L4/F7): 2 bits/pin, 0b01 = GP output. Pin0 input (00),
        // pin1 output (01), pin2 AF (10, NOT counted), pin3 analog (11, not
        // counted), pin12 output.
        let moder = 0b01 << 2 | 0b10 << 4 | 0b11 << 6 | 0b01 << 24;
        assert_eq!(
            decode_dir_mask(DirEncoding::Moder, moder, 0, 16),
            (1 << 1) | (1 << 12)
        );
        // The blinky-style PA5 output: MODER5 = 0b01.
        assert_eq!(decode_dir_mask(DirEncoding::Moder, 0b01 << 10, 0, 16), 1 << 5);

        // STM32F1 CRL/CRH: 4 bits/pin nibbles; MODE (low 2 bits) != 0 means
        // output, in any CNF. 0x3 = GP push-pull 50 MHz, 0xB = AF push-pull
        // (MODE=11 → output), 0x4 = floating input (MODE=00).
        let crl = 0x3 | (0x4 << 4) | (0xB << 8); // pin0 out, pin1 in, pin2 AF out
        let crh = 0x3 << 20; // pin13 out (the blue-pill LED nibble)
        assert_eq!(
            decode_dir_mask(DirEncoding::Stm32f1CrlCrh, crl, crh, 16),
            (1 << 0) | (1 << 2) | (1 << 13)
        );
        // All-input reset value decodes to no outputs.
        assert_eq!(decode_dir_mask(DirEncoding::Stm32f1CrlCrh, 0, 0, 16), 0);

        // DirBits (nRF52 DIR / RP2040 GPIO_OE): identity, clipped to width.
        assert_eq!(decode_dir_mask(DirEncoding::DirBits, 0x2001, 0, 32), 0x2001);
        assert_eq!(
            decode_dir_mask(DirEncoding::DirBits, 0xFFFF_FFFF, 0, 30),
            0x3FFF_FFFF,
            "bits at/above the bank width are phantom pins and must be cleared"
        );
    }

    /// The shipped descriptors' direction maps and the corrected nRF OUT
    /// offset (peripheral-relative 0x4, NOT the datasheet block-relative
    /// 0x504 that Renode's registration point makes an unhandled read).
    #[test]
    fn dir_map_descriptor_shape() {
        let f4 = RenodeConfig::stm32f4_discovery();
        for p in &f4.ports {
            assert_eq!(
                p.dir,
                Some(DirMap {
                    offset: 0x00,
                    encoding: DirEncoding::Moder
                }),
                "F4 port {} must carry the MODER dir map",
                p.letter
            );
        }

        let nrf = RenodeConfig::nrf52840();
        for p in &nrf.ports {
            assert_eq!(
                p.odr_offset, 0x4,
                "nRF OUT is peripheral-relative 0x4 (gpio0/1 are registered at \
                 the 0x…500 register window, so 0x504 reads as unhandled → 0)"
            );
            assert_eq!(
                p.dir,
                Some(DirMap {
                    offset: 0x14,
                    encoding: DirEncoding::DirBits
                }),
                "nRF port {} must carry the DIR dir map",
                p.letter
            );
        }

        // Unverified platforms carry NO dir map: a wrong one would mask every
        // edge to zero, so absence (conservative, direction unobservable) is
        // the required state until a live Renode verifies the register.
        for c in [RenodeConfig::rp2040(), RenodeConfig::sifive_fe310()] {
            for p in &c.ports {
                assert_eq!(p.dir, None, "unverified platform must not claim a dir map");
            }
        }
    }

    /// `drive_direction_observable` follows dir-map coverage of the polled
    /// ports (via [`dir_covers_ports`], the exact function the backend calls):
    /// full coverage → true, an uncovered active port → false, and the
    /// active-ports hint narrows which ports must be covered.
    #[test]
    fn dir_observability_follows_port_coverage() {
        let f103 = RenodeConfig::stm32f103();
        assert!(dir_covers_ports(&f103, None));
        assert!(dir_covers_ports(&f103, Some(&['A', 'C'])));
        let rp = RenodeConfig::rp2040();
        assert!(!dir_covers_ports(&rp, None));
        assert!(!dir_covers_ports(&rp, Some(&['0'])));
        // A mixed config: coverage decided per polled port.
        let mut mixed = RenodeConfig::stm32f4_discovery();
        mixed.ports[1].dir = None; // strip port B's map
        assert!(!dir_covers_ports(&mixed, None));
        assert!(dir_covers_ports(&mixed, Some(&['A', 'C'])));
        assert!(!dir_covers_ports(&mixed, Some(&['A', 'B'])));
    }

    #[test]
    fn adc_count_conversion() {
        // 12-bit converter, 3.3 V full scale.
        assert_eq!(adc_count(0.0, 3.3, 4095), 0);
        assert_eq!(adc_count(3.3, 3.3, 4095), 4095);
        // Clamped above full scale and below zero.
        assert_eq!(adc_count(5.0, 3.3, 4095), 4095);
        assert_eq!(adc_count(-1.0, 3.3, 4095), 0);
        // 2.0 V of 3.3 V against the 2^n full scale: (2.0/3.3)*4096 = 2482.4 →
        // 2482 (top-code clamp only bites at true full scale).
        assert_eq!(adc_count(2.0, 3.3, 4095), 2482);
        // Near full scale the 2^n scaling reads 4095 where the old (2^n-1)
        // scaling under-read to 4094: 3.2992/3.3 → *4096 = 4095.0 vs *4095 = 4094.
        assert_eq!(adc_count(3.2992, 3.3, 4095), 4095);
        // Broken map (zero full scale) reads stuck-at-zero, not NaN.
        assert_eq!(adc_count(1.0, 0.0, 4095), 0);
    }

    #[test]
    fn adc_inject_rendering() {
        // Peripheral-model path: template substitution.
        let cmd = render_adc_inject(
            &AdcInject::MonitorCommand("sysbus.adc SetDefaultValue {count}".to_string()),
            2482,
            2000,
        );
        assert_eq!(cmd, "sysbus.adc SetDefaultValue 2482");
        let cmd = render_adc_inject(
            &AdcInject::MonitorCommand("sysbus.adc FeedMillivolts {millivolts}".to_string()),
            2482,
            2000,
        );
        assert_eq!(cmd, "sysbus.adc FeedMillivolts 2000");
        // RAM/result-word path.
        let cmd = render_adc_inject(&AdcInject::MemoryWord(0x2000_4000), 0x9B2, 2000);
        assert_eq!(cmd, "sysbus WriteDoubleWord 0x20004000 0x9B2");
    }

    /// Bit-identity proof for the data-driven config bridge (05 §5.5): every
    /// stock config serializes and deserializes back to a value that is EQUAL to
    /// what the constructor produced. This proves two things at once:
    ///   1. the struct is a *lossless* plain-data carrier, no field is dropped
    ///      or altered on the round trip, so W5's future file load reconstructs
    ///      the exact config the constructor makes today (the whole point of the
    ///      bridge);
    ///   2. the config VALUE is unchanged by the refactor; the constructors
    ///      were not touched (only inert `#[derive]`s were added), and the
    ///      `*_config_shape` tests below still pin the individual field values
    ///      they always pinned, so before == after.
    ///
    /// A config with an `AdcChannelMap` is included so the just-merged ADC work
    /// (the `f64` full-scale field) is proven to fold through the round trip too.
    fn assert_roundtrip(config: &RenodeConfig) {
        let json = serde_json::to_string(config).expect("serialize RenodeConfig");
        let back: RenodeConfig = serde_json::from_str(&json).expect("deserialize RenodeConfig");
        assert_eq!(
            *config, back,
            "config must survive a serialize -> deserialize round trip bit-identically"
        );
    }

    #[test]
    fn rp2040_config_shape() {
        let c = RenodeConfig::rp2040();
        assert_eq!(c.machine, "rp2040");
        assert_eq!(c.platform, "@platforms/cpus/rp2040.repl");
        assert_eq!(c.expected_e_machine, crate::elf::EM_ARM);
        assert_eq!(c.frequency_hz, 125_000_000);
        // One 30-pin bank read at SIO GPIO_OUT (offset 0x10 from SIO_BASE), NOT
        // a port ODR; the honest SIO adaptation of the ODR-offset discipline.
        assert_eq!(c.ports.len(), 1);
        assert_eq!(c.ports[0].letter, '0');
        assert_eq!(c.ports[0].peripheral, "sio");
        assert_eq!(c.ports[0].odr_offset, 0x10);
        assert_eq!(c.ports[0].width, 30);
        // Nothing unverified is claimed: no ADC map, no bus controllers.
        assert!(c.adc_channels.is_empty());
        assert!(c.i2c_controllers.is_empty());
        assert!(c.spi_controllers.is_empty());
    }

    /// Drive one I2C bridge read request through the real
    /// [`I2cBridgeState::handle_stream`] path over a local socket pair, with a
    /// stateful slave model (a register pointer that advances one step per
    /// byte served). Returns the response bytes Renode would receive and the
    /// final pointer position (== number of `I2cEvent::Read` callback
    /// invocations).
    fn run_bridge_read(single_read_prefetch: bool, read_count: u32) -> (Vec<u8>, u8) {
        let (mut responses, pointer) = run_bridge_reads(single_read_prefetch, &[read_count]);
        (responses.remove(0), pointer)
    }

    /// Like [`run_bridge_read`] but drives several sequential read requests
    /// (one connection each, exactly as the generated C# connects per call)
    /// through ONE bridge state against ONE stateful slave, so tests can
    /// observe where the slave's register pointer lands between reads.
    fn run_bridge_reads(single_read_prefetch: bool, read_counts: &[u32]) -> (Vec<Vec<u8>>, u8) {
        use std::io::Read as _;
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test listener");
        let port = listener.local_addr().expect("local addr").port();

        // Stateful slave: serves 0x10, 0x11, 0x12, ... and advances its
        // register pointer once per byte read. Start/Stop are acknowledged
        // without advancing anything.
        let pointer = Arc::new(Mutex::new(0u8));
        let slave_pointer = Arc::clone(&pointer);
        let cb: I2cCb = Box::new(move |event| match event {
            I2cEvent::Read { .. } => {
                let mut p = slave_pointer.lock().unwrap();
                let value = 0x10 + *p;
                *p += 1;
                Some(value)
            }
            _ => None,
        });
        let callback = Arc::new(Mutex::new(cb));

        let mut state = I2cBridgeState {
            single_read_prefetch,
            ..I2cBridgeState::default()
        };

        let mut responses = Vec::with_capacity(read_counts.len());
        for &read_count in read_counts {
            let mut client = TcpStream::connect(("127.0.0.1", port)).expect("connect");
            let (mut server, _) = listener.accept().expect("accept");

            // op=READ, addr=0x48, read_count, payload_len=0; the exact wire
            // header the generated C# `Read(count)` now sends (true count).
            let mut request = vec![I2C_OP_READ, 0x48];
            request.extend_from_slice(&read_count.to_be_bytes());
            request.extend_from_slice(&0u32.to_be_bytes());
            client.write_all(&request).expect("write request");

            state
                .handle_stream(&mut server, &callback, false)
                .expect("handle_stream");

            let mut len_bytes = [0u8; 4];
            client.read_exact(&mut len_bytes).expect("response length");
            let mut response = vec![0u8; u32::from_be_bytes(len_bytes) as usize];
            client.read_exact(&mut response).expect("response payload");
            responses.push(response);
        }

        let final_pointer = *pointer.lock().unwrap();
        (responses, final_pointer)
    }

    /// BUG #18 regression: a genuine single-byte read must invoke the slave
    /// callback exactly once (the register pointer advances by 1, not 2) and
    /// return exactly one byte to Renode, no over-fetch, no over-return.
    #[test]
    fn single_byte_read_consumes_and_returns_exactly_one_byte() {
        let (response, pointer) = run_bridge_read(false, 1);
        assert_eq!(response, vec![0x10], "exactly one byte returned to Renode");
        assert_eq!(pointer, 1, "slave callback invoked exactly once");
    }

    /// The gated STM32F1 two-byte-receive prefetch still works where enabled:
    /// a count==1 request fetches and returns two bytes so Renode 1.16.1's
    /// one-shot STM32F4_I2C fifo can satisfy both DR reads. count==1 is the
    /// correct wire shape for this case: the model calls `Read()` (default
    /// count 1) exactly once at the address phase, never N=2, see the
    /// `single_read_prefetch` field docs for the source-level proof.
    #[test]
    fn stm32f1_prefetch_still_fills_two_byte_receive() {
        let (response, pointer) = run_bridge_read(true, 1);
        assert_eq!(response, vec![0x10, 0x11]);
        assert_eq!(pointer, 2);
        // And back-to-back two-byte receives stay aligned: the second
        // transaction serves the next pair, so an LM75-style stream of
        // paired reads never skews.
        let (responses, pointer) = run_bridge_reads(true, &[1, 1]);
        assert_eq!(responses, vec![vec![0x10, 0x11], vec![0x12, 0x13]]);
        assert_eq!(pointer, 4);
    }

    /// On an STM32F1 platform the wire carries no signal separating a
    /// standalone one-byte read from a two-byte receive (both arrive as one
    /// `READ count=1` and nothing else, proof in the `single_read_prefetch`
    /// field docs), so exact single-byte service for stateful,
    /// auto-incrementing slaves is an explicit opt-out, not a heuristic:
    /// with `HAUKSBEE_RENODE_I2C_SINGLE_READ_PREFETCH=0` resolved, a
    /// standalone single-byte read consumes exactly ONE byte from the slave
    /// and the next read returns the NEXT register, not the one after.
    #[test]
    fn stm32f1_single_read_exact_with_prefetch_opt_out() {
        let platform = &RenodeConfig::stm32f103().platform;
        let prefetch = resolve_single_read_prefetch(Some("0"), platform);
        assert!(!prefetch, "override =0 wins over the STM32F1 default");
        // Two sequential single-byte reads against ONE slave: the second must
        // return the NEXT register (0x11), not the one after (0x12), i.e.
        // the first read consumed exactly one byte, no discarded over-fetch.
        let (responses, pointer) = run_bridge_reads(prefetch, &[1, 1]);
        assert_eq!(responses, vec![vec![0x10], vec![0x11]]);
        assert_eq!(pointer, 2, "two reads advanced the slave by exactly two");
    }

    /// The full prefetch policy resolution: platform default, =0/=1
    /// overrides in both directions, unrecognized values falling back.
    #[test]
    fn resolve_single_read_prefetch_covers_default_and_overrides() {
        let f1 = &RenodeConfig::stm32f103().platform;
        let nrf = &RenodeConfig::nrf52840().platform;
        assert!(resolve_single_read_prefetch(None, f1));
        assert!(!resolve_single_read_prefetch(None, nrf));
        assert!(!resolve_single_read_prefetch(Some("0"), f1));
        assert!(resolve_single_read_prefetch(Some("1"), nrf));
        assert!(resolve_single_read_prefetch(Some("yes"), f1));
        assert!(!resolve_single_read_prefetch(Some("yes"), nrf));
    }

    /// Multi-byte reads are untouched by the quirk in either mode.
    #[test]
    fn multi_byte_read_fetches_exactly_read_count() {
        let (response, pointer) = run_bridge_read(true, 3);
        assert_eq!(response, vec![0x10, 0x11, 0x12]);
        assert_eq!(pointer, 3);
    }

    /// The prefetch gate: only STM32F1 platform descriptions enable the
    /// two-byte-receive quirk; the F4-Discovery (collateral wiring) and every
    /// other platform serve single-byte reads exactly.
    #[test]
    fn single_read_prefetch_gated_to_stm32f1_platforms() {
        assert!(platform_needs_i2c_single_read_prefetch(
            &RenodeConfig::stm32f103().platform
        ));
        assert!(!platform_needs_i2c_single_read_prefetch(
            &RenodeConfig::stm32f4_discovery().platform
        ));
        assert!(!platform_needs_i2c_single_read_prefetch(
            &RenodeConfig::nrf52840().platform
        ));
    }

    /// The generated C# no longer makes the over-fetch decision: it forwards
    /// the controller's true `count` on the wire (the Rust host owns the
    /// prefetch policy, which the tests above pin).
    #[test]
    fn generated_bridge_source_requests_true_count() {
        let source = render_i2c_bridge_source(4242, &[0x48]);
        assert!(source.contains("Request(2, new byte[0], count)"));
        assert!(!source.contains("count == 1 ? 2 : count"));
    }

    #[test]
    fn config_bridge_roundtrips_bit_identically() {
        assert_roundtrip(&RenodeConfig::stm32f103());
        assert_roundtrip(&RenodeConfig::stm32f4_discovery());
        assert_roundtrip(&RenodeConfig::nrf52840());
        assert_roundtrip(&RenodeConfig::sifive_fe310());
        assert_roundtrip(&RenodeConfig::rp2040());
        // With the merged ADC work folded in as struct data.
        assert_roundtrip(&RenodeConfig::stm32f103().with_adc_channel(AdcChannelMap {
            channel: 0,
            inject: AdcInject::MemoryWord(0x2000_4000),
            full_scale_volts: 3.3,
            max_count: 4095,
        }));
        assert_roundtrip(&RenodeConfig::stm32f103().with_adc_channel(AdcChannelMap {
            channel: 1,
            inject: AdcInject::MonitorCommand("sysbus.adc FeedMillivolts {millivolts}".to_string()),
            full_scale_volts: 1.8,
            max_count: 1023,
        }));
    }
}
