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
use std::path::Path;
use std::time::Duration;
use uart::UartSocket;

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
}

impl RenodeConfig {
    /// STM32F103C8 ("blue pill"): PA-PG, USART1, 8 MHz HSI.
    ///
    /// ODR is at offset 0x0C, ports are 16-bit. This is the configuration the
    /// galvani STM32 demo board binds to.
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
        }
    }
}

/// Allocate two distinct free TCP ports, holding both listeners until both
/// numbers are read so the OS cannot reissue one to the other. Renode binds
/// each shortly after we release them.
fn free_port_pair() -> Result<(u16, u16)> {
    let a = std::net::TcpListener::bind(("127.0.0.1", 0))
        .context("allocating monitor TCP port")?;
    let b = std::net::TcpListener::bind(("127.0.0.1", 0))
        .context("allocating uart TCP port")?;
    let pa = a.local_addr()?.port();
    let pb = b.local_addr()?.port();
    // Distinct by construction (both listeners are bound simultaneously), but
    // assert to make any future regression loud rather than silent.
    anyhow::ensure!(pa != pb, "port allocator returned a collision");
    Ok((pa, pb))
    // listeners drop here, releasing both ports for Renode to bind.
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
    on_pin_change: Option<Box<dyn FnMut(PinId, bool) + Send>>,
    /// UART byte callback.
    on_uart: Option<Box<dyn FnMut(u8) + Send>>,
    firmware_loaded: bool,
    /// Virtual time advanced so far, in cycles-equivalent (frequency * seconds).
    cycles: u64,
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
        monitor.command(&format!("mach create \"{}\"", config.machine))?;
        let plat = monitor
            .command(&format!("machine LoadPlatformDescription {}", config.platform))?;
        if plat.to_lowercase().contains("error") {
            bail!("Renode failed to load platform {}: {plat}", config.platform);
        }

        // Run at host speed: the analog solver sets the pace, not wall time.
        monitor.command("emulation SetGlobalAdvanceImmediately true")?;

        // Optional UART bridge: a server socket terminal on the pre-allocated
        // port (distinct from the monitor port by construction).
        let mut uart = None;
        if let Some(usart) = &config.uart {
            monitor.command(&format!(
                "emulation CreateServerSocketTerminal {uart_port} \"galvani_uart\" false"
            ))?;
            let conn = monitor
                .command(&format!("connector Connect {usart} galvani_uart"))?;
            if conn.to_lowercase().contains("error") {
                bail!("Renode failed to connect UART {usart}: {conn}");
            }
            uart = Some(UartSocket::connect(uart_port, Duration::from_secs(10))?);
        }

        // Any platform-specific extra setup (e.g. attaching a button).
        for cmd in &config.extra_setup {
            monitor.command(cmd)?;
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
            firmware_loaded: false,
            cycles: 0,
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
            Ok(resp) => parse_hex_or_dec(&resp).unwrap_or_else(|| {
                *self.last_odr.get(&port.letter).unwrap_or(&0)
            }),
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
        for port in &ports {
            let new = self.read_odr(port);
            let prev = *self.last_odr.get(&port.letter).unwrap_or(&0);
            if new != prev {
                let changed = new ^ prev;
                if let Some(cb) = &mut self.on_pin_change {
                    for bit in 0..port.width {
                        if (changed >> bit) & 1 != 0 {
                            let high = (new >> bit) & 1 != 0;
                            cb(PinId {
                                port: port.letter,
                                bit,
                            }, high);
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
            if let Some(cb) = &mut self.on_uart {
                for b in bytes {
                    cb(b);
                }
            }
        }
    }

}

impl Mcu for RenodeBackend {
    fn load_firmware(&mut self, path: &Path) -> Result<()> {
        let p = path
            .to_str()
            .context("non-UTF-8 firmware path")?;
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let resp = match ext.as_str() {
            "elf" | "" => self.monitor.command(&format!("sysbus LoadELF @{p}"))?,
            "hex" => self.monitor.command(&format!("sysbus LoadHEX @{p}"))?,
            "bin" => bail!("raw .bin needs a load address; supply an ELF instead"),
            other => bail!("unsupported firmware extension '.{other}' — use .elf or .hex"),
        };
        if resp.to_lowercase().contains("error")
            || resp.to_lowercase().contains("exception")
        {
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
            if r.to_lowercase().contains("error") {
                bail!("Renode post-load command failed ({cmd}): {r}");
            }
        }
        self.firmware_loaded = true;
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

    fn on_pin_change(&mut self, cb: Box<dyn FnMut(PinId, bool) + Send>) {
        self.on_pin_change = Some(cb);
    }

    fn uart_write(&mut self, bytes: &[u8]) {
        if let Some(u) = &mut self.uart {
            let _ = u.write_bytes(bytes);
        }
    }

    fn on_uart(&mut self, cb: Box<dyn FnMut(u8) + Send>) {
        self.on_uart = Some(cb);
    }

    fn on_i2c(&mut self, _cb: Box<dyn FnMut(I2cEvent) -> Option<u8> + Send>) {
        // I2C peripheral interception is not yet wired for the Renode backend.
    }

    fn on_spi(&mut self, _cb: Box<dyn FnMut(SpiEvent) -> u8 + Send>) {
        // SPI peripheral interception is not yet wired for the Renode backend.
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
        if resp.to_lowercase().contains("error") {
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
        assert!(c.ports.iter().any(|p| p.letter == 'C' && p.odr_offset == 0x0C));
        assert_eq!(c.uart.as_deref(), Some("sysbus.usart1"));
    }
}
