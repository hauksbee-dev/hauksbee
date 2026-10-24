//! MCU module aliases: a module's value string to the MCU it carries, and a
//! module's footprint or symbol to the identity a card can match.
//!
//! The table lives in `db/mcu_modules.toml` (see the header there for the
//! rules on adding an entry). It only renames: the binder routes the returned
//! part name through its ordinary family router, so an alias cannot invent a
//! backend and a family with no emulator is still refused, by name.
//!
//! Three kinds of entry:
//! * `value_re` + `mcu`: the module's value names the MCU it carries.
//! * `footprint_re` / `lib_re` + `as_value`: the module's footprint or symbol
//!   is specific to one module, so a board whose value says nothing (`~`,
//!   `MCU`, a project part number) is resolved as if its value were `as_value`.
//! * `blocker`: the identity pattern does not fix the chip (a footprint shared
//!   by several boards) or the library has no pad map for the module; the
//!   binder leaves the part unresolved and the report says `blocker`, plainly.

use regex::{Regex, RegexBuilder};
use serde::Deserialize;
use std::sync::OnceLock;

const BUILTIN: &str = include_str!("../db/mcu_modules.toml");

#[derive(Debug, Deserialize)]
struct File {
    #[serde(default)]
    mcu_modules: Vec<Entry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    id: String,
    #[serde(default)]
    value_re: Option<String>,
    #[serde(default)]
    footprint_re: Option<String>,
    #[serde(default)]
    lib_re: Option<String>,
    #[serde(default)]
    mcu: Option<String>,
    #[serde(default)]
    as_value: Option<String>,
    #[serde(default)]
    blocker: Option<String>,
    source: String,
}

/// One module alias.
#[derive(Debug, Clone)]
pub struct McuModuleAlias {
    /// Stable identifier, for diagnostics.
    pub id: String,
    /// The underlying MCU part name, as the family router reads it (empty for
    /// an entry that only names a footprint or a blocker).
    pub mcu: String,
    /// For a footprint or symbol entry: the value to resolve the part as.
    pub as_value: Option<String>,
    /// Why the module cannot be bound, when the table says so.
    pub blocker: Option<String>,
    /// Where the module's MCU (or the ambiguity) is stated.
    pub source: String,
    value_re: Option<Regex>,
    footprint_re: Option<Regex>,
    lib_re: Option<Regex>,
}

fn regex(entry: &str, pattern: &str) -> Result<Regex, String> {
    RegexBuilder::new(pattern)
        .case_insensitive(true)
        .build()
        .map_err(|err| format!("mcu_modules: entry '{entry}': {err}"))
}

fn blank(s: Option<&str>) -> bool {
    s.is_none_or(|s| s.trim().is_empty())
}

fn parse(src: &str) -> Result<Vec<McuModuleAlias>, String> {
    let file: File = toml::from_str(src).map_err(|e| format!("mcu_modules: {e}"))?;
    file.mcu_modules
        .into_iter()
        .map(|e| {
            if e.source.trim().is_empty() {
                return Err(format!("mcu_modules: entry '{}' needs a source", e.id));
            }
            let (value, footprint, lib) = (
                !blank(e.value_re.as_deref()),
                !blank(e.footprint_re.as_deref()),
                !blank(e.lib_re.as_deref()),
            );
            if !(value || footprint || lib) {
                return Err(format!(
                    "mcu_modules: entry '{}' needs value_re, footprint_re or lib_re",
                    e.id
                ));
            }
            if value && blank(e.mcu.as_deref()) {
                return Err(format!(
                    "mcu_modules: entry '{}' has a value_re and needs the mcu it names",
                    e.id
                ));
            }
            if (footprint || lib) && blank(e.as_value.as_deref()) && blank(e.blocker.as_deref()) {
                return Err(format!(
                    "mcu_modules: entry '{}' matches a footprint or symbol and needs as_value or blocker",
                    e.id
                ));
            }
            let compile = |p: Option<&str>| {
                p.filter(|p| !p.trim().is_empty())
                    .map(|p| regex(&e.id, p))
                    .transpose()
            };
            Ok(McuModuleAlias {
                value_re: compile(e.value_re.as_deref())?,
                footprint_re: compile(e.footprint_re.as_deref())?,
                lib_re: compile(e.lib_re.as_deref())?,
                mcu: e.mcu.clone().unwrap_or_default(),
                as_value: e.as_value.clone().filter(|s| !s.trim().is_empty()),
                blocker: e.blocker.clone().filter(|s| !s.trim().is_empty()),
                id: e.id,
                source: e.source,
            })
        })
        .collect()
}

/// The built-in alias table, parsed once.
///
/// # Panics
///
/// Never in a shipped build: the embedded table is checked by a unit test.
#[must_use]
pub fn builtin_mcu_modules() -> &'static [McuModuleAlias] {
    static TABLE: OnceLock<Vec<McuModuleAlias>> = OnceLock::new();
    TABLE.get_or_init(|| parse(BUILTIN).expect("db/mcu_modules.toml is valid"))
}

/// The alias whose value pattern matches `value` (trimmed), if any.
#[must_use]
pub fn mcu_module_alias(value: &str) -> Option<&'static McuModuleAlias> {
    let value = value.trim();
    builtin_mcu_modules()
        .iter()
        .find(|a| a.value_re.as_ref().is_some_and(|re| re.is_match(value)))
}

/// The entry that a part's library symbol or footprint names, if any. A
/// footprint (`Library:Name`) and a symbol id are each tried against the
/// entry's own pattern.
#[must_use]
pub fn mcu_module_by_identity(lib_id: &str, footprint: &str) -> Option<&'static McuModuleAlias> {
    let (lib_id, footprint) = (lib_id.trim(), footprint.trim());
    builtin_mcu_modules().iter().find(|a| {
        let by_footprint = a
            .footprint_re
            .as_ref()
            .is_some_and(|re| !footprint.is_empty() && re.is_match(footprint));
        let by_lib = a
            .lib_re
            .as_ref()
            .is_some_and(|re| !lib_id.is_empty() && re.is_match(lib_id));
        by_footprint || by_lib
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_table_parses_and_matches_real_spellings() {
        assert!(builtin_mcu_modules().len() >= 4);
        for (value, mcu) in [
            ("ESP32_WROOM32_SKINNY", "ESP32-WROOM-32"),
            ("T-Micro32_Plus", "ESP32-PICO-V3-02"),
            ("nice_nano", "nRF52840"),
            ("nice!nano v2", "nRF52840"),
            ("XIAO_ESP32C6", "ESP32-C6"),
            ("Pico 2", "RP2350"),
            ("RPi_Pico2_SMD", "RP2350"),
            ("Raspberry Pi Pico 2 W", "RP2350"),
        ] {
            assert_eq!(
                mcu_module_alias(value).map(|a| a.mcu.as_str()),
                Some(mcu),
                "{value}"
            );
        }
        for value in [
            "nano",
            "T-Micro",
            "ESP32_WROOM32",
            "XIAO_RP2040",
            "Pico",
            "Pico W",
            "RPi_Pico_SMD",
        ] {
            assert!(mcu_module_alias(value).is_none(), "{value}");
        }
    }

    #[test]
    fn a_module_specific_footprint_names_the_value_to_resolve_as() {
        for (fp, as_value) in [
            ("RF_Module:ESP32-WROOM-32", "ESP32-WROOM-32"),
            ("RF_Module:ESP32-WROOM-32U", "ESP32-WROOM-32"),
            ("RF_Module:ESP32-WROVER-E", "ESP32-WROVER"),
            ("RF_Module:ESP32-S3-WROOM-1", "ESP32-S3-WROOM-1"),
            ("RF_Module:ESP32-C3-MINI-1", "ESP32-C3-MINI-1"),
        ] {
            let entry = mcu_module_by_identity("", fp).unwrap_or_else(|| panic!("{fp}"));
            assert_eq!(entry.as_value.as_deref(), Some(as_value), "{fp}");
            assert!(entry.blocker.is_none(), "{fp}");
        }
        // The S3 WROVER is a different chip from the classic WROVER.
        assert!(mcu_module_by_identity("", "RF_Module:ESP32-S3-WROVER-1")
            .and_then(|e| e.as_value.as_deref())
            .is_none_or(|v| v != "ESP32-WROVER"));
    }

    #[test]
    fn a_footprint_shared_by_several_chips_blocks_with_a_plain_reason() {
        for fp in [
            "Module:RaspberryPi_Pico_Common_THT",
            "Module:RaspberryPi_Pico_SMD",
            "Module:Arduino_Nano",
            "Module:Arduino_Nano_WithMountingHoles",
            "Module:Arduino_Pro_Mini",
        ] {
            let entry = mcu_module_by_identity("", fp).unwrap_or_else(|| panic!("{fp}"));
            assert!(entry.as_value.is_none(), "{fp}");
            assert!(entry.blocker.is_some(), "{fp}");
        }
    }

    #[test]
    fn a_dev_board_on_a_shared_footprint_is_named_by_its_value() {
        for (value, mcu) in [
            ("Arduino_Nano_Every", "ATmega4809"),
            ("Arduino Nano ESP32", "ESP32-S3"),
            ("Arduino_Nano_RP2040_Connect", "RP2040"),
            ("Arduino_Nano_33_BLE", "nRF52840"),
            ("Arduino_Nano_33_IoT", "SAMD21"),
            ("Adafruit_Feather_M0", "ATSAMD21G18"),
            ("Adafruit_Feather_HUZZAH_ESP8266", "ESP8266"),
            ("WEMOS_D1_mini", "ESP8266EX"),
            ("Teensy4.1", "MIMXRT1062"),
            ("Teensy 3.2", "MK20DX256"),
            ("TeensyLC", "MKL26Z64"),
            ("Teensy2.0", "ATmega32U4"),
            ("Seeeduino XIAO", "ATSAMD21G18"),
            ("XIAO-nRF52840-DIP", "nRF52840"),
        ] {
            let entry = mcu_module_alias(value).unwrap_or_else(|| panic!("{value}"));
            assert_eq!(entry.mcu, mcu, "{value}");
            let blocker = entry
                .blocker
                .as_deref()
                .unwrap_or_else(|| panic!("{value}"));
            assert!(
                blocker.contains(mcu.trim_end_matches("EX")),
                "{value}: {blocker}"
            );
        }
        // The classic Nano and the boards the library carries cards for are not
        // blocked by value.
        for value in [
            "Arduino_Nano_v3.x",
            "Arduino Nano",
            "XIAO RP2040",
            "XIAO-ESP32C3-DIP",
            "Adafruit_Feather_HUZZAH32_ESP32",
            "Feather RP2040",
            "Teensy++2.0",
        ] {
            assert!(
                mcu_module_alias(value)
                    .and_then(|e| e.blocker.as_ref())
                    .is_none(),
                "{value}"
            );
        }
    }

    #[test]
    fn a_dev_board_symbol_or_footprint_names_the_board() {
        for (lib, fp, as_value) in [
            (
                "MCU_Module:RaspberryPi_Pico_Extensive",
                "Module:RaspberryPi_Pico_SMD",
                "Pico",
            ),
            ("MCU_Module:RaspberryPi_Pico_Debug", "", "Pico"),
            (
                "MCU_Module:RaspberryPi_Pico_W",
                "Module:RaspberryPi_Pico_W_SMD",
                "Pico W",
            ),
            (
                "",
                "Seeed_Studio_XIAO_Series:XIAO-RP2040-DIP",
                "XIAO RP2040",
            ),
            (
                "Seeed_Studio_XIAO_Series:XIAO-ESP32C3-SMD",
                "",
                "XIAO ESP32C3",
            ),
            (
                "",
                "Seeed_Studio_XIAO_Series:XIAO-ESP32S3-DIP",
                "XIAO ESP32S3",
            ),
        ] {
            let entry = mcu_module_by_identity(lib, fp).unwrap_or_else(|| panic!("{lib} {fp}"));
            assert_eq!(entry.as_value.as_deref(), Some(as_value), "{lib} {fp}");
        }
        // The RP2350 Pico 2 symbol is not taken for the RP2040 Pico.
        assert!(mcu_module_by_identity("MCU_Module:RaspberryPi_Pico2", "")
            .and_then(|e| e.as_value.as_deref())
            .is_none());
        let feather = mcu_module_by_identity("", "Module:Adafruit_Feather").unwrap();
        assert!(feather
            .blocker
            .as_deref()
            .is_some_and(|b| b.contains("Feather")));
    }

    #[test]
    fn a_bare_chip_or_connector_footprint_matches_no_module() {
        for fp in [
            "Package_DFN_QFN:QFN-56-1EP_7x7mm_P0.4mm_EP3.2x3.2mm",
            "Package_QFP:LQFP-48_7x7mm_P0.5mm",
            "Connector_PinHeader_2.54mm:PinHeader_2x20_P2.54mm_Vertical",
            "RF_Module:ESP32-PICO-D4",
            "",
        ] {
            assert!(mcu_module_by_identity("", fp).is_none(), "{fp}");
        }
    }

    #[test]
    fn an_entry_without_a_source_is_rejected() {
        let src = "[[mcu_modules]]\nid='x'\nvalue_re='^X$'\nmcu='ESP32'\nsource=''\n";
        assert!(parse(src).is_err());
    }

    #[test]
    fn a_footprint_entry_needs_a_value_to_resolve_as_or_a_blocker() {
        let src = "[[mcu_modules]]\nid='x'\nfootprint_re='X'\nsource='s'\n";
        assert!(parse(src).is_err());
        let src = "[[mcu_modules]]\nid='x'\nfootprint_re='X'\nas_value='Y'\nsource='s'\n";
        assert!(parse(src).is_ok());
    }

    #[test]
    fn a_value_entry_needs_the_mcu_it_names() {
        let src = "[[mcu_modules]]\nid='x'\nvalue_re='^X$'\nsource='s'\n";
        assert!(parse(src).is_err());
    }
}
