//! Boot-mode backends: a core that stands in for a chip's ROM bootloader, and
//! the strap value handed to Espressif's QEMU.
//!
//! A chip whose boot straps select its ROM bootloader does not run the user
//! firmware, whatever image is on the board. [`RomBootMcu`] is the [`Mcu`] the
//! co-simulator installs in that case: it never executes the firmware, and for
//! ST parts it answers the serial protocol of the system-memory bootloader the
//! way the ROM does, so a host tool that probes the board sees a bootloader and
//! not a silent chip.
//!
//! # The ST USART bootloader stand-in
//!
//! ST's bootloader is mask ROM and cannot be vendored, so this is a behavioural
//! stub written from the protocol document (AN3155 rev 18, "USART protocol used
//! in the STM32 bootloader"):
//!
//! * it waits for the auto-baud byte `0x7F` (a frame of `0x7F`, even parity,
//!   one stop bit) and answers `ACK` (`0x79`);
//! * every command is the command byte followed by its complement; a bad
//!   complement or an unknown command is answered `NACK` (`0x1F`);
//! * `Get` (`0x00`) returns `ACK`, `N` (number of bytes that follow, minus
//!   one), the protocol version, the supported command codes, `ACK`;
//!   `Get Version` (`0x01`) returns `ACK`, the version, two zero option bytes,
//!   `ACK`; `Get ID` (`0x02`) returns `ACK`, `N = 1`, the two product-ID bytes
//!   (MSB first), `ACK`.
//!
//! The memory commands (read, write, erase, go, protection) are answered
//! `NACK`, which is also what a read-protected device does for everything
//! outside the identification commands: the stand-in identifies the chip and
//! its bootloader, and does not emulate flashing.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::traits::{
    ExecutionProbe, I2cCb, InputResponderBatchCb, InputResponderCb, InputResponderDirectionCb, Mcu,
    McuState, PinChangeCb, PinId, SpiCb, UartCb,
};

/// The bootloader's positive acknowledge.
pub const ACK: u8 = 0x79;
/// The bootloader's negative acknowledge.
pub const NACK: u8 = 0x1F;
/// The auto-baud byte the host sends first.
pub const SYNC: u8 = 0x7F;

/// What the ST bootloader reports about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StBootloaderInfo {
    /// Protocol version byte (`0x31` is v3.1).
    pub version: u8,
    /// Product ID returned by Get ID.
    pub product_id: u16,
    /// Command codes the Get command lists.
    pub commands: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Waiting for `0x7F`.
    Sync,
    /// Synchronised; waiting for a command byte.
    Command,
    /// Got a command byte; waiting for its complement.
    Complement(u8),
}

/// The ST system-memory bootloader's serial side.
#[derive(Debug, Clone)]
pub struct StUsartBootloader {
    info: StBootloaderInfo,
    phase: Phase,
}

impl StUsartBootloader {
    #[must_use]
    pub const fn new(info: StBootloaderInfo) -> Self {
        Self {
            info,
            phase: Phase::Sync,
        }
    }

    /// Feed one received byte; the bytes to transmit in answer.
    pub fn receive(&mut self, byte: u8) -> Vec<u8> {
        match self.phase {
            Phase::Sync => {
                if byte == SYNC {
                    self.phase = Phase::Command;
                    vec![ACK]
                } else {
                    // Not the auto-baud frame: the ROM keeps scanning RX.
                    Vec::new()
                }
            }
            Phase::Command => {
                self.phase = Phase::Complement(byte);
                Vec::new()
            }
            Phase::Complement(cmd) => {
                self.phase = Phase::Command;
                if byte != !cmd {
                    return vec![NACK];
                }
                self.command(cmd)
            }
        }
    }

    fn command(&self, cmd: u8) -> Vec<u8> {
        match cmd {
            0x00 => {
                let mut out = vec![ACK];
                // N = bytes that follow, minus one: the version and the codes.
                out.push(u8::try_from(self.info.commands.len()).unwrap_or(u8::MAX));
                out.push(self.info.version);
                out.extend(&self.info.commands);
                out.push(ACK);
                out
            }
            0x01 => vec![ACK, self.info.version, 0x00, 0x00, ACK],
            0x02 => {
                let [msb, lsb] = self.info.product_id.to_be_bytes();
                vec![ACK, 0x01, msb, lsb, ACK]
            }
            _ => vec![NACK],
        }
    }
}

/// What the stand-in answers on its serial port.
#[derive(Debug, Clone)]
pub enum RomPersonality {
    /// ST's USART bootloader.
    StUsart(StUsartBootloader),
    /// A boot with no serial protocol (a USB-only ROM): the chip is silent on
    /// the UART and the user firmware does not run.
    Silent,
}

/// A core that is the chip's ROM bootloader instead of the user firmware.
pub struct RomBootMcu {
    personality: RomPersonality,
    frequency_hz: u64,
    cycles: u64,
    uart: Option<UartCb>,
}

impl RomBootMcu {
    #[must_use]
    pub fn new(personality: RomPersonality, frequency_hz: u64) -> Self {
        Self {
            personality,
            frequency_hz,
            cycles: 0,
            uart: None,
        }
    }

    fn transmit(&mut self, bytes: Vec<u8>) {
        if let Some(cb) = self.uart.as_mut() {
            for b in bytes {
                cb(b);
            }
        }
    }
}

impl Mcu for RomBootMcu {
    /// The user firmware is not loaded: the ROM bootloader is what runs.
    fn load_firmware(&mut self, _path: &Path) -> Result<()> {
        Ok(())
    }

    fn run_cycles(&mut self, n: u64) -> Result<u64> {
        self.cycles = self.cycles.saturating_add(n);
        Ok(n)
    }

    fn run_micros(&mut self, us: u64) -> Result<()> {
        let n = u128::from(us) * u128::from(self.frequency_hz) / 1_000_000;
        self.cycles = self
            .cycles
            .saturating_add(u64::try_from(n).unwrap_or(u64::MAX));
        Ok(())
    }

    fn frequency(&self) -> u64 {
        self.frequency_hz
    }

    fn set_digital_in(&mut self, _pin: PinId, _high: bool) {}

    fn set_analog_in(&mut self, _channel: u8, _volts: f64) {}

    fn on_pin_change(&mut self, _cb: PinChangeCb) {}

    fn uart_write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            let answer = match &mut self.personality {
                RomPersonality::StUsart(bl) => bl.receive(b),
                RomPersonality::Silent => Vec::new(),
            };
            self.transmit(answer);
        }
    }

    fn on_uart(&mut self, cb: UartCb) {
        self.uart = Some(cb);
    }

    fn on_i2c(&mut self, _cb: I2cCb) {}

    fn on_spi(&mut self, _cb: SpiCb) {}

    fn state(&self) -> McuState {
        McuState {
            pc: 0,
            cycles: self.cycles,
            sleeping: true,
            done: false,
            crashed: false,
        }
    }

    fn execution_probe(&mut self) -> ExecutionProbe {
        ExecutionProbe {
            reached_entry: Some(false),
            entry_symbol: Some("main".to_string()),
            // The bootloader's serial answers are not the firmware talking.
            boot_rom_console: true,
            ..ExecutionProbe::default()
        }
    }
}

/// What a [`DeferredMcu`] does once the core is first needed.
type Replay = Box<dyn FnOnce(&mut (dyn Mcu + Send)) + Send>;
/// Builds the real core, loading its firmware.
pub type CoreFactory = Box<dyn FnOnce() -> Result<Box<dyn Mcu + Send>> + Send>;

/// A core that does not exist until the chip first has to run.
///
/// A chip held in reset runs nothing, so it needs no emulator; but reset can
/// release later in the run, and then the user firmware must run. This core
/// stands in until the first run request for a non-zero time, then builds the
/// real one and replays everything the board attached in the meantime (the
/// pin, serial, bus and responder hooks, the last input levels). Until then it
/// is the silent stand-in: asleep, not at the entry point, no console.
///
/// If building the real core fails, the failure is returned from the run that
/// asked for it and from every later one, so the cause stays visible.
pub struct DeferredMcu {
    /// The real core, once built.
    live: Option<Box<dyn Mcu + Send>>,
    /// Waiting for the first run: the factory and the attachments to replay.
    factory: Option<CoreFactory>,
    replay: Vec<Replay>,
    firmware: Option<PathBuf>,
    digital_in: HashMap<PinId, bool>,
    analog_in: HashMap<u8, f64>,
    failure: Option<String>,
    idle: RomBootMcu,
}

impl DeferredMcu {
    /// `idle_hz` is the clock reported while the real core does not exist.
    #[must_use]
    pub fn new(factory: CoreFactory, idle_hz: u64) -> Self {
        Self {
            live: None,
            factory: Some(factory),
            replay: Vec::new(),
            firmware: None,
            digital_in: HashMap::new(),
            analog_in: HashMap::new(),
            failure: None,
            idle: RomBootMcu::new(RomPersonality::Silent, idle_hz),
        }
    }

    /// Whether the real core has been built.
    #[must_use]
    pub fn is_started(&self) -> bool {
        self.live.is_some()
    }

    fn core(&self) -> &dyn Mcu {
        match self.live.as_deref() {
            Some(c) => c,
            None => &self.idle,
        }
    }

    fn core_mut(&mut self) -> &mut dyn Mcu {
        match self.live.as_deref_mut() {
            Some(c) => c,
            None => &mut self.idle,
        }
    }

    /// Build the real core and bring it to the state the board left the stand-in in.
    fn start(&mut self) -> Result<()> {
        if self.live.is_some() {
            return Ok(());
        }
        if let Some(why) = &self.failure {
            anyhow::bail!("{why}");
        }
        let Some(factory) = self.factory.take() else {
            anyhow::bail!("the core was already consumed");
        };
        let mut core = match factory() {
            Ok(core) => core,
            Err(e) => {
                self.failure = Some(format!("{e:#}"));
                return Err(e);
            }
        };
        if let Some(path) = self.firmware.take() {
            core.load_firmware(&path)?;
        }
        for attach in self.replay.drain(..) {
            attach(core.as_mut());
        }
        for (pin, high) in self.digital_in.drain() {
            core.set_digital_in(pin, high);
        }
        for (channel, volts) in self.analog_in.drain() {
            core.set_analog_in(channel, volts);
        }
        self.live = Some(core);
        Ok(())
    }

    /// Forward an attachment now, or keep it for when the core starts.
    fn attach(&mut self, f: impl FnOnce(&mut (dyn Mcu + Send)) + Send + 'static) {
        if let Some(core) = self.live.as_deref_mut() {
            f(core);
        } else {
            self.replay.push(Box::new(f));
        }
    }
}

impl Mcu for DeferredMcu {
    fn load_firmware(&mut self, path: &Path) -> Result<()> {
        if let Some(core) = self.live.as_deref_mut() {
            return core.load_firmware(path);
        }
        self.firmware = Some(path.to_path_buf());
        Ok(())
    }

    fn run_cycles(&mut self, n: u64) -> Result<u64> {
        if self.live.is_none() && n == 0 {
            return Ok(0);
        }
        self.start()?;
        self.core_mut().run_cycles(n)
    }

    fn run_micros(&mut self, us: u64) -> Result<()> {
        if self.live.is_none() && us == 0 {
            return Ok(());
        }
        self.start()?;
        self.core_mut().run_micros(us)
    }

    fn run_millis(&mut self, ms: u64) -> Result<()> {
        if self.live.is_none() && ms == 0 {
            return Ok(());
        }
        self.start()?;
        self.core_mut().run_millis(ms)
    }

    fn supports_reset(&self) -> bool {
        self.live.is_none() || self.core().supports_reset()
    }

    fn reset(&mut self) -> Result<()> {
        // Nothing has run, so there is nothing to put back.
        if let Some(core) = self.live.as_deref_mut() {
            return core.reset();
        }
        Ok(())
    }

    fn frequency(&self) -> u64 {
        self.core().frequency()
    }

    fn set_digital_in(&mut self, pin: PinId, high: bool) {
        if let Some(core) = self.live.as_deref_mut() {
            core.set_digital_in(pin, high);
        } else {
            self.digital_in.insert(pin, high);
        }
    }

    fn set_analog_in(&mut self, channel: u8, volts: f64) {
        if let Some(core) = self.live.as_deref_mut() {
            core.set_analog_in(channel, volts);
        } else {
            self.analog_in.insert(channel, volts);
        }
    }

    fn on_pin_change(&mut self, cb: PinChangeCb) {
        self.attach(move |core| core.on_pin_change(cb));
    }

    fn current_cycle(&self) -> u64 {
        self.core().current_cycle()
    }

    fn cycle_exact(&self) -> bool {
        self.core().cycle_exact()
    }

    fn on_input_responder(&mut self, responder: InputResponderCb) {
        self.attach(move |core| core.on_input_responder(responder));
    }

    fn input_responder_synchronous(&self) -> bool {
        self.core().input_responder_synchronous()
    }

    fn input_responder_batches_atomic(&self) -> bool {
        self.core().input_responder_batches_atomic()
    }

    fn input_responder_tracks_direction(&self) -> bool {
        self.core().input_responder_tracks_direction()
    }

    fn on_input_responder_batch(&mut self, responder: InputResponderBatchCb) {
        self.attach(move |core| core.on_input_responder_batch(responder));
    }

    fn on_input_responder_direction(&mut self, responder: InputResponderDirectionCb) {
        self.attach(move |core| core.on_input_responder_direction(responder));
    }

    /// A chip that has not started receives nothing: serial bytes sent while it
    /// is held in reset are lost, as on the bench.
    fn uart_write(&mut self, bytes: &[u8]) {
        self.core_mut().uart_write(bytes);
    }

    fn uart_rx_overflow(&self) -> u64 {
        self.core().uart_rx_overflow()
    }

    fn uart_rx_pending(&self) -> usize {
        self.core().uart_rx_pending()
    }

    fn on_uart(&mut self, cb: UartCb) {
        self.attach(move |core| core.on_uart(cb));
    }

    fn on_i2c(&mut self, cb: I2cCb) {
        self.attach(move |core| core.on_i2c(cb));
    }

    fn on_spi(&mut self, cb: SpiCb) {
        self.attach(move |core| core.on_spi(cb));
    }

    fn on_spi_controller(&mut self, controller: &str, cb: SpiCb) {
        let controller = controller.to_string();
        self.attach(move |core| core.on_spi_controller(&controller, cb));
    }

    fn state(&self) -> McuState {
        self.core().state()
    }

    fn set_active_ports(&mut self, ports: &[char]) {
        let ports = ports.to_vec();
        self.attach(move |core| core.set_active_ports(&ports));
    }

    fn pins_configured_output(&self) -> Vec<PinId> {
        self.core().pins_configured_output()
    }

    fn read_register(&mut self, addr: u32) -> Option<u32> {
        self.core_mut().read_register(addr)
    }

    fn drive_direction_observable(&self) -> bool {
        self.core().drive_direction_observable()
    }

    fn pins_routed_to_peripheral(&self) -> Vec<PinId> {
        self.core().pins_routed_to_peripheral()
    }

    fn peripheral_routing_unobserved(&self, pin: PinId) -> Option<String> {
        self.core().peripheral_routing_unobserved(pin)
    }

    fn pin_can_drive(&self, pin: PinId) -> bool {
        self.core().pin_can_drive(pin)
    }

    fn adc_dropped_channels(&self) -> Vec<u8> {
        self.core().adc_dropped_channels()
    }

    fn watchdog_limitation(&self) -> Option<String> {
        self.core().watchdog_limitation()
    }

    fn timing_limitation(&self) -> Option<String> {
        self.core().timing_limitation()
    }

    fn peripheral_config(&mut self) -> Option<crate::periph_snapshot::PeripheralConfig> {
        self.core_mut().peripheral_config()
    }

    fn clock_assumption(&self) -> Option<String> {
        self.core().clock_assumption()
    }

    fn external_clock_modelled(&self) -> bool {
        self.core().external_clock_modelled()
    }

    fn clock_host_paced(&self) -> bool {
        self.core().clock_host_paced()
    }

    fn watchdog_resets(&self) -> u64 {
        self.core().watchdog_resets()
    }

    fn execution_probe(&mut self) -> ExecutionProbe {
        self.core_mut().execution_probe()
    }

    fn i2c_bus_modeled(&self) -> bool {
        self.core().i2c_bus_modeled()
    }

    fn spi_bus_modeled(&self, controller: Option<&str>) -> bool {
        self.core().spi_bus_modeled(controller)
    }

    fn set_i2c_slave_addresses(&mut self, addresses: &[u8]) {
        let addresses = addresses.to_vec();
        self.attach(move |core| core.set_i2c_slave_addresses(&addresses));
    }

    fn set_i2c_device_temperature(&mut self, addr: u8, milli_c: i32) {
        self.attach(move |core| core.set_i2c_device_temperature(addr, milli_c));
    }
}

/// The strap value for one QEMU GPIO device, passed at spawn as
/// `-global driver=<device>,property=<property>,value=<value>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QemuStrapMode {
    /// The GPIO device (`esp32.gpio`).
    pub device: String,
    /// The property (`strap_mode`).
    pub property: String,
    /// The register value the machine latches as its strapping pins.
    pub value: u32,
}

impl QemuStrapMode {
    /// The QEMU command-line argument that sets it.
    #[must_use]
    pub fn global_arg(&self) -> String {
        format!(
            "driver={},property={},value=0x{:x}",
            self.device, self.property, self.value
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn bootloader() -> StUsartBootloader {
        StUsartBootloader::new(StBootloaderInfo {
            version: 0x22,
            product_id: 0x0410,
            commands: vec![
                0x00, 0x01, 0x02, 0x11, 0x21, 0x31, 0x43, 0x63, 0x73, 0x82, 0x92,
            ],
        })
    }

    fn feed(bl: &mut StUsartBootloader, bytes: &[u8]) -> Vec<u8> {
        bytes.iter().flat_map(|&b| bl.receive(b)).collect()
    }

    #[test]
    fn the_autobaud_byte_is_acknowledged() {
        let mut bl = bootloader();
        assert_eq!(bl.receive(SYNC), vec![ACK]);
    }

    #[test]
    fn bytes_before_the_autobaud_byte_are_ignored() {
        let mut bl = bootloader();
        assert!(feed(&mut bl, &[0x00, 0xFF, 0x55]).is_empty());
        assert_eq!(bl.receive(SYNC), vec![ACK]);
    }

    #[test]
    fn get_id_returns_the_product_id_msb_first() {
        let mut bl = bootloader();
        bl.receive(SYNC);
        assert_eq!(
            feed(&mut bl, &[0x02, 0xFD]),
            vec![ACK, 0x01, 0x04, 0x10, ACK]
        );
    }

    #[test]
    fn get_version_returns_the_version_and_two_zero_option_bytes() {
        let mut bl = bootloader();
        bl.receive(SYNC);
        assert_eq!(feed(&mut bl, &[0x01, 0xFE]), vec![ACK, 0x22, 0, 0, ACK]);
    }

    #[test]
    fn get_lists_the_version_and_the_commands() {
        let mut bl = bootloader();
        bl.receive(SYNC);
        let out = feed(&mut bl, &[0x00, 0xFF]);
        assert_eq!(out[0], ACK);
        assert_eq!(out[1], 11, "N = the number of commands (version excluded)");
        assert_eq!(out[2], 0x22);
        assert_eq!(
            &out[3..14],
            &[0x00, 0x01, 0x02, 0x11, 0x21, 0x31, 0x43, 0x63, 0x73, 0x82, 0x92]
        );
        assert_eq!(out[14], ACK);
        assert_eq!(out.len(), 15);
    }

    #[test]
    fn a_wrong_complement_or_an_unknown_command_is_refused() {
        let mut bl = bootloader();
        bl.receive(SYNC);
        assert_eq!(feed(&mut bl, &[0x02, 0x02]), vec![NACK]);
        assert_eq!(
            feed(&mut bl, &[0x11, 0xEE]),
            vec![NACK],
            "memory commands are refused"
        );
        assert_eq!(
            feed(&mut bl, &[0x02, 0xFD]),
            vec![ACK, 0x01, 0x04, 0x10, ACK]
        );
    }

    #[test]
    fn the_core_answers_through_the_uart_hook_and_never_runs_the_firmware() {
        let mut core = RomBootMcu::new(RomPersonality::StUsart(bootloader()), 8_000_000);
        let got = Arc::new(Mutex::new(Vec::new()));
        let sink = got.clone();
        core.on_uart(Box::new(move |b| sink.lock().unwrap().push(b)));
        core.uart_write(&[SYNC, 0x02, 0xFD]);
        assert_eq!(*got.lock().unwrap(), vec![ACK, ACK, 0x01, 0x04, 0x10, ACK]);
        core.run_micros(1000).unwrap();
        assert_eq!(core.state().cycles, 8000);
        let probe = core.execution_probe();
        assert_eq!(probe.reached_entry, Some(false));
        assert!(probe.boot_rom_console);
    }

    #[test]
    fn a_deferred_core_is_built_only_when_it_first_has_to_run() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let built = Arc::new(AtomicBool::new(false));
        let flag = built.clone();
        let mut core = DeferredMcu::new(
            Box::new(move || {
                flag.store(true, Ordering::SeqCst);
                Ok(Box::new(RomBootMcu::new(
                    RomPersonality::StUsart(bootloader()),
                    8_000_000,
                )) as Box<dyn Mcu + Send>)
            }),
            1_000_000,
        );
        let got = Arc::new(Mutex::new(Vec::new()));
        let sink = got.clone();
        core.on_uart(Box::new(move |b| sink.lock().unwrap().push(b)));
        core.set_analog_in(0, 1.5);
        // Held: zero-length chunks, serial sent to a chip that is not running.
        core.run_micros(0).unwrap();
        assert_eq!(core.run_cycles(0).unwrap(), 0);
        core.uart_write(&[SYNC]);
        assert!(!built.load(Ordering::SeqCst), "nothing runs, nothing built");
        assert!(!core.is_started());
        assert_eq!(core.frequency(), 1_000_000);
        assert_eq!(core.state().cycles, 0);
        assert!(core.supports_reset() && core.reset().is_ok());
        assert!(got.lock().unwrap().is_empty());
        // Released: the real core appears with the hook attached before.
        core.run_micros(1000).unwrap();
        assert!(built.load(Ordering::SeqCst));
        assert!(core.is_started());
        assert_eq!(core.frequency(), 8_000_000);
        core.uart_write(&[SYNC]);
        assert_eq!(*got.lock().unwrap(), vec![ACK]);
    }

    #[test]
    fn a_deferred_core_that_cannot_be_built_says_so_every_time() {
        let mut core = DeferredMcu::new(Box::new(|| anyhow::bail!("no emulator here")), 1_000_000);
        core.run_micros(0).unwrap();
        for _ in 0..2 {
            let err = core.run_micros(10).expect_err("the core cannot start");
            assert!(format!("{err:#}").contains("no emulator here"), "{err:#}");
        }
    }

    #[test]
    fn the_strap_argument_is_a_qemu_global() {
        let m = QemuStrapMode {
            device: "esp32.gpio".into(),
            property: "strap_mode".into(),
            value: 0x0f,
        };
        assert_eq!(
            m.global_arg(),
            "driver=esp32.gpio,property=strap_mode,value=0xf"
        );
    }
}
