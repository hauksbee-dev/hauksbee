//! `[models.i2c]`: the 7-bit address an I2C target answers at and what sets it.
//!
//! A board check can only say that two parts on one bus answer at the same
//! address if the cards state the address. The block holds what the datasheet's
//! address table holds: the address the part answers at when nothing selects
//! another, the address-select pins and the address each combination of their
//! levels gives, any further addresses the part answers at, and whether
//! firmware can move the address once the board is up (and which pin it needs
//! for that).
//!
//! ```toml
//! [models.i2c]
//! address = 0x48                    # the default / ADDR-to-GND address
//! basis = "TI ADS1115 SBAS444E, 7.5.1.1 PDF p.20 Table 7-2"
//! [[models.i2c.select]]             # one row per strap combination of the table
//! address = 0x48
//! straps = { addr = "low" }
//! [[models.i2c.select]]
//! address = 0x4A
//! straps = { addr = "sda" }
//! ```
//!
//! A part with no address pin states `address` alone. A part whose address is
//! chosen by pins states one `select` row per combination the datasheet lists;
//! a board whose straps are not all resolvable, or match no row, gets no
//! address from the check, which then says nothing about the part.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::schema::ModelEntry;

/// The level an address-select pin is tied to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum I2cStrap {
    /// Tied (directly or through a resistor) to ground.
    Low,
    /// Tied to the part's own supply or a positive rail.
    High,
    /// Tied to the bus's SDA line (four-level address pins).
    Sda,
    /// Tied to the bus's SCL line (four-level address pins).
    Scl,
    /// Left unconnected, for a pin whose datasheet gives an address for that.
    Open,
}

/// One row of the datasheet's address table: the address the strap levels give.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct I2cAddressRow {
    /// The 7-bit address.
    pub address: u8,
    /// `[models.pins]` role of each address-select pin -> the level it is tied to.
    pub straps: BTreeMap<String, I2cStrap>,
}

/// How firmware moves the address after power-up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum I2cReassignVia {
    /// A register write sets a new address (the VL53L0X and VL6180X
    /// `I2C_SLAVE_DEVICE_ADDRESS` register), lost at the next reset.
    Software,
}

/// A part whose address firmware can move.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct I2cReassign {
    /// What moves it.
    pub via: I2cReassignVia,
    /// The pin the part must be held in reset or shutdown on while its
    /// neighbour is moved (XSHUT, an enable): without a way to sequence it, the
    /// part wakes at the default address together with any other part there.
    /// Absent when no pin is needed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires_pin: Option<String>,
    /// The datasheet sentence and page for the mechanism.
    pub basis: String,
}

fn default_sda() -> String {
    "sda".into()
}
fn default_scl() -> String {
    "scl".into()
}

/// `[models.i2c]` (see the module documentation).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct I2cAddress {
    /// The part's fixed address, or the datasheet's default for one with address pins.
    ///
    /// A board whose straps match no `select` row is not assumed to use it.
    pub address: u8,
    /// `[models.pins]` role of the data line.
    #[serde(default = "default_sda")]
    pub sda_role: String,
    /// `[models.pins]` role of the clock line.
    #[serde(default = "default_scl")]
    pub scl_role: String,
    /// The datasheet's address table, one row per combination of strap levels.
    #[serde(default, rename = "select", skip_serializing_if = "Vec::is_empty")]
    pub select: Vec<I2cAddressRow>,
    /// Further 7-bit addresses the part also answers at, whatever the straps
    /// (a second function on the same die, such as a combined accelerometer and
    /// magnetometer).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub also: Vec<u8>,
    /// How firmware moves the address, when it can.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reassignable: Option<I2cReassign>,
    /// The datasheet table or sentence, with page, the address is read from.
    pub basis: String,
}

impl I2cAddress {
    /// What is wrong with the block on `entry`, worded as the fix.
    #[must_use]
    pub fn problems(&self, entry: &ModelEntry) -> Vec<String> {
        let mut out = Vec::new();
        let has_role = |r: &str| entry.pins.values().any(|x| x.eq_ignore_ascii_case(r));
        let seven_bit = |a: u8| (0x08..=0x77).contains(&a);
        if !seven_bit(self.address) {
            out.push(format!(
                "[models.i2c] address 0x{:02x} is outside the usable 7-bit range 0x08..=0x77",
                self.address
            ));
        }
        for role in [&self.sda_role, &self.scl_role] {
            if !has_role(role) {
                out.push(format!(
                    "[models.i2c] bus role '{role}' is not a role in [models.pins]"
                ));
            }
        }
        if self.basis.trim().is_empty() {
            out.push("[models.i2c] needs a basis citing the datasheet table and page".into());
        }
        for row in &self.select {
            if !seven_bit(row.address) {
                out.push(format!(
                    "[[models.i2c.select]] address 0x{:02x} is outside the usable 7-bit range",
                    row.address
                ));
            }
            if row.straps.is_empty() {
                out.push("[[models.i2c.select]] row names no strap".into());
            }
            for role in row.straps.keys() {
                if !has_role(role) {
                    out.push(format!(
                        "[[models.i2c.select]] strap role '{role}' is not a role in [models.pins]"
                    ));
                }
            }
        }
        if let Some(first) = self.select.first() {
            for (i, row) in self.select.iter().enumerate() {
                if !row.straps.keys().eq(first.straps.keys()) {
                    out.push(
                        "[[models.i2c.select]] rows must all name the same strap roles".into(),
                    );
                    break;
                }
                if self.select[..i].iter().any(|r| r.straps == row.straps) {
                    out.push(format!(
                        "[[models.i2c.select]] states the straps {:?} twice",
                        row.straps
                    ));
                }
            }
            if !self.select.iter().any(|r| r.address == self.address) {
                out.push(format!(
                    "[models.i2c] address 0x{:02x} is not one of the [[models.i2c.select]] rows",
                    self.address
                ));
            }
        }
        for a in &self.also {
            if !seven_bit(*a) {
                out.push(format!(
                    "[models.i2c] also 0x{a:02x} is not a usable 7-bit address"
                ));
            }
        }
        if let Some(r) = &self.reassignable {
            if let Some(pin) = &r.requires_pin {
                if !has_role(pin) {
                    out.push(format!(
                        "[models.i2c.reassignable] requires_pin '{pin}' is not a role in [models.pins]"
                    ));
                }
            }
            if r.basis.trim().is_empty() {
                out.push("[models.i2c.reassignable] needs a basis citing the page".into());
            }
        }
        out
    }

    /// The addresses the part answers at for the strap `levels` its board
    /// holds, with the main one first. `None` when a strap is not in `levels`
    /// or the levels match no row: the board does not say which address it is.
    #[must_use]
    pub fn addresses_for(&self, levels: &BTreeMap<String, I2cStrap>) -> Option<Vec<u8>> {
        let main = if self.select.is_empty() {
            self.address
        } else {
            self.select
                .iter()
                .find(|row| {
                    row.straps
                        .iter()
                        .all(|(role, want)| levels.get(role) == Some(want))
                })?
                .address
        };
        let mut all = vec![main];
        all.extend(self.also.iter().copied().filter(|a| *a != main));
        Some(all)
    }

    /// The strap roles the address depends on (empty for a fixed address).
    #[must_use]
    pub fn strap_roles(&self) -> Vec<&str> {
        self.select
            .first()
            .map(|r| r.straps.keys().map(String::as_str).collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(toml_text: &str) -> I2cAddress {
        toml::from_str(toml_text).expect("block parses")
    }

    #[test]
    fn a_fixed_address_needs_no_straps() {
        let b = block("address = 0x68\nbasis = \"fixture p.1\"");
        assert_eq!(b.addresses_for(&BTreeMap::new()), Some(vec![0x68]));
        assert!(b.strap_roles().is_empty());
        assert_eq!(b.sda_role, "sda");
    }

    #[test]
    fn rows_are_matched_by_every_strap_and_unmatched_levels_give_nothing() {
        let b = block(
            "address = 0x48\nbasis = \"p.1\"\n\
             [[select]]\naddress = 0x48\nstraps = { addr = \"low\" }\n\
             [[select]]\naddress = 0x49\nstraps = { addr = \"high\" }\n",
        );
        let at = |s: I2cStrap| BTreeMap::from([("addr".to_owned(), s)]);
        assert_eq!(b.addresses_for(&at(I2cStrap::High)), Some(vec![0x49]));
        assert_eq!(b.addresses_for(&at(I2cStrap::Sda)), None);
        assert_eq!(b.addresses_for(&BTreeMap::new()), None);
    }

    #[test]
    fn further_addresses_follow_the_main_one() {
        let b = block("address = 0x19\nalso = [0x1e, 0x19]\nbasis = \"p.1\"");
        assert_eq!(b.addresses_for(&BTreeMap::new()), Some(vec![0x19, 0x1e]));
    }

    #[test]
    fn problems_name_a_bad_role_or_a_reserved_address() {
        let mut entry = ModelEntry::default();
        entry.pins.insert("1".into(), "sda".into());
        let b = block("address = 0x02\nbasis = \"p.1\"");
        let p = b.problems(&entry);
        assert!(p.iter().any(|m| m.contains("0x02")), "{p:?}");
        assert!(p.iter().any(|m| m.contains("'scl'")), "{p:?}");
    }
}
