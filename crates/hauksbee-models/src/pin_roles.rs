//! The power-pin vocabulary of a card's `[models.pins]` roles.
//!
//! A card names each pad's role as a free string, and the power pads are the
//! ones every board check eventually needs to find: which pads feed the part,
//! which return its current. This is the one table that says it. A role is a
//! *primary supply* (`vcc`, `vdd`, `avdd`, `vddio`, `iovdd`, `vcca`, ...) or a
//! *return* (`gnd`, `agnd`, `pgnd`, `vss`, `avss`, ...), or neither.
//!
//! What counts is deliberately narrow, because a check that reads it must
//! never call a legal board wrong:
//!
//! * A bare voltage name (`3v3`, `5v`) is not a supply: on a dev board that
//!   pad is as often an output as an input.
//! * A regulated internal rail (`vddcore`, `vddcp`, `vregvdd`, `vcap`), a
//!   switched or external-option supply (`extvcc`, `vbat`, `vbus`, `vin`,
//!   `vm`), the input of an internal regulator the part can bypass (`uvcc`,
//!   the USB pad regulator of an AVR: its `UVREGE` bit disables the regulator
//!   when the output pin is powered directly) and an exposed or sense return
//!   (`gnd_ep`, `sensgnd`) are not primary: a part is not wrong for leaving
//!   them to a capacitor or open.
//! * A numbered role (`vdd2`, `vddio3`) is one more pad of the family its
//!   digit-free name makes; a family is a supply the part needs only when the
//!   card also names its base pad (`vdd`, or `vdd1`). A card that lists only
//!   `vddio2` describes a second supply some packages leave unused, and says
//!   nothing a check may hold the board to.
//!
//! The cards carry no per-pin "optional" flag, so this vocabulary is the whole
//! of what "required" means for the power-pin checks; widening it is a card
//! decision, made here once.

use std::sync::OnceLock;

use regex::Regex;

/// Whether a power pad feeds the part or returns its current.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerKind {
    /// A primary supply input: the part is unpowered without it.
    Supply,
    /// A return: the part has no reference without it.
    Return,
}

/// A card role read as a power pad.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PowerPin {
    pub kind: PowerKind,
    /// The role without its numbering: `vdd2` and `vdd1a` are both `vdd`. Every
    /// return shares the family `gnd`: a part with any return connected has a
    /// reference.
    pub family: String,
    /// Whether this role names the family's base pad (no number, or `1`). A
    /// family with no base pad on a card is not one the part is held to.
    pub base: bool,
    /// A true ground (`gnd`, `agnd`, `pgnd`...) as against `vss`/`avss`, which on
    /// a split-supply amplifier is its negative rail.
    pub ground: bool,
}

fn supply_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^(?P<fam>(?:a|d|io|usb_|t|cp)?(?:vcc|vdd)(?:a|b|d|io|ana|q)?)(?P<num>\d*)[a-d]?(?:_[a-d])?$",
        )
        .expect("supply role regex")
    })
}

fn return_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^(?:a|d|p|u|cp)?(?P<root>gnd|vss)(?:a|io|_a)?\d*$").expect("return role regex")
    })
}

/// `role` read as a power pad, or `None` for any other role (and for the
/// internal rails, options and bare voltage names the module doc leaves out).
#[must_use]
pub fn power_pin(role: &str) -> Option<PowerPin> {
    let r = role.trim().to_ascii_lowercase();
    if let Some(c) = supply_re().captures(&r) {
        let num = &c["num"];
        return Some(PowerPin {
            kind: PowerKind::Supply,
            family: c["fam"].to_string(),
            base: num.is_empty() || num == "1",
            ground: false,
        });
    }
    if matches!(r.as_str(), "vio" | "v+" | "vs") {
        return Some(PowerPin {
            kind: PowerKind::Supply,
            family: r,
            base: true,
            ground: false,
        });
    }
    let c = return_re().captures(&r)?;
    Some(PowerPin {
        kind: PowerKind::Return,
        family: "gnd".to_string(),
        base: true,
        ground: &c["root"] == "gnd",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn supply(role: &str) -> Option<(String, bool)> {
        power_pin(role)
            .filter(|p| p.kind == PowerKind::Supply)
            .map(|p| (p.family, p.base))
    }

    #[test]
    fn primary_supplies_group_by_family() {
        for (role, family, base) in [
            ("vcc", "vcc", true),
            ("VDD", "vdd", true),
            ("vdd2", "vdd", false),
            ("vdd1a", "vdd", true),
            ("vdd33_a", "vdd", false),
            ("vdda33_b", "vdda", false),
            ("vcca", "vcca", true),
            ("vccb", "vccb", true),
            ("vddio", "vddio", true),
            ("vddio2", "vddio", false),
            ("iovdd3", "iovdd", false),
            ("avcc", "avcc", true),
            ("avdd", "avdd", true),
            ("dvdd2", "dvdd", false),
            ("usb_vdd", "usb_vdd", true),
            ("tvdd", "tvdd", true),
            ("cpvdd", "cpvdd", true),
            ("vio", "vio", true),
            ("v+", "v+", true),
            ("vs", "vs", true),
        ] {
            assert_eq!(supply(role), Some((family.to_string(), base)), "{role}");
        }
    }

    #[test]
    fn internal_rails_options_and_bare_voltages_are_not_primary() {
        for role in [
            "3v3", "5v", "3v3a", "adc_avdd", "vddcore", "vddcp", "vddcr", "vregvdd", "extvcc",
            "vbat", "vbus", "vin", "vm", "uvcc", "vref", "vcap1", "v3", "gnd_ep", "avss_ep",
            "sensgnd", "gnd_aux", "vregvss", "in_plus", "out", "en",
        ] {
            assert_eq!(power_pin(role), None, "{role}");
        }
    }

    #[test]
    fn returns_share_one_family_and_split_ground_from_vss() {
        for (role, ground) in [
            ("gnd", true),
            ("gnd3", true),
            ("agnd", true),
            ("dgnd2", true),
            ("pgnd", true),
            ("gndio", true),
            ("gnd_a", true),
            ("ugnd", true),
            ("vss", false),
            ("vss2", false),
            ("avss", false),
            ("vssa", false),
        ] {
            let p = power_pin(role).unwrap_or_else(|| panic!("{role} is a return"));
            assert_eq!(p.kind, PowerKind::Return, "{role}");
            assert_eq!(p.family, "gnd", "{role}");
            assert_eq!(p.ground, ground, "{role}");
        }
    }
}
