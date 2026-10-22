//! A chip's boot-mode table: which strap levels at reset release send it where.
//!
//! The static strap lint ([`crate::schema::StrapPin`]) asks whether a strap net
//! is *defensible*. This table answers the co-simulator's question instead:
//! given the levels the solved board actually holds on the strap pins as reset
//! is released, which boot mode does the vendor's table select, and does the
//! user firmware run in it?
//!
//! The card carries the vendor's table verbatim (`[[models.boot_modes]]`, every
//! row citing its document, table and page) plus the electrical facts a reader
//! of the board needs (`[[models.boot_inputs]]`: thresholds and the silicon pull
//! of each strap pin; `[models.reset_input]`: the pin whose release starts the
//! part). [`select_mode`] is the pure lookup. Deciding what *level* a board net
//! holds is the binder's and the co-simulator's job (it needs the solved
//! circuit); this module only turns levels into a mode.

use std::collections::BTreeMap;

use hauksbee_ir::boot::BootOutcome;
use serde::{Deserialize, Serialize};

use crate::schema::StrapInternalPull;

/// A pin level in the vendor's table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PinLevel {
    Low,
    High,
}

impl PinLevel {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::High => "high",
        }
    }
}

/// The event that latches the strap pins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LatchEvent {
    /// The strap pins are read as the reset pin is released (STM32, RP2040).
    #[default]
    ResetRelease,
    /// Read at the rising edge of the chip-enable pin (ESP32 `CHIP_PU`/`EN`).
    ChipPu,
    /// Read once, as the supply comes up.
    PowerOn,
}

/// `[models.strap_latch]`: when the strap pins are sampled.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
pub struct StrapLatch {
    #[serde(default)]
    pub event: LatchEvent,
    /// In plain words: what exactly latches the pins.
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub cite: String,
}

/// `[models.reset_input]`: the pin whose release starts the part.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct ResetInput {
    /// Role in the card's `[models.pins]`.
    pub role: String,
    /// Maximum volts read as low, as `[per_vdd, offset_v]`.
    #[serde(default)]
    pub vil: Option<[f64; 2]>,
    /// Minimum volts read as high, as `[per_vdd, offset_v]`.
    #[serde(default)]
    pub vih: Option<[f64; 2]>,
    /// True when the pin is active-high reset (a pin that holds the part in
    /// reset while it reads high). Every part so far resets on a low level.
    #[serde(default)]
    pub active_high: bool,
    #[serde(default)]
    pub cite: String,
}

/// `[[models.boot_inputs]]`: one pin the boot table reads.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct BootInput {
    /// Role in the card's `[models.pins]`.
    pub role: String,
    /// The pin's name for a reader (`BOOT0`, `GPIO0`); the role upper-cased
    /// when absent.
    #[serde(default)]
    pub pin: Option<String>,
    /// The pull the silicon applies at the latch, which settles an undriven
    /// net to a level.
    #[serde(default)]
    pub internal_pull: StrapInternalPull,
    #[serde(default)]
    pub vil: Option<[f64; 2]>,
    #[serde(default)]
    pub vih: Option<[f64; 2]>,
    /// What else the pin does at the latch, or a caveat about its level.
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub cite: String,
}

impl BootInput {
    /// The name shown for the pin.
    #[must_use]
    pub fn display_name(&self) -> String {
        self.pin.clone().unwrap_or_else(|| self.role.to_uppercase())
    }
}

/// `[[models.boot_modes]]`: one row of the vendor's table.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct BootModeRow {
    /// The vendor's name for the mode.
    pub name: String,
    /// Pin role -> required level. A role not named is a don't-care.
    #[serde(default)]
    pub when: BTreeMap<String, PinLevel>,
    pub outcome: BootOutcome,
    /// What the part does in this mode, in plain words.
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub cite: String,
}

/// How a part's ROM bootloader talks to a host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RomProtocol {
    /// ST's USART bootloader (AN3155): a 0x7F auto-baud byte, then commands.
    StUsart,
    /// A USB mass-storage / DFU boot with no serial protocol (RP2040 BOOTSEL).
    UsbOnly,
}

/// `[models.rom_bootloader]`: what answers when a boot mode runs the ROM
/// bootloader instead of the user firmware.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct RomBootloader {
    pub protocol: RomProtocol,
    /// The bootloader's protocol version byte (`0x31` for v3.1), when the
    /// protocol reports one.
    #[serde(default)]
    pub version: Option<u8>,
    /// The product ID the Get ID command returns.
    #[serde(default)]
    pub product_id: Option<u16>,
    /// The command codes the Get command lists, in the vendor's order.
    #[serde(default)]
    pub commands: Vec<u8>,
    /// Which serial port the bootloader listens on, in words (`USART1 on
    /// PA9/PA10`).
    #[serde(default)]
    pub port: String,
    #[serde(default)]
    pub cite: String,
}

/// `[models.qemu_boot]`: how the strap levels reach the QEMU machine that
/// models this chip (Espressif's fork reads them as one register value).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct QemuBoot {
    /// The GPIO device the `strap_mode` property belongs to (`esp32.gpio`).
    pub device: String,
    /// The property name.
    #[serde(default = "default_strap_property")]
    pub property: String,
    /// The register value QEMU's own table calls flash boot. The straps with a
    /// defined level are overlaid on this value, so a pin with none keeps
    /// QEMU's default.
    pub flash_boot: u32,
    /// Strap role -> bit of the value.
    pub bits: BTreeMap<String, u8>,
    #[serde(default)]
    pub cite: String,
}

fn default_strap_property() -> String {
    "strap_mode".to_string()
}

impl QemuBoot {
    /// The `strap_mode` value for the strap levels in force: the flash-boot
    /// value with each given role's bit set or cleared. A role with no defined
    /// level keeps the machine's default bit; a role with no bit in the table
    /// is ignored.
    #[must_use]
    pub fn strap_value(&self, levels: &BTreeMap<String, PinLevel>) -> u32 {
        let mut value = self.flash_boot;
        for (role, level) in levels {
            if let Some(&bit) = self.bits.get(role) {
                match level {
                    PinLevel::High => value |= 1 << bit,
                    PinLevel::Low => value &= !(1 << bit),
                }
            }
        }
        value
    }
}

/// The pins and table of one card, borrowed for lookups.
#[derive(Debug, Clone, Copy)]
pub struct BootTable<'a> {
    pub latch: Option<&'a StrapLatch>,
    pub reset: Option<&'a ResetInput>,
    pub inputs: &'a [BootInput],
    pub modes: &'a [BootModeRow],
}

impl BootTable<'_> {
    /// Whether the card carries a boot table at all.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.modes.is_empty()
    }

    /// Every role any row reads, in first-use order.
    #[must_use]
    pub fn strap_roles(&self) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for row in self.modes {
            for role in row.when.keys() {
                if !out.contains(&role.as_str()) {
                    out.push(role);
                }
            }
        }
        out
    }

    /// The input row for `role`.
    #[must_use]
    pub fn input(&self, role: &str) -> Option<&BootInput> {
        self.inputs.iter().find(|i| i.role == role)
    }
}

/// A card's whole boot table, owned: what the binder hands the co-simulator so
/// it can resolve the boot mode without the model library.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BootSpec {
    pub latch: Option<StrapLatch>,
    pub reset: Option<ResetInput>,
    pub inputs: Vec<BootInput>,
    pub modes: Vec<BootModeRow>,
    pub qemu: Option<QemuBoot>,
    pub rom: Option<RomBootloader>,
}

impl BootSpec {
    /// Borrow as a [`BootTable`].
    #[must_use]
    pub fn table(&self) -> BootTable<'_> {
        BootTable {
            latch: self.latch.as_ref(),
            reset: self.reset.as_ref(),
            inputs: &self.inputs,
            modes: &self.modes,
        }
    }
}

impl crate::schema::ModelEntry {
    /// The card's boot-mode table, borrowed.
    #[must_use]
    pub fn boot_table(&self) -> BootTable<'_> {
        BootTable {
            latch: self.strap_latch.as_ref(),
            reset: self.reset_input.as_ref(),
            inputs: &self.boot_inputs,
            modes: &self.boot_modes,
        }
    }

    /// The card's boot table as an owned [`BootSpec`], or `None` when the card
    /// has no boot-mode rows.
    #[must_use]
    pub fn boot_spec(&self) -> Option<BootSpec> {
        (!self.boot_modes.is_empty()).then(|| BootSpec {
            latch: self.strap_latch.clone(),
            reset: self.reset_input.clone(),
            inputs: self.boot_inputs.clone(),
            modes: self.boot_modes.clone(),
            qemu: self.qemu_boot.clone(),
            rom: self.rom_bootloader.clone(),
        })
    }
}

/// The result of looking the levels up in the table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection<'a> {
    /// The row the vendor's table selects.
    Row(&'a BootModeRow),
    /// Rows still fit, but a role they read has no defined level. `roles` are
    /// the roles that could not be read and `rows` the rows still possible.
    Indeterminate {
        roles: Vec<String>,
        rows: Vec<&'a BootModeRow>,
    },
    /// The table has no row for these levels (the vendor lists the
    /// combination as reserved, or omits it).
    NoRow,
}

#[derive(PartialEq, Eq)]
enum Fit {
    Match,
    Unknown,
    Mismatch,
}

fn fit(row: &BootModeRow, levels: &BTreeMap<String, Option<PinLevel>>) -> Fit {
    let mut unknown = false;
    for (role, want) in &row.when {
        match levels.get(role).copied().flatten() {
            Some(got) if got == *want => {}
            Some(_) => return Fit::Mismatch,
            None => unknown = true,
        }
    }
    if unknown {
        Fit::Unknown
    } else {
        Fit::Match
    }
}

/// Most unread roles [`select_mode`] enumerates; a card with more reads as
/// indeterminate.
const MAX_UNKNOWN_ROLES: usize = 10;

/// The row the vendor's table selects for the levels read (first match wins).
/// A role missing from `levels`, or mapped to `None`, has no defined level.
///
/// When undefined roles keep several rows possible, the answer is
/// [`Selection::Row`] only if every way the undefined roles could fall (every
/// high/low assignment) lands on a row with the same outcome; a way that lands
/// on no row at all counts against it, because the vendor's table omits that
/// combination. Otherwise the answer is [`Selection::Indeterminate`].
#[must_use]
pub fn select_mode<'a>(
    modes: &'a [BootModeRow],
    levels: &BTreeMap<String, Option<PinLevel>>,
) -> Selection<'a> {
    let mut possible: Vec<&BootModeRow> = Vec::new();
    for row in modes {
        match fit(row, levels) {
            Fit::Mismatch => {}
            Fit::Match if possible.is_empty() => return Selection::Row(row),
            Fit::Match | Fit::Unknown => possible.push(row),
        }
    }
    if possible.is_empty() {
        return Selection::NoRow;
    }
    let mut roles: Vec<String> = Vec::new();
    for row in &possible {
        for role in row.when.keys() {
            if levels.get(role).copied().flatten().is_none() && !roles.contains(role) {
                roles.push(role.clone());
            }
        }
    }
    let indeterminate =
        |roles: Vec<String>, rows: Vec<&'a BootModeRow>| Selection::Indeterminate { roles, rows };
    if roles.len() > MAX_UNKNOWN_ROLES {
        return indeterminate(roles, possible);
    }
    let mut first_row: Option<&'a BootModeRow> = None;
    for mask in 0..(1_u32 << roles.len()) {
        let mut assigned = levels.clone();
        for (i, role) in roles.iter().enumerate() {
            let level = if (mask >> i) & 1 == 1 {
                PinLevel::High
            } else {
                PinLevel::Low
            };
            assigned.insert(role.clone(), Some(level));
        }
        let landed = modes.iter().find(|r| fit(r, &assigned) == Fit::Match);
        match (landed, first_row) {
            (None, _) => return indeterminate(roles, possible),
            (Some(row), None) => first_row = Some(row),
            (Some(row), Some(first)) if row.outcome != first.outcome => {
                return indeterminate(roles, possible);
            }
            (Some(_), Some(_)) => {}
        }
    }
    first_row.map_or_else(|| indeterminate(roles, possible), Selection::Row)
}

/// `per_vdd * vdd + offset_v` for an affine threshold.
#[must_use]
pub fn threshold(affine: [f64; 2], vdd: f64) -> f64 {
    affine[0].mul_add(vdd, affine[1])
}

/// The level `volts` reads as against the thresholds, or `None` between them.
/// A missing threshold falls back to the half-supply point, the usual CMOS
/// midpoint, only when both are missing.
#[must_use]
pub fn level_from_volts(
    volts: f64,
    vdd: f64,
    vil: Option<[f64; 2]>,
    vih: Option<[f64; 2]>,
) -> Option<PinLevel> {
    let (lo, hi) = match (vil, vih) {
        (Some(l), Some(h)) => (threshold(l, vdd), threshold(h, vdd)),
        (Some(l), None) => (threshold(l, vdd), vdd * 0.7),
        (None, Some(h)) => (vdd * 0.3, threshold(h, vdd)),
        (None, None) => (vdd * 0.3, vdd * 0.7),
    };
    if volts <= lo {
        Some(PinLevel::Low)
    } else if volts >= hi {
        Some(PinLevel::High)
    } else {
        None
    }
}

/// The level an undriven strap net settles to from the silicon pull.
#[must_use]
pub const fn pull_level(pull: StrapInternalPull) -> Option<PinLevel> {
    match pull {
        StrapInternalPull::PullUp => Some(PinLevel::High),
        StrapInternalPull::PullDown => Some(PinLevel::Low),
        StrapInternalPull::None | StrapInternalPull::Unknown => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, when: &[(&str, PinLevel)], outcome: BootOutcome) -> BootModeRow {
        BootModeRow {
            name: name.into(),
            when: when.iter().map(|&(r, l)| (r.to_string(), l)).collect(),
            outcome,
            detail: String::new(),
            cite: String::new(),
        }
    }

    fn stm32() -> Vec<BootModeRow> {
        vec![
            row(
                "Main flash",
                &[("boot0", PinLevel::Low)],
                BootOutcome::UserFlash,
            ),
            row(
                "System memory",
                &[("boot0", PinLevel::High), ("boot1", PinLevel::Low)],
                BootOutcome::RomBootloader,
            ),
            row(
                "Embedded SRAM",
                &[("boot0", PinLevel::High), ("boot1", PinLevel::High)],
                BootOutcome::Sram,
            ),
        ]
    }

    fn lv(pairs: &[(&str, Option<PinLevel>)]) -> BTreeMap<String, Option<PinLevel>> {
        pairs.iter().map(|&(r, l)| (r.to_string(), l)).collect()
    }

    #[test]
    fn boot0_low_selects_main_flash_whatever_boot1_does() {
        let modes = stm32();
        let sel = select_mode(
            &modes,
            &lv(&[("boot0", Some(PinLevel::Low)), ("boot1", None)]),
        );
        assert!(matches!(sel, Selection::Row(r) if r.outcome == BootOutcome::UserFlash));
    }

    #[test]
    fn boot0_high_with_boot1_low_selects_the_system_bootloader() {
        let modes = stm32();
        let sel = select_mode(
            &modes,
            &lv(&[
                ("boot0", Some(PinLevel::High)),
                ("boot1", Some(PinLevel::Low)),
            ]),
        );
        assert!(matches!(sel, Selection::Row(r) if r.outcome == BootOutcome::RomBootloader));
    }

    #[test]
    fn an_undefined_boot0_is_indeterminate_because_the_outcomes_differ() {
        let modes = stm32();
        let sel = select_mode(
            &modes,
            &lv(&[("boot0", None), ("boot1", Some(PinLevel::Low))]),
        );
        match sel {
            Selection::Indeterminate { roles, rows } => {
                assert_eq!(roles, ["boot0"]);
                assert_eq!(rows.len(), 2);
            }
            other => panic!("expected indeterminate, got {other:?}"),
        }
    }

    #[test]
    fn an_undefined_pin_that_cannot_change_the_outcome_still_resolves() {
        let modes = stm32();
        let sel = select_mode(&modes, &lv(&[("boot0", Some(PinLevel::Low))]));
        assert!(matches!(sel, Selection::Row(r) if r.name == "Main flash"));
    }

    #[test]
    fn a_combination_the_table_omits_has_no_row() {
        let modes = vec![row(
            "flash",
            &[("gpio0", PinLevel::High)],
            BootOutcome::UserFlash,
        )];
        let sel = select_mode(&modes, &lv(&[("gpio0", Some(PinLevel::Low))]));
        assert_eq!(sel, Selection::NoRow);
    }

    #[test]
    fn an_unread_pin_whose_other_state_has_no_row_is_indeterminate() {
        // Only one state of gpio1 selects a row; the other has none.
        let modes = vec![
            row(
                "flash",
                &[("gpio0", PinLevel::High)],
                BootOutcome::UserFlash,
            ),
            row(
                "spi download",
                &[("gpio0", PinLevel::Low), ("gpio1", PinLevel::High)],
                BootOutcome::RomBootloader,
            ),
        ];
        let sel = select_mode(
            &modes,
            &lv(&[("gpio0", Some(PinLevel::Low)), ("gpio1", None)]),
        );
        assert!(matches!(sel, Selection::Indeterminate { .. }), "{sel:?}");
    }

    #[test]
    fn thresholds_are_affine_in_the_supply() {
        // VIL max 0.3 VDD, VIH min 0.7 VDD at a 3.3 V supply.
        let vil = Some([0.3, 0.0]);
        let vih = Some([0.7, 0.0]);
        assert_eq!(level_from_volts(0.2, 3.3, vil, vih), Some(PinLevel::Low));
        assert_eq!(level_from_volts(3.0, 3.3, vil, vih), Some(PinLevel::High));
        assert_eq!(level_from_volts(1.65, 3.3, vil, vih), None);
    }

    #[test]
    fn qemu_strap_value_overlays_only_driven_bits() {
        let q = QemuBoot {
            device: "esp32.gpio".into(),
            property: "strap_mode".into(),
            flash_boot: 0x12,
            bits: [("gpio0".to_string(), 4), ("gpio2".to_string(), 3)].into(),
            cite: String::new(),
        };
        assert_eq!(q.strap_value(&BTreeMap::new()), 0x12);
        let driven = [("gpio0".to_string(), PinLevel::Low)].into();
        assert_eq!(q.strap_value(&driven), 0x02);
    }
}
