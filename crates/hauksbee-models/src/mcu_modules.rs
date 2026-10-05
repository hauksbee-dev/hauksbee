//! MCU module aliases: a module's value string to the MCU it carries.
//!
//! The table lives in `db/mcu_modules.toml` (see the header there for the
//! rules on adding an entry). It only renames: the binder routes the returned
//! part name through its ordinary family router, so an alias cannot invent a
//! backend and a family with no emulator is still refused, by name.

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
    value_re: String,
    mcu: String,
    source: String,
}

/// One module alias.
#[derive(Debug, Clone)]
pub struct McuModuleAlias {
    /// Stable identifier, for diagnostics.
    pub id: String,
    /// The underlying MCU part name, as the family router reads it.
    pub mcu: String,
    /// Where the module's MCU is stated.
    pub source: String,
    value_re: Regex,
}

fn parse(src: &str) -> Result<Vec<McuModuleAlias>, String> {
    let file: File = toml::from_str(src).map_err(|e| format!("mcu_modules: {e}"))?;
    file.mcu_modules
        .into_iter()
        .map(|e| {
            if [&e.value_re, &e.mcu, &e.source]
                .iter()
                .any(|s| s.trim().is_empty())
            {
                return Err(format!(
                    "mcu_modules: entry '{}' needs value_re, mcu and source",
                    e.id
                ));
            }
            let value_re = RegexBuilder::new(&e.value_re)
                .case_insensitive(true)
                .build()
                .map_err(|err| format!("mcu_modules: entry '{}': {err}", e.id))?;
            Ok(McuModuleAlias {
                id: e.id,
                mcu: e.mcu,
                source: e.source,
                value_re,
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

/// The alias whose pattern matches `value` (trimmed), if any.
#[must_use]
pub fn mcu_module_alias(value: &str) -> Option<&'static McuModuleAlias> {
    let value = value.trim();
    builtin_mcu_modules()
        .iter()
        .find(|a| a.value_re.is_match(value))
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
        ] {
            assert_eq!(
                mcu_module_alias(value).map(|a| a.mcu.as_str()),
                Some(mcu),
                "{value}"
            );
        }
        for value in ["nano", "T-Micro", "ESP32_WROOM32", "XIAO_RP2040"] {
            assert!(mcu_module_alias(value).is_none(), "{value}");
        }
    }

    #[test]
    fn an_entry_without_a_source_is_rejected() {
        let src = "[[mcu_modules]]\nid='x'\nvalue_re='^X$'\nmcu='ESP32'\nsource=''\n";
        assert!(parse(src).is_err());
    }
}
