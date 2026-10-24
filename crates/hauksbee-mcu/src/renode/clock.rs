//! Following the clock tree the firmware configures, and gating oscillator
//! readiness on the board.
//!
//! Renode models no clock tree for most parts: its platforms declare one core
//! rate (`cpu PerformanceInMips`, `nvic systickFrequency`, each timer's
//! `frequency`) and keep it whatever the firmware writes to the RCC. A blinky
//! that sets the F4 PLL to 168 MHz then runs 10.5x slow, and one that waits for
//! `HSERDY` proceeds on a board with no crystal. Both are silent wrong answers.
//!
//! This module closes both gaps from descriptor data alone:
//!
//! - **Retune.** After every slice the backend reads the registers of the
//!   descriptor's clock decode block (`[[soc.decode]]`, kind `clock`) in one
//!   Monitor round trip, evaluates the tree, and re-runs every
//!   `[[soc.clock_targets]]` command whose decoded value changed. The machine
//!   therefore follows the SYSCLK the firmware selected, to within one slice.
//! - **Readiness.** `[[soc.clock_ready]]` entries become one Python hook pair on
//!   the RCC peripheral: a before-write hook timestamps the moment an enable
//!   field turns on, and an after-read hook forces the ready bit to
//!   `enabled for >= startup time, and (for a crystal) the board has one, and
//!   every prerequisite is ready`. The timestamp is exact to the instruction:
//!   Renode's master time source only advances at quantum boundaries, so the
//!   hook derives virtual time from the host's slice start plus
//!   `ExecutedInstructions / PerformanceInMips`.
//!
//! No register layout lives here. Every offset, mask and delay is descriptor
//! data, so the same code serves the F1, F4, G4, L0, F0 and RP2040.
//!
//! Long-form how-and-why: docs/how-and-why/hauksbee-mcu/renode.md.

use super::{Monitor, RenodeBackend, RenodeConfig};
use crate::periph_snapshot::{
    decode_clock, render_clock_command, BlockKind, ClockConfig, PeripheralConfig, RegisterReader,
};
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

/// Per-backend clock-following state.
#[derive(Debug, Default)]
pub(super) struct ClockState {
    /// Virtual time advanced so far, in microseconds. The readiness hooks add
    /// the instructions retired since the last slice to this.
    pub virtual_us: f64,
    /// Whether the Python clock model is loaded (the descriptor has a clock
    /// decode block or readiness entries).
    pub model_loaded: bool,
    /// Last rounded value applied for each `[[soc.clock_targets]]` entry.
    pub(super) applied: HashMap<usize, u64>,
    /// The most recent decode of the clock tree.
    pub last: Option<ClockConfig>,
}

/// Python for one `[[soc.clock_ready]]` table, as a statement list that fills
/// `ENT`. A `None` startup scale and an absent `when` render as Python `None`.
fn entries_literal(config: &RenodeConfig) -> String {
    let mut out = String::from("ENT = {\n");
    for e in &config.clock_ready {
        let scale = e.startup_scale.map_or_else(
            || "None".to_string(),
            |s| {
                format!(
                    "({}, {}, {:?}, {}, {:?})",
                    s.register, s.mask, s.us_per_count, s.multiplier_mask, s.multiplier
                )
            },
        );
        let needs: Vec<String> = e
            .needs
            .iter()
            .map(|n| {
                let when = n.when.map_or_else(
                    || "None".to_string(),
                    |w| format!("({}, {}, {})", w.register, w.mask, w.value),
                );
                format!("('{}', {when})", n.entry)
            })
            .collect();
        let _ = writeln!(
            out,
            "    '{name}': {{'p': '{p}', 'en_reg': {en_reg}, 'en_mask': {en_mask}, \
             'en_val': {en_val}, 'rd_reg': {rd_reg}, 'rd_mask': {rd_mask}, 'ext': {ext}, \
             'su': {su:?}, 'scale': {scale}, 'needs': [{needs}]}},",
            name = e.name,
            p = e.peripheral,
            en_reg = e.enable_register,
            en_mask = e.enable_mask,
            en_val = e.enabled_value(),
            rd_reg = e.ready_register,
            rd_mask = e.ready_mask,
            ext = if e.requires_external { "True" } else { "False" },
            su = e.startup_us,
            needs = needs.join(", "),
        );
    }
    out.push_str("}\n");
    out
}

/// The readiness model's functions, shared by every part (the per-part data
/// is the `ENT` and `UF` literals the script prepends).
const READINESS_BODY: &str = r"
def now_us():
    return b.hkb_t0 + (b.hkb_cpu.ExecutedInstructions - b.hkb_i0) / float(b.hkb_cpu.PerformanceInMips)

def ready(p, name):
    e = ENT[name]
    t = b.hkb_on.get(name)
    if t is None:
        return False
    if e['ext'] and not b.hkb_ext:
        return False
    for (need, when) in e['needs']:
        if when is not None and (p.ReadDoubleWord(when[0]) & when[1]) != when[2]:
            continue
        if not ready(p, need):
            return False
    su = e['su']
    if e['scale'] is not None:
        r = p.ReadDoubleWord(e['scale'][0])
        su = (r & e['scale'][1]) * e['scale'][2]
        if (r & e['scale'][3]) != 0:
            su = su * e['scale'][4]
    elif e['ext'] and b.hkb_xstated and b.hkb_xsu is not None:
        su = b.hkb_xsu
    el = now_us() - t
    if el < su:
        return False
    if e['ext'] and e['scale'] is not None and b.hkb_xsu is not None and el < b.hkb_xsu:
        # The firmware's delay counter expired before the crystal started:
        # the flag says stable, the oscillator is not. Record the first such
        # read and stop the core, which on silicon has no usable clock.
        if b.hkb_fault is None:
            b.hkb_fault = (name, el, b.hkb_xsu)
        try:
            b.hkb_cpu.IsHalted = True
        except Exception:
            pass
    return True

def on_read(p, pname, offset, value):
    for n in ENT:
        e = ENT[n]
        if e['p'] == pname and e['rd_reg'] == offset:
            if ready(p, n):
                value = value | e['rd_mask']
            else:
                value = value & ~e['rd_mask']
    return value & 0xFFFFFFFF

def on_write(pname, offset, value):
    for n in ENT:
        e = ENT[n]
        if e['p'] == pname and e['en_reg'] == offset:
            if (value & e['en_mask']) == e['en_val']:
                if n not in b.hkb_on:
                    b.hkb_on[n] = now_us()
            else:
                b.hkb_on.pop(n, None)

def uf_write(pname, offset, value):
    for (p, reg, mask, arr) in UF:
        if p != pname:
            continue
        if reg == offset:
            b.hkb_fw[(pname, reg)] = value
            return (value | mask) & 0xFFFFFFFF
        if arr is not None and arr == offset:
            b.hkb_fw[(pname, arr)] = value
            if value < 0xFFFF:
                return value + 1
    return value

def uf_read(pname, offset, value):
    for (p, reg, mask, arr) in UF:
        if p != pname:
            continue
        if reg == offset:
            fw = b.hkb_fw.get((pname, reg), 0)
            return ((value & ~mask) | (fw & mask)) & 0xFFFFFFFF
        if arr is not None and arr == offset:
            return b.hkb_fw.get((pname, arr), value)
    return value

def rd(a):
    return b.hkb_bus.ReadDoubleWord(a)

b.hkb_rdy_read = on_read
b.hkb_rdy_write = on_write
b.hkb_uf_write = uf_write
b.hkb_uf_read = uf_read
b.hkb_fw = {}
b.hkb_rd = rd

def fault():
    f = b.hkb_fault
    if f is None:
        return 'none'
    return '%s %r %r' % (f[0], f[1], f[2])

b.hkb_fault_text = fault
b.hkb_ok = 1
";

/// The Python module that implements oscillator readiness for `config`.
///
/// `external` is whether the board has a crystal. The returned source is
/// executed once in Renode's Python scope; the hooks it defines are looked up
/// by name from `__builtin__`, so the per-access hook command stays one short
/// line and the (large) body is parsed once.
pub(super) fn readiness_script(config: &RenodeConfig, external: bool) -> String {
    let head = format!(
        "import __builtin__ as b\n\
         from Antmicro.Renode.Core import EmulationManager\n\
         _m = None\n\
         for _mm in EmulationManager.Instance.CurrentEmulation.Machines:\n\
         \x20   if str(_mm) == '{machine}':\n\
         \x20       _m = _mm\n\
         b.hkb_cpu = _m['{cpu}']\n\
         b.hkb_bus = _m.SystemBus\n\
         b.hkb_ext = {ext}\n\
         b.hkb_xsu = {xsu}\n\
         b.hkb_xstated = {xstated}\n\
         b.hkb_fault = None\n\
         b.hkb_t0 = 0.0\n\
         b.hkb_i0 = 0\n\
         b.hkb_on = {{}}\n",
        machine = config.machine,
        cpu = config.cpu,
        ext = if external { "True" } else { "False" },
        xsu = config
            .crystal_startup
            .as_ref()
            .map_or_else(|| "None".to_string(), |s| format!("{}.0", s.startup_us)),
        xstated = if config.crystal_startup.as_ref().is_some_and(|s| s.stated) {
            "True"
        } else {
            "False"
        },
    );
    format!(
        "{head}{}{}{READINESS_BODY}",
        entries_literal(config),
        timer_fixes_literal(config)
    )
}

/// Parse the readiness model's fault line, `<name> <waited_us> <needed_us>`,
/// from the last line of a Monitor reply that has one.
fn parse_fault(reply: &str) -> Option<crate::traits::ClockFault> {
    reply.lines().rev().find_map(|line| {
        let mut it = line.split_whitespace();
        let oscillator = it.next()?.to_string();
        let waited_us = it.next()?.parse().ok()?;
        let needed_us = it.next()?.parse().ok()?;
        it.next().is_none().then_some(crate::traits::ClockFault {
            oscillator,
            waited_us,
            needed_us,
        })
    })
}

/// Write `script` to a fresh temp file and return its path.
fn write_script(script: &str) -> Result<PathBuf> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    for _ in 0..1024 {
        let path = std::env::temp_dir().join(format!(
            "hauksbee-renode-clock-{}-{:016x}.py",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut f) => {
                f.write_all(script.as_bytes())
                    .with_context(|| format!("writing clock model to {}", path.display()))?;
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => {
                return Err(e).with_context(|| format!("creating {}", path.display()));
            }
        }
    }
    bail!("could not allocate a temporary file for the Renode clock model")
}

/// The `UF` list of (peripheral, enable register, enable mask, auto-reload
/// register) timer fixes.
fn timer_fixes_literal(config: &RenodeConfig) -> String {
    let mut out = String::from("UF = [\n");
    for u in &config.timer_fixes {
        let _ = writeln!(
            out,
            "    ('{}', {}, {}, {}),",
            u.peripheral,
            u.enable_register,
            u.enable_mask,
            u.auto_reload_register
                .map_or_else(|| "None".to_string(), |r| r.to_string())
        );
    }
    out.push_str("]\n");
    out
}

/// Load the readiness model and attach its hooks to every peripheral the
/// descriptor names. Returns the temp file to delete on drop, or `None` when
/// the descriptor has no `[[soc.clock_ready]]` entries.
///
/// # Errors
///
/// When the script cannot be written, Renode rejects it, or the model does not
/// answer its self-check (a Python error in the hooks would otherwise leave
/// them silently inert).
pub(super) fn install_readiness(
    monitor: &mut Monitor,
    config: &RenodeConfig,
) -> Result<Option<PathBuf>> {
    if config.clock_ready.is_empty()
        && config.timer_fixes.is_empty()
        && clock_addresses(config).is_empty()
    {
        return Ok(None);
    }
    let script = readiness_script(config, config.external_clock.unwrap_or(true));
    let path = write_script(&script)?;
    let mut install = || -> Result<()> {
        monitor
            .run(&format!(
                "python \"exec(open('{}').read())\"",
                path.display()
            ))
            .context("loading the clock-readiness model")?;
        let ok = monitor
            .run("python \"import __builtin__ as b; print(b.hkb_ok)\"")
            .context("checking the clock-readiness model")?;
        if !ok.trim().ends_with('1') {
            bail!("the clock-readiness model did not initialise: {ok}");
        }
        let mut seen: Vec<&str> = Vec::new();
        for e in &config.clock_ready {
            if seen.contains(&e.peripheral.as_str()) {
                continue;
            }
            seen.push(&e.peripheral);
            let p = &e.peripheral;
            monitor.run(&format!(
                "sysbus SetHookAfterPeripheralRead sysbus.{p} \"import __builtin__ as b; \
                 value = b.hkb_rdy_read(self, '{p}', offset, value)\""
            ))?;
            monitor.run(&format!(
                "sysbus SetHookBeforePeripheralWrite sysbus.{p} \"import __builtin__ as b; \
                 b.hkb_rdy_write('{p}', offset, value)\""
            ))?;
        }
        for u in &config.timer_fixes {
            let p = u.peripheral.as_str();
            // The enable bit is forced on inside the emulator from the start;
            // the hooks below keep it on across firmware writes and show the
            // firmware its own value on reads.
            monitor.run(&format!(
                "sysbus.{p} WriteDoubleWord {} {}",
                u.enable_register, u.enable_mask
            ))?;
            if seen.contains(&p) {
                continue;
            }
            seen.push(p);
            monitor.run(&format!(
                "sysbus SetHookAfterPeripheralRead sysbus.{p} \"import __builtin__ as b; \
                 value = b.hkb_uf_read('{p}', offset, value)\""
            ))?;
            monitor.run(&format!(
                "sysbus SetHookBeforePeripheralWrite sysbus.{p} \"import __builtin__ as b; \
                 value = b.hkb_uf_write('{p}', offset, value)\""
            ))?;
        }
        Ok(())
    };
    match install() {
        Ok(()) => Ok(Some(path)),
        Err(e) => {
            let _ = std::fs::remove_file(&path);
            Err(e)
        }
    }
}

/// The distinct absolute register addresses of the first clock decode block,
/// in a fixed order.
fn clock_addresses(config: &RenodeConfig) -> Vec<u32> {
    config
        .decode
        .iter()
        .find(|b| b.kind == BlockKind::Clock)
        .map(|b| b.registers.values().copied().collect())
        .unwrap_or_default()
}

/// A `RegisterReader` over a batch of values read in one Monitor round trip.
struct Batch<'a> {
    addresses: &'a [u32],
    values: &'a [u32],
}

impl RegisterReader for Batch<'_> {
    fn read_u32(&mut self, address: u32) -> Option<u32> {
        let i = self.addresses.iter().position(|a| *a == address)?;
        self.values.get(i).copied()
    }
}

/// A `RegisterReader` that reads through the Monitor, one register at a time.
struct MonitorReader<'a> {
    monitor: &'a mut Monitor,
}

impl RegisterReader for MonitorReader<'_> {
    fn read_u32(&mut self, address: u32) -> Option<u32> {
        self.monitor
            .command(&format!("sysbus ReadDoubleWord 0x{address:X}"))
            .ok()
            .and_then(|r| super::parse_hex_or_dec(&r))
    }
}

impl RenodeBackend {
    /// Account `seconds` of virtual time, refresh the readiness model's time
    /// base, re-read the clock registers and retune the machine if the decoded
    /// clock moved. One Monitor round trip per slice.
    ///
    /// # Errors
    ///
    /// When the Monitor connection fails or a retune command is rejected.
    pub(super) fn follow_clock(&mut self, seconds: f64) -> Result<()> {
        self.clock.virtual_us += seconds * 1_000_000.0;
        if !self.clock.model_loaded {
            return Ok(());
        }
        let addresses = clock_addresses(&self.config);
        // A recorded clock fault keeps the core halted: the halt the hook
        // requested is re-asserted at every slice boundary.
        let mut stmt = format!(
            "import __builtin__ as b; b.hkb_t0 = {:?}; b.hkb_i0 = b.hkb_cpu.ExecutedInstructions; \
             b.hkb_cpu.IsHalted = b.hkb_cpu.IsHalted or b.hkb_fault is not None; ",
            self.clock.virtual_us
        );
        if addresses.is_empty() {
            stmt.push_str("pass");
            self.monitor.run(&format!("python \"{stmt}\""))?;
            return Ok(());
        }
        let list: Vec<String> = addresses.iter().map(|a| format!("0x{a:X}")).collect();
        let _ = write!(
            stmt,
            "print(' '.join('%x' % b.hkb_rd(a) for a in [{}]))",
            list.join(", ")
        );
        let reply = self.monitor.run(&format!("python \"{stmt}\""))?;
        let values: Vec<u32> = reply
            .split_whitespace()
            .filter_map(|t| u32::from_str_radix(t, 16).ok())
            .collect();
        if values.len() != addresses.len() {
            // A short or garbled reply is not evidence of a clock change.
            return Ok(());
        }
        let mut reader = Batch {
            addresses: &addresses,
            values: &values,
        };
        if let Some(clock) = decode_clock(&self.config.decode, &mut reader) {
            self.apply_clock(clock)?;
        }
        Ok(())
    }

    /// Re-run every clock target whose decoded value changed.
    fn apply_clock(&mut self, clock: ClockConfig) -> Result<()> {
        let targets = self.config.clock_targets.clone();
        for (i, target) in targets.iter().enumerate() {
            let Some(hz) = clock
                .values
                .iter()
                .find(|(n, _)| *n == target.var)
                .and_then(|(_, v)| v.known())
            else {
                continue;
            };
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // positive Hz
            let key = hz.round().max(0.0) as u64;
            if self.clock.applied.get(&i) == Some(&key) {
                continue;
            }
            self.monitor
                .run(&render_clock_command(&target.command, hz))
                .with_context(|| format!("retuning {} to {hz} Hz", target.var))?;
            self.clock.applied.insert(i, key);
        }
        self.clock.last = Some(clock);
        Ok(())
    }

    /// The first premature oscillator-ready read the readiness model recorded
    /// (see [`crate::ClockFault`]), or `None` when there was none or the model
    /// is not loaded.
    pub(super) fn read_clock_fault(&mut self) -> Option<crate::traits::ClockFault> {
        if !self.clock.model_loaded || self.config.crystal_startup.is_none() {
            return None;
        }
        let reply = self
            .monitor
            .run("python \"import __builtin__ as b; print(b.hkb_fault_text())\"")
            .ok()?;
        parse_fault(&reply)
    }

    /// Decode every block the descriptor declares from the machine's current
    /// registers.
    pub(super) fn read_peripheral_config(&mut self) -> Option<PeripheralConfig> {
        if self.config.decode.is_empty() {
            return None;
        }
        let blocks = self.config.decode.clone();
        let mut reader = MonitorReader {
            monitor: &mut self.monitor,
        };
        Some(PeripheralConfig::decode(&blocks, &mut reader))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::periph_snapshot::{ClockNeed, ClockReady, RegisterCondition, StartupScale};

    fn config_with(ready: Vec<ClockReady>) -> RenodeConfig {
        let mut c = crate::renode::RenodeConfig::stm32f103();
        c.clock_ready = ready;
        c
    }

    #[test]
    fn script_embeds_every_descriptor_number() {
        let c = config_with(vec![
            ClockReady {
                peripheral: "rcc".into(),
                name: "hse".into(),
                enable_register: 0,
                enable_mask: 0x1_0000,
                enable_value: None,
                ready_register: 0,
                ready_mask: 0x2_0000,
                requires_external: true,
                startup_us: 2000.0,
                startup_scale: None,
                needs: vec![],
            },
            ClockReady {
                peripheral: "rcc".into(),
                name: "pll".into(),
                enable_register: 0,
                enable_mask: 0x100_0000,
                enable_value: None,
                ready_register: 0,
                ready_mask: 0x200_0000,
                requires_external: false,
                startup_us: 200.0,
                startup_scale: None,
                needs: vec![ClockNeed {
                    entry: "hse".into(),
                    when: Some(RegisterCondition {
                        register: 4,
                        mask: 0x1_0000,
                        value: 0x1_0000,
                    }),
                }],
            },
        ]);
        let s = readiness_script(&c, false);
        assert!(s.contains("b.hkb_ext = False"), "{s}");
        assert!(
            s.contains("'hse': {'p': 'rcc', 'en_reg': 0, 'en_mask': 65536"),
            "{s}"
        );
        assert!(s.contains("'su': 2000.0"), "{s}");
        assert!(s.contains("'needs': [('hse', (4, 65536, 65536))]"), "{s}");
        assert!(readiness_script(&c, true).contains("b.hkb_ext = True"));
    }

    /// The RP2040 XOSC delay: `STARTUP.DELAY` counts of 256 crystal cycles,
    /// times 4 while `STARTUP.X4` (bit 20) is set. The script must carry both
    /// the multiplier bit and the factor, and apply them to the live register.
    #[test]
    fn script_carries_the_startup_multiplier() {
        let c = config_with(vec![ClockReady {
            peripheral: "xosc".into(),
            name: "xosc".into(),
            enable_register: 0,
            enable_mask: 0xFF_F000,
            enable_value: Some(0xFA_B000),
            ready_register: 4,
            ready_mask: 0x8000_0000,
            requires_external: true,
            startup_us: 1003.0,
            startup_scale: Some(StartupScale {
                register: 0x0C,
                mask: 0x3FFF,
                us_per_count: 256.0 / 12.0,
                multiplier_mask: 0x10_0000,
                multiplier: 4.0,
            }),
            needs: vec![],
        }]);
        let s = readiness_script(&c, true);
        assert!(
            s.contains("'scale': (12, 16383, 21.333333333333332, 1048576, 4.0)"),
            "{s}"
        );
        assert!(
            s.contains("e['scale'][3]") && s.contains("e['scale'][4]"),
            "{s}"
        );
    }

    /// The board crystal's start-up reaches the model: without one the model
    /// knows none and never records a fault; with one it carries the time and
    /// whether the card states it, and the fault branch compares the
    /// programmed delay against it.
    #[test]
    fn script_carries_the_crystal_startup() {
        let mut c = config_with(vec![]);
        let s = readiness_script(&c, true);
        assert!(s.contains("b.hkb_xsu = None"), "{s}");
        assert!(s.contains("b.hkb_xstated = False"), "{s}");
        c = c.with_crystal_startup(Some(crate::traits::CrystalStartup {
            reference: "Y1".into(),
            startup_us: 10_000,
            stated: true,
        }));
        let s = readiness_script(&c, true);
        assert!(s.contains("b.hkb_xsu = 10000.0"), "{s}");
        assert!(s.contains("b.hkb_xstated = True"), "{s}");
        assert!(s.contains("el < b.hkb_xsu"), "{s}");
        assert!(s.contains("b.hkb_cpu.IsHalted = True"), "{s}");
    }

    #[test]
    fn fault_line_parses_and_none_does_not() {
        let f = parse_fault("xosc 1003.2 10000.0\n").expect("parses");
        assert_eq!(f.oscillator, "xosc");
        assert!((f.waited_us - 1003.2).abs() < 1e-9);
        assert!((f.needed_us - 10_000.0).abs() < 1e-9);
        assert_eq!(parse_fault("none\n"), None);
        assert_eq!(parse_fault(""), None);
    }
}
