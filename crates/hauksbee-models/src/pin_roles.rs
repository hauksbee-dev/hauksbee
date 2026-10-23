//! The power-pin vocabulary of a card's `[models.pins]` roles.
//!
//! A card names each pad's role as a free string, and the power pads are the
//! ones every board check eventually needs to find: which pads feed the part,
//! which return its current. This is the one table that says it. A role is a
//! *primary supply* (`vcc`, `vdd`, `avdd`, `vddio`, `iovdd`, `vcca`, `uvcc`...)
//! or a *return* (`gnd`, `agnd`, `pgnd`, `vss`, `avss`, ...), or neither.
//!
//! What counts is deliberately narrow, because a check that reads it must
//! never call a legal board wrong:
//!
//! * A bare voltage name (`3v3`, `5v`) is not a supply: on a dev board that
//!   pad is as often an output as an input.
//! * A regulated internal rail (`vddcore`, `vddcp`, `vregvdd`, `vcap`), a
//!   switched or external-option supply (`extvcc`, `vbat`, `vbus`, `vin`,
//!   `vm`) and an exposed or sense return (`gnd_ep`, `sensgnd`) are not
//!   primary: a part is not wrong for leaving them to a capacitor or open.
//! * `vs` is a supply only on an amplifier or comparator card (the op-amp
//!   pinout's single supply pin). On a gate driver `VS` is the high-side
//!   switch node, so it is read by part kind, never by name alone.
//! * A numbered role (`vdd2`, `vddio3`) is one more pad of the family its
//!   digit-free name makes; a family is a supply the part needs only when the
//!   card also names its base pad (`vdd`, or `vdd1`). A card that lists only
//!   `vddio2` describes a second supply some packages leave unused, and says
//!   nothing a check may hold the board to.
//!
//! A supply the datasheet lets the board leave open (an AVR's `UVCC` when its
//! USB pad regulator is bypassed) is a card decision, not a vocabulary one: the
//! card lists it under `[[models.optional_supplies]]` with the figure or table
//! that says so ([`crate::ModelEntry::optional_supplies`]).
//!
//! `pin_roles_cases.tsv` beside this file is the shared table of cases this
//! vocabulary and the fault-injection harness's independent statement of it
//! (`qc/fault_injection/catalogue.py`) must both reproduce.

use std::sync::OnceLock;

use regex::Regex;

use crate::ComponentKind;

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
            r"^(?P<fam>(?:a|d|io|usb_|t|cp|u)?(?:vcc|vdd)(?:a|b|d|io|ana|q)?)(?P<num>\d*)[a-d]?(?:_[a-d])?$",
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

/// `role` of a `kind` card read as a power pad, or `None` for any other role
/// (and for the internal rails, options and bare voltage names the module doc
/// leaves out).
#[must_use]
pub fn power_pin(role: &str, kind: ComponentKind) -> Option<PowerPin> {
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
    let amplifier = matches!(kind, ComponentKind::Opamp | ComponentKind::Comparator);
    if matches!(r.as_str(), "vio" | "v+") || (r == "vs" && amplifier) {
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

/// Whether `role` names a sense or measurement input (`sense`, `isense`, `sns`,
/// `imon`, `vmon`, `meas`, `ain0`).
///
/// That is a shunt-sense pin, a monitor or an ADC channel: anything the part
/// reads rather than runs from.
///
/// A sense input's printed range is a measurement window (a shunt's full scale,
/// a differential span, an ADC's input range), not a rail the board supplies.
/// So a `supply_range` row on such a pin is refused by card validation, and a
/// check that reads a pin's rating ignores one: only an `input_tolerance` row,
/// which states a ground-referenced ceiling whatever the supply, speaks for the
/// pin. A supply or return role is never a sense input, and neither are the bare
/// differential inputs `in_plus`/`in_minus` of an amplifier or comparator.
#[must_use]
pub fn is_sense_input_role(role: &str) -> bool {
    let r = role.trim().to_ascii_lowercase();
    if r.is_empty()
        || supply_re().is_match(&r)
        || return_re().is_match(&r)
        || r.contains("gnd")
        || r.contains("vss")
    {
        return false;
    }
    let (word, _) = r.split_once('_').unwrap_or((r.as_str(), ""));
    let measure = |w: &str| {
        let w = w.trim_end_matches(|c: char| c.is_ascii_digit());
        w.contains("sense")
            || w.contains("sns")
            || w == "sens"
            || matches!(w, "imon" | "vmon" | "mon" | "meas" | "ain" | "adcin")
    };
    measure(word) || r.split('_').any(measure)
}

/// Whether `role` names a switching converter's switch node.
///
/// That is `lx`, `ph`, `phase`, `sw` or `sw` with a channel number. Such a pin
/// carries the chopped inductor current, so it senses nothing and is never a
/// quiet regulated rail.
#[must_use]
pub fn is_switch_node_role(role: &str) -> bool {
    let r = role.trim().to_ascii_lowercase();
    matches!(r.as_str(), "lx" | "ph" | "phase")
        || r.strip_prefix("sw")
            .is_some_and(|rest| rest.chars().all(|c| c.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn supply(role: &str) -> Option<(String, bool)> {
        power_pin(role, ComponentKind::Digital)
            .filter(|p| p.kind == PowerKind::Supply)
            .map(|p| (p.family, p.base))
    }

    #[test]
    fn sense_and_measurement_inputs_are_named_by_their_role() {
        for role in [
            "sense",
            "isense",
            "vsense",
            "vsense_a",
            "sense_p",
            "sns",
            "isns",
            "imon",
            "vmon",
            "meas",
            "ain0",
            "sense2",
            "Vbat_sense",
        ] {
            assert!(is_sense_input_role(role), "{role}");
        }
        for role in [
            "vcc", "vdd", "gnd", "sensgnd", "in_plus", "in_minus", "en", "fb", "out", "cs", "csb",
            "sda", "scl", "ctrl", "sw", "vbus", "",
        ] {
            assert!(!is_sense_input_role(role), "{role}");
        }
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
            ("uvcc", "uvcc", true),
            ("tvdd", "tvdd", true),
            ("cpvdd", "cpvdd", true),
            ("vio", "vio", true),
            ("v+", "v+", true),
        ] {
            assert_eq!(supply(role), Some((family.to_string(), base)), "{role}");
        }
    }

    #[test]
    fn internal_rails_options_and_bare_voltages_are_not_primary() {
        for role in [
            "3v3", "5v", "3v3a", "adc_avdd", "vddcore", "vddcp", "vddcr", "vregvdd", "extvcc",
            "vbat", "vbus", "vin", "vm", "vref", "vcap1", "v3", "gnd_ep", "avss_ep", "sensgnd",
            "gnd_aux", "vregvss", "in_plus", "out", "en",
        ] {
            assert_eq!(power_pin(role, ComponentKind::Digital), None, "{role}");
        }
    }

    /// `VS` is an op-amp's supply and a gate driver's switch node.
    #[test]
    fn vs_is_a_supply_only_on_an_amplifier_or_comparator() {
        for kind in [ComponentKind::Opamp, ComponentKind::Comparator] {
            let p = power_pin("vs", kind).expect("an amplifier's VS is its supply");
            assert_eq!((p.kind, p.family.as_str()), (PowerKind::Supply, "vs"));
        }
        for kind in [
            ComponentKind::Vreg,
            ComponentKind::Digital,
            ComponentKind::Mcu,
        ] {
            assert_eq!(power_pin("vs", kind), None, "{kind:?}");
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
            let p = power_pin(role, ComponentKind::Digital)
                .unwrap_or_else(|| panic!("{role} is a return"));
            assert_eq!(p.kind, PowerKind::Return, "{role}");
            assert_eq!(p.family, "gnd", "{role}");
            assert_eq!(p.ground, ground, "{role}");
        }
    }

    /// One row of `pin_roles_cases.tsv`: `role`, the card kind it is read on
    /// (`*` for any), and what it reads as.
    fn cases() -> Vec<(String, String, String)> {
        include_str!("pin_roles_cases.tsv")
            .lines()
            .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
            .map(|l| {
                let cols: Vec<&str> = l.split('\t').collect();
                assert_eq!(cols.len(), 3, "pin_roles_cases.tsv row {l:?}");
                (
                    cols[0].to_string(),
                    cols[1].to_string(),
                    cols[2].to_string(),
                )
            })
            .collect()
    }

    fn kind_of(name: &str) -> ComponentKind {
        ComponentKind::ALL
            .into_iter()
            .find(|k| {
                serde_json::to_value(k)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    == Some(name.to_string())
            })
            .unwrap_or_else(|| panic!("pin_roles_cases.tsv: no kind {name:?}"))
    }

    fn reads(role: &str, kind: ComponentKind) -> String {
        match power_pin(role, kind) {
            None => "none".to_string(),
            Some(p) if p.kind == PowerKind::Supply => {
                format!("supply {} {}", p.family, u8::from(p.base))
            }
            Some(p) => format!("return {}", u8::from(p.ground)),
        }
    }

    /// The shared table the Python harness also reproduces
    /// (`qc/tests/test_fault_injection.py`): every row reads as stated here,
    /// and every role a built-in card names is either a row or reads as no
    /// power pad, so a new card role or a regex edit on one side alone fails.
    #[test]
    fn the_shared_case_table_is_this_vocabulary() {
        let table = cases();
        for (role, kind, want) in &table {
            let kinds: Vec<ComponentKind> = if kind == "*" {
                ComponentKind::ALL.to_vec()
            } else {
                vec![kind_of(kind)]
            };
            for k in kinds {
                assert_eq!(&reads(role, k), want, "{role} on {k:?}");
            }
        }
        let lib = crate::ModelLibrary::builtin();
        for entry in lib.entries.iter().map(|le| &le.compiled.entry) {
            for role in entry.pins.values() {
                let r = role.trim().to_ascii_lowercase();
                let listed = table
                    .iter()
                    .any(|(t, k, _)| *t == r && (k == "*" || kind_of(k) == entry.kind));
                assert!(
                    listed || reads(&r, entry.kind) == "none",
                    "card {} role {r:?} reads as {} but is not in pin_roles_cases.tsv",
                    entry.id,
                    reads(&r, entry.kind)
                );
            }
        }
    }
}
