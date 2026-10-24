//! Execution evidence read off an ESP core's UART0 and program counter.
//!
//! The ESP mask ROM prints its own boot log on UART0 ("rst:0x1 (POWERON...)",
//! "entry 0x40080568") before any byte of the flash image runs, so UART bytes
//! alone do not prove the firmware executed: a blank or broken image still
//! makes the ROM talk. The backend therefore samples the program counter at
//! chunk boundaries and counts a sample outside the mask ROM as firmware code
//! running, and reads the console for the ESP-IDF panic handler's lines and
//! for the ROM's reset banner, which reports every reboot after the first.
//!

use std::collections::BTreeMap;

/// The ESP32, ESP32-S3 and ESP32-C3 internal ROM instruction range (each TRM's
/// Internal ROM 0 at `0x4000_0000`, 384 KB). Code anywhere else came from the
/// flash image: the 2nd-stage bootloader in IRAM or the app.
const MASK_ROM: std::ops::Range<u32> = 0x4000_0000..0x4006_0000;

/// What the console and PC samples showed of the firmware running.
#[derive(Debug, Default)]
pub struct BootConsole {
    line: Vec<u8>,
    /// ROM reset banners seen; the first is the power-on boot.
    boots: u32,
    last_reset: String,
    /// Fault kind to count, in a stable order.
    faults: BTreeMap<String, u32>,
    /// PC samples that landed outside the mask ROM.
    pub firmware_pcs: u64,
}

impl BootConsole {
    /// Feed UART0 bytes as they arrive.
    pub fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            if b == b'\n' {
                let line = String::from_utf8_lossy(&self.line).into_owned();
                self.line.clear();
                self.scan(line.trim_end());
            } else if self.line.len() < 512 {
                self.line.push(b);
            }
        }
    }

    /// Record one sampled program counter.
    pub fn sample_pc(&mut self, pc: u32) {
        if !MASK_ROM.contains(&pc) {
            self.firmware_pcs += 1;
        }
    }

    fn scan(&mut self, line: &str) {
        if let Some(rest) = line.strip_prefix("rst:") {
            self.boots += 1;
            // "0xc (SW_CPU_RESET),boot:0x12 ..." -> "SW_CPU_RESET"
            self.last_reset = rest
                .split(['(', ')'])
                .nth(1)
                .unwrap_or(rest)
                .trim()
                .to_string();
            return;
        }
        let panic = line
            .split_once("panic'ed (")
            .map(|(_, what)| format!("panic: {}", what.trim_end_matches([')', '.'])));
        // "assert failed: btdm_low_power_mode_init bt.c:1757 (cond)" keeps the
        // function and file:line, drops the expression.
        let assert = || {
            line.find("assert failed: ").map(|at| {
                let what: Vec<&str> = line[at..].split_whitespace().take(4).collect();
                what.join(" ")
            })
        };
        let abort = || {
            line.contains("abort() was called")
                .then(|| "abort() called".to_string())
        };
        let fault = panic.or_else(assert).or_else(abort);
        if let Some(fault) = fault {
            *self.faults.entry(fault).or_default() += 1;
        }
    }

    /// One entry per fault kind, plus the reboots the ROM reported after the
    /// power-on boot (a deep-sleep wake is not a fault).
    #[must_use]
    pub fn faults(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .faults
            .iter()
            .map(|(what, n)| format!("{what} x{n}"))
            .collect();
        if self.boots > 1 && !self.last_reset.contains("DEEPSLEEP") {
            out.push(format!(
                "core rebooted x{} (last reset cause {})",
                self.boots - 1,
                self.last_reset
            ));
        }
        out
    }
}

/// The program counter in a QEMU `info registers` dump: Xtensa prints
/// `PC=4209660a`, RISC-V prints ` pc       42000c3e`.
#[must_use]
pub fn parse_pc(dump: &str) -> Option<u32> {
    dump.lines().find_map(|l| {
        let l = l.trim();
        let hex = l
            .strip_prefix("PC=")
            .or_else(|| l.strip_prefix("pc").map(str::trim_start))?;
        u32::from_str_radix(hex.split_whitespace().next()?.trim_start_matches("0x"), 16).ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panics_asserts_and_reboots_become_faults() {
        let mut c = BootConsole::default();
        // Lines from the 2026-10-01 sweep (smartknob, openrz67, HUZZAH32).
        c.feed(b"rst:0x1 (POWERON_RESET),boot:0x12 (SPI_FAST_FLASH_BOOT)\nentry 0x40080644\n");
        assert!(c.faults().is_empty());
        c.feed(b"Guru Meditation Error: Core  0 panic'ed (Coprocessor exception)\r\n");
        c.feed(b"assert failed: btdm_low_power_mode_init bt.c:1757 (select_src_ret)\n");
        c.feed(b"rst:0xc (SW_CPU_RESET),boot:0x12 (SPI_FAST_FLASH_BOOT)\n");
        c.feed(b"Guru Meditation Error: Core  0 panic'ed (Coprocessor exception)\n");
        assert_eq!(
            c.faults(),
            [
                "assert failed: btdm_low_power_mode_init bt.c:1757 x1",
                "panic: Coprocessor exception x2",
                "core rebooted x1 (last reset cause SW_CPU_RESET)",
            ]
        );
        let mut sleep = BootConsole::default();
        sleep.feed(b"rst:0x1 (POWERON)\nrst:0x5 (DEEPSLEEP_RESET),boot:0x8\n");
        assert!(sleep.faults().is_empty());
    }

    #[test]
    fn only_pcs_outside_the_mask_rom_count_as_firmware() {
        let mut c = BootConsole::default();
        for pc in [0x4000_0400, 0x4005_ffff, 0x4008_0644, 0x4209_660a] {
            c.sample_pc(pc);
        }
        assert_eq!(c.firmware_pcs, 2);
        assert_eq!(parse_pc("CPU#0\nPC=4209660a\n  SR=0"), Some(0x4209_660a));
        assert_eq!(
            parse_pc(" x0/zero 0\n pc       42000c3e\n"),
            Some(0x4200_0c3e)
        );
        assert_eq!(parse_pc("no registers"), None);
    }
}
