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

mod monitor;
mod process;
mod uart;

pub use process::{find_renode, is_available};

use crate::traits::{I2cEvent, Mcu, McuState, PinId, SpiEvent};
use anyhow::{bail, Context, Result};
use monitor::Monitor;
use process::RenodeProcess;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use uart::UartSocket;

type I2cCb = Box<dyn FnMut(I2cEvent) -> Option<u8> + Send>;
type SpiCb = Box<dyn FnMut(SpiEvent) -> u8 + Send>;

/// How a single GPIO port is addressed inside Renode.
#[derive(Debug, Clone)]
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
}

/// Per-MCU Renode configuration: enough to bring up a machine and wire it.
#[derive(Debug, Clone)]
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
}

impl RenodeConfig {
    /// STM32F103C8 ("blue pill"): PA-PG, USART1, 8 MHz HSI.
    ///
    /// ODR is at offset 0x0C, ports are 16-bit. This is the configuration the
    /// hauksbee STM32 demo board binds to.
    pub fn stm32f103() -> Self {
        let ports = ['A', 'B', 'C', 'D', 'E', 'F', 'G']
            .into_iter()
            .map(|l| PortMap {
                letter: l,
                peripheral: format!("gpioPort{l}"),
                odr_offset: 0x0C,
                width: 16,
            })
            .collect();
        RenodeConfig {
            machine: "f103".to_string(),
            platform: "@platforms/cpus/stm32f103.repl".to_string(),
            cpu: "sysbus.cpu".to_string(),
            uart: Some("sysbus.usart1".to_string()),
            ports,
            frequency_hz: 8_000_000,
            extra_setup: Vec::new(),
            post_load_setup: Vec::new(),
            i2c_controllers: vec!["i2c1".to_string()],
            // SPI1 is not in the stock stm32f103.repl; we add it when an SPI
            // slave is attached (see install_spi_bridge).
            // A single-line definition without the IRQ connection works for
            // polling-mode firmware (the firmware checks SR flags, not NVIC).
            spi_controllers: vec!["spi1".to_string()],
            spi_extra_repl: Some(
                "spi1: SPI.STM32SPI @ sysbus 0x40013000".to_string(),
            ),
            expected_e_machine: crate::elf::EM_ARM,
            mcu_label: "STM32F103 (ARM Cortex-M3)".to_string(),
        }
    }

    /// STM32F4 Discovery (STM32F407): PA-PE, USART2, 16 MHz HSI default.
    /// STM32F4 GPIO ODR is at offset 0x14.
    pub fn stm32f4_discovery() -> Self {
        let ports = ['A', 'B', 'C', 'D', 'E']
            .into_iter()
            .map(|l| PortMap {
                letter: l,
                peripheral: format!("gpioPort{l}"),
                odr_offset: 0x14,
                width: 16,
            })
            .collect();
        RenodeConfig {
            machine: "f4disco".to_string(),
            platform: "@platforms/boards/stm32f4_discovery.repl".to_string(),
            cpu: "sysbus.cpu".to_string(),
            uart: Some("sysbus.usart2".to_string()),
            ports,
            frequency_hz: 16_000_000,
            extra_setup: Vec::new(),
            post_load_setup: Vec::new(),
            i2c_controllers: vec!["i2c1".to_string()],
            // spi1, spi2, and spi3 are already defined in the base stm32f4.repl
            // that stm32f4_discovery.repl includes. Registering them again via
            // spi_extra_repl causes a Renode redefinition/address-conflict error
            // (the Monitor dumps the peripheral's method list instead of accepting
            // the command, then panics on the bridge registration that follows).
            // Fix: set spi_extra_repl = None so install_spi_bridge_for skips the
            // fragment load and goes straight to registering the bridge peripheral
            // on the already-existing spi2/spi3 controllers.
            spi_controllers: vec!["spi2".to_string(), "spi3".to_string()],
            spi_extra_repl: None,
            expected_e_machine: crate::elf::EM_ARM,
            mcu_label: "STM32F407 (ARM Cortex-M4)".to_string(),
        }
    }

    /// nRF52840: two 32-bit GPIO ports (gpio0/gpio1), uart0, 64 MHz.
    /// The nRF GPIO OUT register is at offset 0x504.
    pub fn nrf52840() -> Self {
        let ports = vec![
            PortMap {
                letter: '0',
                peripheral: "gpio0".to_string(),
                odr_offset: 0x504,
                width: 32,
            },
            PortMap {
                letter: '1',
                peripheral: "gpio1".to_string(),
                odr_offset: 0x504,
                width: 32,
            },
        ];
        RenodeConfig {
            machine: "nrf52".to_string(),
            platform: "@platforms/cpus/nrf52840.repl".to_string(),
            cpu: "sysbus.cpu".to_string(),
            uart: Some("sysbus.uart0".to_string()),
            ports,
            frequency_hz: 64_000_000,
            extra_setup: Vec::new(),
            post_load_setup: Vec::new(),
            i2c_controllers: Vec::new(),
            spi_controllers: Vec::new(),
            spi_extra_repl: None,
            expected_e_machine: crate::elf::EM_ARM,
            mcu_label: "nRF52840 (ARM Cortex-M4)".to_string(),
        }
    }

    /// SiFive FE310 (HiFive1) RISC-V: one 32-bit GPIO port, uart0, 16 MHz.
    /// The FE310 GPIO output value register (`port`) is at offset 0x0C.
    pub fn sifive_fe310() -> Self {
        let ports = vec![PortMap {
            letter: '0',
            peripheral: "gpio0".to_string(),
            odr_offset: 0x0C,
            width: 32,
        }];
        RenodeConfig {
            machine: "fe310".to_string(),
            platform: "@platforms/cpus/sifive-fe310.repl".to_string(),
            cpu: "sysbus.cpu".to_string(),
            uart: Some("sysbus.uart0".to_string()),
            ports,
            frequency_hz: 16_000_000,
            extra_setup: Vec::new(),
            // The FE310 Zephyr shell demo's ELF entry does not point at the
            // bring-up code; the upstream Renode resc sets PC to `vinit` and
            // tags the PRCI clock regs so the HFROSC/PLL config reads as ready.
            post_load_setup: vec![
                r#"sysbus Tag <0x10008000 4> "PRCI_HFROSCCFG" 0xFFFFFFFF"#.to_string(),
                r#"sysbus Tag <0x10008008 4> "PRCI_PLLCFG" 0xFFFFFFFF"#.to_string(),
                "{cpu} PC `sysbus GetSymbolAddress \"vinit\"`".to_string(),
            ],
            i2c_controllers: Vec::new(),
            spi_controllers: Vec::new(),
            spi_extra_repl: None,
            expected_e_machine: crate::elf::EM_RISCV,
            mcu_label: "SiFive FE310 (RISC-V)".to_string(),
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
    /// dropped — a broken bridge must be visible, never silently absorbed.
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
    /// after that — a truncated header/payload, an oversized payload, a response
    /// write that does not land, or an undefined op code — returns `Err` so the
    /// server logs it rather than letting a broken bridge look like quiet,
    /// valid bus traffic.
    ///
    /// Note: a `Read` for which the callback returns `None` is NOT a bridge
    /// failure — `None` is the model layer's legitimate "no slave here / NACK",
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
                    let mut response = Vec::with_capacity(read_count);
                    for _ in 0..read_count {
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
/// an op — a truncated MOSI byte, a MISO write that does not land, or an op code
/// the protocol does not define — returns `Err` so the server logs it instead of
/// the firmware silently reading a plausible-but-fake bus byte.
fn handle_spi_stream(stream: &mut TcpStream, callback: &Arc<Mutex<SpiCb>>, trace: bool) -> Result<()> {
    let mut op_buf = [0u8; 1];
    match stream.read_exact(&mut op_buf) {
        Ok(()) => {}
        // Empty connection (no bytes at all): the peer connected and closed
        // without a request. Benign — do not treat as a bridge failure.
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
        Err(e) => return Err(e).context("reading SPI bridge op byte"),
    }
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
                cb(SpiEvent { mosi, deselect: false })
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
            let _ = cb(SpiEvent { mosi: 0, deselect: true });
            Ok(())
        }
        other => bail!("SPI bridge: unknown op code 0x{other:02X}"),
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

    /// Last-read ODR per port letter, for edge synthesis.
    last_odr: HashMap<char, u32>,
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
    /// When `HAUKSBEE_RENODE_TRACE=1`, the path of Renode's log file to which
    /// function-name trace lines are written. `None` when tracing is disabled.
    trace_log_path: Option<PathBuf>,
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
            trace_log_path: None,
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

    /// Poll the relevant ports' ODRs, diff against the snapshot, fire edges.
    ///
    /// If the engine has hinted which ports are wired (`active_ports`), only
    /// those are queried; otherwise every configured port is polled.
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
            let new = self.read_odr(port);
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
        let mut state = I2cBridgeState::default();
        let bridge = BridgeServer::start("I2C", cb, move |stream, callback| {
            state.handle_stream(stream, callback, trace)
        })?;
        let source = render_i2c_bridge_source(bridge.port(), &self.i2c_slave_addresses);
        self.install_bridge_source("i2c", source, bridge.port())?;

        for controller in &self.config.i2c_controllers {
            for &addr in &self.i2c_slave_addresses {
                let class_name = format!("HauksbeeI2CBridge_{addr:02X}");
                let device_name = format!("hauksbee_i2c_{addr:02x}");
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
        let bridge = BridgeServer::start("SPI", cb, move |stream, callback| {
            handle_spi_stream(stream, callback, trace)
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
            // Renode 1.16.1's STM32F4_I2C model asks the I2C slave for one byte
            // during the STM32F1 two-byte receive sequence used by the test
            // firmware. Returning both bytes lets the controller fill the
            // pending two-byte receive while every byte still comes from the
            // host callback.
            var requested = count == 1 ? 2 : count;
            var response = Request(2, new byte[0], requested);
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
            let _ = self.monitor.command(&format!(
                "sysbus.{} OnGPIO {} {}",
                port.peripheral, pin.bit, high
            ));
        }
    }

    fn set_analog_in(&mut self, _channel: u8, _volts: f64) {
        // ADC injection is platform-specific in Renode (the ADC peripheral
        // model and its `FeedSample`/`SetDefaultValue` API vary by SoC). The
        // STM32F103 demo couples through the LED net rather than the ADC, so
        // this is a documented no-op until a per-platform ADC map is added.
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

    fn set_i2c_slave_addresses(&mut self, addresses: &[u8]) {
        self.i2c_slave_addresses = addresses.to_vec();
        self.i2c_slave_addresses.sort_unstable();
        self.i2c_slave_addresses.dedup();
    }
}

impl RenodeBackend {
    /// Advance virtual time by `seconds`, then exchange GPIO/UART state.
    fn run_seconds(&mut self, seconds: f64) -> Result<()> {
        if !self.firmware_loaded {
            bail!("no firmware loaded into the Renode machine");
        }

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
    }
}
