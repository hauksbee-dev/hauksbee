//! The one reader of an MCU backend id.
//!
//! A card names its emulator as `<kind>:<part>` (`simavr:atmega328p`,
//! `renode:stm32f103`, `qemu:esp32`, `none:esp32s2`), and a `SoC` descriptor names
//! the same kind bare (`backend = "renode"`). Every consumer parses the string
//! here, so a new kind or a changed spelling has one place to change and no
//! caller keeps its own `strip_prefix`.
//!
//! Pure std; no emulator link, so it builds in every feature shape.

/// The in-process AVR core kind (`simavr:<part>`).
pub const SIMAVR: &str = "simavr";
/// The external Renode kind (`renode:<part>`).
pub const RENODE: &str = "renode";
/// The external Espressif QEMU kind (`qemu:<part>`).
pub const QEMU: &str = "qemu";
/// A recognised family no emulator models (`none:<family>`).
pub const NONE: &str = "none";

/// A parsed backend id, borrowing the part (or family) from the id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendId<'a> {
    /// `simavr:<part>`, the in-process AVR core.
    Simavr(&'a str),
    /// `renode:<part>`.
    Renode(&'a str),
    /// `qemu:<part>`.
    Qemu(&'a str),
    /// `none:<family>`: recognised, but no emulator models it.
    None(&'a str),
}

impl<'a> BackendId<'a> {
    /// Parse `kind:part`. `None` for an unknown kind, a missing `:` or an empty
    /// part, so a caller that must refuse unknown ids has one test to make.
    #[must_use]
    pub fn parse(id: &'a str) -> Option<Self> {
        let (kind, part) = id.split_once(':')?;
        if part.is_empty() {
            return None;
        }
        match kind {
            SIMAVR => Some(Self::Simavr(part)),
            RENODE => Some(Self::Renode(part)),
            QEMU => Some(Self::Qemu(part)),
            NONE => Some(Self::None(part)),
            _ => None,
        }
    }

    /// The kind token (`simavr`, `renode`, `qemu`, `none`).
    #[must_use]
    pub const fn kind(self) -> &'static str {
        match self {
            Self::Simavr(_) => SIMAVR,
            Self::Renode(_) => RENODE,
            Self::Qemu(_) => QEMU,
            Self::None(_) => NONE,
        }
    }

    /// The part (or, for `none`, the family) after the colon.
    #[must_use]
    pub const fn part(self) -> &'a str {
        match self {
            Self::Simavr(p) | Self::Renode(p) | Self::Qemu(p) | Self::None(p) => p,
        }
    }
}

/// The simavr core named by `id`, when it is a `simavr:<part>` id.
#[must_use]
pub fn simavr_part(id: &str) -> Option<&str> {
    match BackendId::parse(id)? {
        BackendId::Simavr(part) => Some(part),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_parses_and_round_trips() {
        for (id, kind, part) in [
            ("simavr:atmega328p", SIMAVR, "atmega328p"),
            ("renode:stm32f103", RENODE, "stm32f103"),
            ("qemu:esp32", QEMU, "esp32"),
            ("none:esp32s2", NONE, "esp32s2"),
        ] {
            let parsed = BackendId::parse(id).expect(id);
            assert_eq!((parsed.kind(), parsed.part()), (kind, part), "{id}");
            assert_eq!(format!("{}:{}", parsed.kind(), parsed.part()), id);
        }
    }

    #[test]
    fn unknown_kinds_and_malformed_ids_do_not_parse() {
        for id in [
            "",
            "simavr",
            "simavr:",
            ":atmega328p",
            "simvar:atmega328p",
            "Renode:x",
        ] {
            assert_eq!(BackendId::parse(id), None, "{id:?}");
        }
        assert_eq!(simavr_part("renode:stm32f103"), None);
        assert_eq!(simavr_part("simavr:attiny85"), Some("attiny85"));
    }
}
