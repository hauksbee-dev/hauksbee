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

use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::traits::{
    ExecutionProbe, I2cCb, Mcu, McuState, PinChangeCb, PinId, SpiCb, UartCb,
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
    AwaitSync,
    /// Synchronised; waiting for a command byte.
    AwaitCommand,
    /// Got a command byte; waiting for its complement.
    AwaitComplement(u8),
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
            phase: Phase::AwaitSync,
        }
    }

    /// Feed one received byte; the bytes to transmit in answer.
    pub fn receive(&mut self, byte: u8) -> Vec<u8> {
        match self.phase {
            Phase::AwaitSync => {
                if byte == SYNC {
                    self.phase = Phase::AwaitCommand;
                    vec![ACK]
                } else {
                    // Not the auto-baud frame: the ROM keeps scanning RX.
                    Vec::new()
                }
            }
            Phase::AwaitCommand => {
                self.phase = Phase::AwaitComplement(byte);
                Vec::new()
            }
            Phase::AwaitComplement(cmd) => {
                self.phase = Phase::AwaitCommand;
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
        self.cycles = self.cycles.saturating_add(u64::try_from(n).unwrap_or(u64::MAX));
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
            commands: vec![0x00, 0x01, 0x02, 0x11, 0x21, 0x31, 0x43, 0x63, 0x73, 0x82, 0x92],
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
        assert_eq!(feed(&mut bl, &[0x02, 0xFD]), vec![ACK, 0x01, 0x04, 0x10, ACK]);
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
        assert_eq!(&out[3..14], &[0x00, 0x01, 0x02, 0x11, 0x21, 0x31, 0x43, 0x63, 0x73, 0x82, 0x92]);
        assert_eq!(out[14], ACK);
        assert_eq!(out.len(), 15);
    }

    #[test]
    fn a_wrong_complement_or_an_unknown_command_is_refused() {
        let mut bl = bootloader();
        bl.receive(SYNC);
        assert_eq!(feed(&mut bl, &[0x02, 0x02]), vec![NACK]);
        assert_eq!(feed(&mut bl, &[0x11, 0xEE]), vec![NACK], "memory commands are refused");
        assert_eq!(feed(&mut bl, &[0x02, 0xFD]), vec![ACK, 0x01, 0x04, 0x10, ACK]);
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
    fn the_strap_argument_is_a_qemu_global() {
        let m = QemuStrapMode {
            device: "esp32.gpio".into(),
            property: "strap_mode".into(),
            value: 0x0f,
        };
        assert_eq!(m.global_arg(), "driver=esp32.gpio,property=strap_mode,value=0xf");
    }
}
