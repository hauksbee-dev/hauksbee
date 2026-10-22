//! Rebooting a Renode core, and counting the reboots the firmware did not ask
//! the host for.
//!
//! Long-form how-and-why: docs/how-and-why/hauksbee-mcu/renode.md.
//!
//! # Why a macro
//!
//! Renode's watchdogs (`NRF52840_Watchdog`, `STM32_IndependentWatchdog`) and
//! the host's own `machine Reset` all end in `Machine.Reset()`, which resets
//! every peripheral and the CPU and then runs the Monitor macro named `reset`
//! if one exists. With none registered Renode logs "No action for reset" and
//! leaves the Cortex-M with `VTOR = 0` and `PC = SP = 0`: on a part whose flash
//! is not at 0 the core halts ("PC does not lay in memory"). That is how the
//! STM32F103 independent watchdog "fired once and went quiet": the first bite
//! reset the machine and nothing started the core again. The nRF52840's flash
//! sits at 0, so the same reset happened to land on a valid vector table and
//! its watchdog already rebooted the core; its descriptor's claim that it
//! "never fires" was wrong.
//!
//! The macro reloads the image, which makes Renode's loader set the vector
//! table offset, `PC` and `SP` from the firmware again, and replays the
//! descriptor's post-load commands. RAM keeps whatever the firmware left in it
//! except where the image itself has loadable RAM sections, the same as a
//! silicon watchdog reset leaves SRAM alone.
//!
//! # Counting
//!
//! A persistent counting hook sits on the firmware's reset entry. Every entry
//! is a boot: the first is the power-on boot, the host's own resets are
//! counted as they are asked for, and every other entry is a reboot the
//! firmware earned (a starved watchdog, or any reset source Renode models).
//!
//! Known limit: a software reset the firmware requests itself
//! (`NVIC_SystemReset`, `AIRCR.SYSRESETREQ`) is indistinguishable from a
//! watchdog bite here and is counted with them. A Renode watchpoint on `AIRCR`
//! installs without error but never fires for core accesses (measured on the
//! STM32F103: 129 reboots, 0 watchpoint hits), so the two cannot be told apart.

use super::{parse_u64, RenodeBackend};
use anyhow::{bail, Context, Result};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Distinct file names for the generated macro scripts of one process.
static SEQ: AtomicU64 = AtomicU64::new(0);

/// The CPU-name placeholder descriptor commands carry.
const CPU_PLACEHOLDER: &str = "{cpu}";

/// Names a firmware's reset entry goes by, strongest first.
const RESET_ENTRY_SYMBOLS: [&str; 5] = [
    "Reset_Handler",
    "ResetISR",
    "_reset_handler",
    "reset_handler",
    "_start",
];

/// Python counter of reset-entry executions, shared through `__builtin__`
/// between the CPU hook and the Monitor's `python` command.
const BOOT_KEY: &str = "hkb_boots";

/// Reboot bookkeeping for one Renode machine.
#[derive(Debug, Default)]
pub(super) struct RebootWatch {
    /// Whether the machine's `reset` macro is registered (the core can be
    /// rebooted, by the host or by a Renode peripheral).
    macro_ready: bool,
    /// Whether the reset-entry counter is installed.
    counting: bool,
    /// Reset-entry executions seen at the last read.
    boots: u64,
    /// Resets the host asked for through [`RenodeBackend::reset_core`].
    host_resets: u64,
}

impl RebootWatch {
    /// Reboots nobody asked for: reset-entry executions beyond the power-on
    /// boot, less the host's own resets.
    pub(super) const fn unrequested(&self) -> u64 {
        self.boots
            .saturating_sub(1)
            .saturating_sub(self.host_resets)
    }

    /// Whether this backend can say how many reboots happened.
    pub(super) const fn counting(&self) -> bool {
        self.counting
    }

    /// Whether the core can be rebooted.
    pub(super) const fn can_reset(&self) -> bool {
        self.macro_ready
    }
}

impl RenodeBackend {
    /// Register the `reset` macro and the reset-entry counter, once the
    /// firmware is loaded. Failure leaves the watch inactive: the backend then
    /// reports it cannot reset, and the watchdog limitation text stays honest.
    pub(super) fn install_reset_support(&mut self, firmware: &Path) {
        if let Err(error) = self.register_reset_macro(firmware) {
            eprintln!("renode: reset macro not installed: {error:#}");
            return;
        }
        self.reboots.macro_ready = true;
        if let Some(entry) = self.reset_entry(firmware) {
            let cpu = self.config.cpu.clone();
            let hook = format!(
                "{cpu} AddHook 0x{entry:X} \"import __builtin__; \
                 __builtin__.{BOOT_KEY} = getattr(__builtin__, '{BOOT_KEY}', 0) + 1\""
            );
            if self.monitor.run(&hook).is_ok() {
                self.reboots.counting = true;
            }
        }
    }

    /// Write and `include` the `reset` macro: reload the image, then replay the
    /// descriptor's post-load commands.
    fn register_reset_macro(&mut self, firmware: &Path) -> Result<()> {
        let abs = firmware
            .to_str()
            .context("non-UTF-8 firmware path for the reset macro")?;
        let loader = match firmware
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("hex") => "LoadHEX",
            _ => "LoadELF",
        };
        let cpu = self.config.cpu.clone();
        let mut body = format!("sysbus {loader} @{abs}\n");
        for cmd in &self.config.post_load_setup {
            body.push_str(&cmd.replace(CPU_PLACEHOLDER, &cpu));
            body.push('\n');
        }
        let script = format!("macro reset\n\"\"\"\n{body}\"\"\"\n");
        let path = std::env::temp_dir().join(format!(
            "hauksbee-renode-reset-{}-{}.resc",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, script).with_context(|| format!("writing {}", path.display()))?;
        self.monitor
            .run(&format!("include \"{}\"", path.display()))
            .context("including the reset macro")?;
        // Removed on drop, with the generated bridge sources.
        self.bridge_source_files.push(path);
        Ok(())
    }

    /// The address the firmware's reset entry executes at: its own symbol if
    /// the ELF names one, else the reset word of the Cortex-M vector table.
    fn reset_entry(&mut self, firmware: &Path) -> Option<u32> {
        for name in RESET_ENTRY_SYMBOLS {
            if let Some(addr) = crate::elf::symbol_address(firmware, name).filter(|&a| a != 0) {
                return Some(addr & !1);
            }
        }
        if self.config.expected_e_machine != crate::elf::EM_ARM {
            return None;
        }
        let vtor = self.vector_table_offset()?;
        let word = self.read_word_abs(vtor.wrapping_add(4))?;
        (word != 0).then_some(word & !1)
    }

    /// Read the boot counter after a run. Cheap when counting is not installed.
    pub(super) fn poll_reboots(&mut self) {
        if !self.reboots.counting {
            return;
        }
        let read = |monitor: &mut super::Monitor, key: &str| {
            monitor
                .command(&format!(
                    "python \"import __builtin__; print getattr(__builtin__, '{key}', 0)\""
                ))
                .ok()
                .and_then(|r| parse_u64(&r))
        };
        if let Some(n) = read(&mut self.monitor, BOOT_KEY) {
            self.reboots.boots = n;
        }
    }

    /// Reboot the core: peripherals back to power-on state, then the `reset`
    /// macro starts the firmware from its vector table again.
    ///
    /// # Errors
    ///
    /// When no `reset` macro is registered, or Renode rejects the reset.
    pub(super) fn reset_core(&mut self) -> Result<()> {
        if !self.reboots.macro_ready {
            bail!("the Renode machine has no reset macro; the core cannot be rebooted");
        }
        self.monitor
            .run("machine Reset")
            .context("resetting the Renode machine")?;
        self.reboots.host_resets += 1;
        // The reset zeroed every peripheral: forget the polled GPIO state so
        // the next poll rebuilds edges from the power-on registers.
        self.last_odr.values_mut().for_each(|v| *v = 0);
        self.last_dir.clear();
        self.poll_reboots();
        Ok(())
    }
}
