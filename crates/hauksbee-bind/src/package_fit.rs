//! Whether a card's pin map fits the footprint it is about to bind to.
//!
//! A map read off another package's column binds cleanly and simulates a
//! different circuit: supply and ground roles land on GPIO pads and nothing
//! complains, while the part counts as bound. [`misfit`] refuses such a card,
//! and the binder and the coverage report then leave the part unmodelled:
//!
//! * the package the card's map is for (`validation::pin_package`) against the
//!   footprint: its outline (a leadless quad is no leaded quad) and its pin
//!   count against the footprint's numbered pads, one exposed pad allowed;
//! * the board's ground against the card's ground roles: when no pad the card
//!   calls ground sits on a ground-named net, yet the board grounds a pad the
//!   card leaves unknown or unmapped, the map is not this footprint's.
//!
//! [`unknown_pads`] is the advisory half: a card that leaves many pads unknown
//! is named, with the supply and ground nets those pads sit on.

use hauksbee_extract::Component;
use hauksbee_models::package::{outline, pin_count};
use hauksbee_models::{ComponentKind, ModelEntry};

/// A connected pin's net as the check sees it: on the ground node, and its name.
pub type NetView<'a> = &'a dyn Fn(Option<i64>) -> Option<Seen>;

/// A net as [`NetView`] reports it: on the ground node, and its name.
pub type Seen = (bool, String);

/// A pad and the net it sits on, if any.
type Pad = (String, Option<Seen>);

/// The card's role for a pad, an Eagle `P$n` pad read as `n` as the binder does.
fn card_role<'m>(model: &'m ModelEntry, pad: &str) -> Option<&'m String> {
    let ordinal = pad.strip_prefix("P$").unwrap_or(pad);
    model.pins.get(pad).or_else(|| model.pins.get(ordinal))
}

fn ground_role(role: &str) -> bool {
    let r = role.to_ascii_lowercase();
    r.starts_with("gnd") || r.ends_with("gnd") || r.starts_with("vss") || r == "ground"
}

fn ground_named(name: &str) -> bool {
    let leaf = name.rsplit('/').next().unwrap_or(name).to_ascii_uppercase();
    ["GND", "VSS", "GROUND"].iter().any(|g| leaf.contains(g)) || leaf == "0"
}

fn unknown_role(role: Option<&String>) -> bool {
    role.is_none_or(|r| r.starts_with("unknown_pad_"))
}

/// The footprint's distinct numbered pads, when the component comes from a layout.
fn layout_pads(comp: &Component) -> Option<usize> {
    let pads = comp.pins.iter().filter_map(|p| {
        p.number
            .strip_prefix("P$")
            .unwrap_or(&p.number)
            .parse::<u32>()
            .ok()
    });
    let pads: std::collections::BTreeSet<u32> = pads.collect();
    (comp.position.is_some() && !pads.is_empty()).then_some(pads.len())
}

/// Why `model`'s pin map does not fit `comp`'s footprint, or `None` when
/// nothing on the board contradicts it.
#[must_use]
pub fn misfit(comp: &Component, model: &ModelEntry, net: NetView) -> Option<String> {
    use ComponentKind::{Connector, Ignore, Passive};
    if matches!(model.kind, Passive | Connector | Ignore) || model.pins.is_empty() {
        return None;
    }
    let footprint = if comp.footprint.is_empty() {
        &comp.lib_id
    } else {
        &comp.footprint
    };
    let id = &model.id;
    if let Some(package) = hauksbee_models::validation::pin_package(model) {
        if let (Some(want), Some(got)) = (outline(package), outline(footprint)) {
            if want != got {
                return Some(format!("card '{id}' maps the pins of a {package} ({want:?}), but footprint '{footprint}' is {got:?}"));
            }
        }
        let pads = layout_pads(comp).or_else(|| pin_count(footprint).map(|n| n as usize));
        if let (Some(n), Some(pads)) = (pin_count(package), pads) {
            let n = n as usize;
            if pads < n || pads > n + 1 {
                return Some(format!("card '{id}' maps the pins of a {n}-pin {package}, but footprint '{footprint}' has {pads} numbered pads"));
            }
        }
    }
    ground_misfit(comp, model, net)
}

/// The card's ground pads off every ground-named net while the board grounds
/// pads the card leaves unknown or unmapped.
fn ground_misfit(comp: &Component, model: &ModelEntry, net: NetView) -> Option<String> {
    let pads = |keep: &dyn Fn(Option<&String>, Option<&Seen>) -> bool| -> Vec<Pad> {
        let view = comp.pins.iter().map(|p| (p.number.clone(), net(p.net)));
        view.filter(|(pad, v)| keep(card_role(model, pad), v.as_ref()))
            .collect()
    };
    let grounds = pads(&|role, _| role.is_some_and(|r| ground_role(r)));
    let grounded = |v: Option<&Seen>| v.is_some_and(|(g, name)| *g || ground_named(name));
    if grounds.is_empty() || grounds.iter().any(|(_, v)| grounded(v.as_ref())) {
        return None;
    }
    let stray = pads(&|role, v| unknown_role(role) && v.is_some_and(|(g, _)| *g));
    if stray.is_empty() {
        return None;
    }
    let on = |(pad, v): &Pad| {
        let at = v.as_ref().map_or("no net", |(_, name)| name.as_str());
        format!("{pad} ({at})")
    };
    let list = |v: &[Pad]| v.iter().map(on).collect::<Vec<_>>().join(", ");
    let stray: Vec<_> = stray.iter().map(|(pad, _)| pad.as_str()).collect();
    Some(format!(
        "card '{}' puts ground on pad(s) {}, none on a ground net, while the board grounds pad(s) {} that the card leaves unknown or unmapped: the map was read for another package or pinout",
        model.id,
        list(&grounds),
        stray.join(", ")
    ))
}

/// The advisory for a card that leaves many pads unknown, naming the supply
/// and ground nets those pads sit on (`rail` names a pin's net when it is one).
#[must_use]
pub fn unknown_pads(
    comp: &Component,
    model: &ModelEntry,
    rail: &dyn Fn(Option<i64>) -> Option<String>,
) -> Option<String> {
    let base = hauksbee_models::validation::unknown_pads_warning(model)?;
    let unknown = comp
        .pins
        .iter()
        .filter(|p| unknown_role(card_role(model, &p.number)));
    let on: Vec<String> = unknown
        .filter_map(|p| Some(format!("{} ({})", p.number, rail(p.net)?)))
        .collect();
    let supply = if on.is_empty() {
        String::new()
    } else {
        format!(
            "; unknown or unmapped pads on supply or ground nets: {}",
            on.join(", ")
        )
    };
    Some(format!(
        "{} ({}): card '{}': {base}{supply}",
        comp.reference, comp.value, model.id
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hauksbee_extract::Pin;

    /// A fake 8-pin MCU card printing two columns: an 8-pin QFN (VDD 1, GND 8)
    /// and a 10-pin SOP (VDD 2, GND 9), with its map read off `package`'s.
    fn card(package: &str, gnd: &str, vdd: &str) -> ModelEntry {
        let pins: Vec<String> = (1..=8)
            .map(|n| match n.to_string() {
                p if p == gnd => format!("\"{p}\" = \"gnd\""),
                p if p == vdd => format!("\"{p}\" = \"vdd\""),
                p => format!("\"{p}\" = \"p{p}\""),
            })
            .collect();
        let toml = format!(
            "[[models]]\nid = \"fake_mcu\"\nkind = \"mcu\"\ndescription = \"fake MCU in {package}\"\n\
             [models.match]\nvalue_re = \"^FAKEMCU$\"\n[models.variant]\norderable_re = \"^FAKEMCU\"\n\
             package = \"{package}\"\nbasis = \"ordering table\"\n[models.pins]\n{}\n",
            pins.join("\n")
        );
        let db: hauksbee_models::schema::DbFile = toml::from_str(&toml).unwrap();
        db.models.into_iter().next().unwrap()
    }

    /// The board's QFN-8 with an exposed pad: VDD on 1, ground on 8 and the pad.
    fn board(footprint: &str, pads: u32) -> Component {
        let net = |n: u32| match n {
            1 => Some(1),
            8 | 9 => Some(0),
            n => Some(i64::from(n) + 10),
        };
        let pin = |n: u32| Pin {
            number: n.to_string(),
            net: net(n),
            ..Pin::default()
        };
        Component {
            reference: "U1".into(),
            value: "FAKEMCU".into(),
            footprint: footprint.into(),
            position: Some((0.0, 0.0, 0.0)),
            pins: (1..=pads).map(pin).collect(),
            ..Component::default()
        }
    }

    fn seen(n: i64) -> Seen {
        (
            n == 0,
            if n == 0 {
                "GND".into()
            } else {
                format!("/N{n}")
            },
        )
    }

    #[test]
    fn a_map_read_off_another_package_column_is_refused_and_the_right_one_binds() {
        let qfn = "Package_DFN_QFN:QFN-8-1EP_3x3mm";
        let cases = [
            // (card package, card gnd pad, card vdd pad, footprint, pads, refusal)
            ("QFN-8", "8", "1", qfn, 9, None),
            ("SOP-10", "9", "2", qfn, 9, Some("SOP-10 (SmallOutline), but footprint")),
            ("QFN-10", "9", "2", qfn, 9, Some("10-pin QFN-10, but footprint")),
            // Same outline and count, wrong column: ground lands on a GPIO.
            ("QFN-8", "3", "2", qfn, 9, Some("ground on pad(s) 3 (/N13), none on a ground net, while the board grounds pad(s) 9")),
        ];
        for (package, gnd, vdd, footprint, pads, want) in cases {
            let got = misfit(&board(footprint, pads), &card(package, gnd, vdd), &|net| {
                net.map(seen)
            });
            match want {
                None => assert_eq!(got, None, "{package}"),
                Some(text) => assert!(
                    got.as_deref().is_some_and(|g| g.contains(text)),
                    "{package}: {got:?}"
                ),
            }
        }
    }

    #[test]
    fn a_card_leaving_most_pads_unknown_is_named_with_the_rails_they_sit_on() {
        let mut model = card("QFN-8", "8", "1");
        for pad in ["1", "2", "3"] {
            model.pins.insert(pad.into(), format!("unknown_pad_{pad}"));
        }
        let rail = |net: Option<i64>| (net == Some(1)).then(|| "+3V3".to_string());
        let warning = unknown_pads(&board("QFN-8", 9), &model, &rail).unwrap();
        assert!(
            warning.contains("3 of 8 pads") && warning.contains("1 (+3V3)"),
            "{warning}"
        );
    }
}
