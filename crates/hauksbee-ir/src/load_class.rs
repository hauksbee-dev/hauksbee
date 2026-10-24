//! The classes of load a datasheet condition is stated for.
//!
//! A datasheet states some figures only for a kind of load: a load switch's
//! output needs 120 uF "for the USB application", where each downstream-facing
//! port must hold VBUS up through a hot-plug, and a 10 nF to 100 nF ceramic for
//! "the general application". A card row that carries one of these classes in
//! `applies_when` is judged only on a board whose net actually reaches such a
//! load; this module is the one list of the classes, read by the card schema,
//! the checks and the drafting guidance.

use serde::{Deserialize, Serialize};

/// A kind of load a card row is conditioned on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LoadClass {
    /// A USB downstream-facing port: the VBUS pin of a USB-A, micro-AB or USB-C
    /// receptacle the board powers.
    UsbDownstream,
}

impl LoadClass {
    /// Every class, in the order the guidance lists them.
    pub const ALL: [Self; 1] = [Self::UsbDownstream];

    /// The card key (`applies_when = "usb_downstream"`).
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::UsbDownstream => "usb_downstream",
        }
    }

    /// The words a finding uses for the load.
    #[must_use]
    pub const fn words(self) -> &'static str {
        match self {
            Self::UsbDownstream => "a USB downstream port",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_class_key_is_its_serde_name() {
        for class in LoadClass::ALL {
            let json = serde_json::to_string(&class).unwrap();
            assert_eq!(json, format!("\"{}\"", class.key()));
            assert_eq!(serde_json::from_str::<LoadClass>(&json).unwrap(), class);
        }
    }
}
