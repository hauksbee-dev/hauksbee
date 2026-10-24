//! Kelvin sensing of low-value current-sense shunts on a `KiCad` layout.
//!
//! A shunt of a few milliohms drops a few millivolts at full load, so copper
//! carrying the same current between the shunt pad and the sense connection
//! adds an error of the same order. The sense connection must leave the shunt
//! pad on copper of its own (a Kelvin connection). This finds shunts by value
//! and by a sense input (an amplifier, comparator, ADC or current-sense input,
//! directly or through a series resistor) on a net touching the shunt, asks the
//! copper connectivity how each sense tap reaches the pad, and reports a tap
//! that reaches it only through load-carrying copper when the shared copper's
//! estimated resistance exceeds 1% of the shunt.

use std::collections::BTreeSet;

use hauksbee_models::netclass::is_ground;
use hauksbee_models::{ComponentKind, ComponentQuery, Confidence, ModelLibrary};

use crate::drc::connectivity::{kelvin_taps_from_text, TapCopper};
use crate::netlint::{parse_ohms, LintCheck, LintFinding, Severity};
use crate::part_class::{classify_two_terminal, is_active_ic, PartClass};
use crate::{Component, ExtractedBoard, Pin};

/// The largest resistance (ohms) read as a current-sense shunt.
const MAX_SHUNT_OHMS: f64 = 0.1;

/// The smallest series resistor (ohms) read as a sense tap's input resistor
/// rather than a load on the shunt's net.
const MIN_TAP_OHMS: f64 = 100.0;

/// Shared-copper resistance, as a fraction of the shunt, above which the
/// reading error is reported.
const REPORT_FRACTION: f64 = 0.01;

/// Pin names that are a differential or single-ended measurement input.
const SENSE_NAMES: [&str; 34] = [
    "+", "-", "+in", "-in", "in+", "in-", "inp", "inn", "in_p", "in_n", "in_plus", "in_minus",
    "vin+", "vin-", "vinp", "vinn", "sense+", "sense-", "sns+", "sns-", "rs+", "rs-", "isense",
    "isns", "csp", "csn", "cs+", "cs-", "sensep", "sensen", "senp", "senn", "isensep", "isensen",
];

/// Whether a pin name (or a card's pin role) names a measurement input.
fn sense_name(name: &str) -> bool {
    let n: String = name
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_ascii_lowercase();
    if n.is_empty() {
        return false;
    }
    // A unit's number in front ("1IN+") or its letter behind ("in_plus_a",
    // "+INA") names the same input of a multi-unit part.
    let bare = n.trim_start_matches(|c: char| c.is_ascii_digit());
    let unit = |s: &str| -> Option<String> {
        let s = s.strip_suffix(['a', 'b', 'c', 'd', '1', '2', '3', '4'])?;
        Some(s.strip_suffix('_').unwrap_or(s).to_string())
    };
    let forms = [Some(n.clone()), Some(bare.to_string()), unit(bare)];
    if forms
        .iter()
        .flatten()
        .any(|f| SENSE_NAMES.contains(&f.as_str()))
    {
        return true;
    }
    // An ADC channel: a token "adc<n>" / "ain<n>" ("PA0/ADC0", "AIN3").
    n.split(|c: char| !c.is_ascii_alphanumeric()).any(|t| {
        ["adc", "ain"].iter().any(|p| {
            t.strip_prefix(p)
                .is_some_and(|rest| rest.chars().all(|c| c.is_ascii_digit()))
        })
    })
}

/// The card's role for each pad of an amplifier, comparator or ADC; `None` for
/// anything else or a part no card names.
fn sensing_card(c: &Component) -> Option<std::collections::BTreeMap<String, String>> {
    let mut q = ComponentQuery::new(
        Some(c.lib_id.clone()).filter(|s| !s.is_empty()),
        Some(c.value.clone()).filter(|s| !s.is_empty()),
        Some(c.footprint.clone()).filter(|s| !s.is_empty()),
    );
    q.reference = Some(c.reference.clone());
    q.properties.clone_from(&c.properties);
    q.mpn = crate::bom::component_mpn(c);
    let res = ModelLibrary::builtin_shared().resolve(&q);
    if matches!(res.confidence, Confidence::Guessed | Confidence::Unresolved) {
        return None;
    }
    let entry = res.model?;
    matches!(
        entry.kind,
        ComponentKind::Opamp | ComponentKind::Comparator | ComponentKind::Adc
    )
    .then(|| entry.pins.clone())
}

/// One component's view for this check.
struct Part<'a> {
    c: &'a Component,
    class: PartClass,
    ohms: Option<f64>,
    /// Pad numbers that are measurement inputs.
    inputs: BTreeSet<&'a str>,
}

impl<'a> Part<'a> {
    fn of(c: &'a Component) -> Self {
        let class = classify_two_terminal(c);
        let ohms = class.is_resistor().then(|| parse_ohms(&c.value)).flatten();
        let ic = is_active_ic(c);
        let card = if ic { sensing_card(c) } else { None };
        let inputs = c
            .pins
            .iter()
            .filter(|p| {
                (ic && sense_name(&p.function))
                    || card
                        .as_ref()
                        .and_then(|roles| roles.get(p.number.trim()))
                        .is_some_and(|role| sense_name(role))
            })
            .map(|p| p.number.as_str())
            .collect();
        Self {
            c,
            class,
            ohms,
            inputs,
        }
    }

    fn is_input(&self, pin: &Pin) -> bool {
        self.inputs.contains(pin.number.as_str())
    }
}

type PadKey = (String, String);

/// A sense connection on one of a shunt's nets.
struct Tap {
    pad: PadKey,
    /// The part whose measurement input it feeds.
    receiver: String,
}

/// Kelvin-sense findings for a `KiCad` layout.
///
/// A shunt is a resistor of at most [`MAX_SHUNT_OHMS`] with a sense connection
/// on each pad's net feeding one part (a differential measurement), or on one
/// pad's net with the other pad on ground (a ground-referenced one). A voltage
/// monitor tapping a rail that happens to pass through a resistor is neither.
pub fn kelvin_sense(board: &ExtractedBoard, text: &str, out: &mut Vec<LintFinding>) {
    let parts: Vec<Part> = board
        .components
        .iter()
        .filter(|c| !c.dnp)
        .map(Part::of)
        .collect();
    let net_name = |id: i64| {
        board
            .nets
            .iter()
            .find(|n| n.id == id)
            .map(|n| n.name.clone())
    };
    for shunt in &parts {
        let Some(r_shunt) = shunt.ohms.filter(|r| *r > 0.0 && *r <= MAX_SHUNT_OHMS) else {
            continue;
        };
        let nets: Vec<(i64, String, &str)> = shunt
            .c
            .pins
            .iter()
            .filter_map(|p| Some((p.net?, net_name(p.net?)?, p.number.as_str())))
            .collect();
        let [a, b] = nets.as_slice() else { continue };
        if a.0 == b.0 {
            continue;
        }
        let taps = [
            taps_on(&parts, shunt, a.0, b.0),
            taps_on(&parts, shunt, b.0, a.0),
        ];
        for (side, other, (own, theirs)) in [(a, b, (0, 1)), (b, a, (1, 0))] {
            let (net, name, pad) = (side.0, &side.1, side.2);
            let referenced = is_ground(&other.1) || is_ground(name);
            let judged: Vec<&Tap> = taps[own]
                .iter()
                .filter(|t| referenced || taps[theirs].iter().any(|u| u.receiver == t.receiver))
                .collect();
            if judged.is_empty() {
                continue;
            }
            let loads = loads_on(&parts, shunt, net, &taps[own]);
            if loads.is_empty() {
                continue;
            }
            let mut keys: Vec<PadKey> = judged.iter().map(|t| t.pad.clone()).collect();
            keys.sort();
            keys.dedup();
            let key = (shunt.c.reference.clone(), pad.to_string());
            let Some(verdicts) = kelvin_taps_from_text(text, name, &key, &keys, &loads) else {
                continue;
            };
            // A pad with any Kelvin tap is judged wired for it; otherwise the
            // tap whose shared copper is least speaks for the pad.
            let worst = keys
                .iter()
                .zip(&verdicts)
                .map(|(tap, verdict)| match verdict {
                    TapCopper::Shared {
                        ohms,
                        distance_mm,
                        through_pour,
                    } => Some((tap, *ohms, *distance_mm, *through_pour)),
                    TapCopper::Dedicated => None,
                })
                .collect::<Option<Vec<_>>>()
                .and_then(|shared| shared.into_iter().min_by(|x, y| x.1.total_cmp(&y.1)));
            let Some((tap, ohms, distance, pour)) = worst else {
                continue;
            };
            let fraction = ohms / r_shunt;
            if fraction <= REPORT_FRACTION {
                continue;
            }
            out.push(LintFinding {
                check: LintCheck::KelvinSense,
                severity: Severity::Medium,
                message: format!(
                    "current-sense shunt {} ({}): the sense connection {}.{} on net '{name}' reaches the shunt's pad {pad} only through {} that also carries the load current, joining it about {distance:.1} mm from the pad. That shared copper is roughly {:.2} mΩ (0.5 mΩ per square at 1 oz), {:.1}% of the shunt, so the measured current reads about {:.1}% high at full load. Run a sense track of its own from the shunt pad itself (a Kelvin connection)",
                    shunt.c.reference,
                    shunt.c.value,
                    tap.0,
                    tap.1,
                    if pour { "a copper pour" } else { "track copper" },
                    ohms * 1e3,
                    fraction * 100.0,
                    fraction * 100.0,
                ),
                refs: vec![shunt.c.reference.clone(), tap.0.clone()],
                nets: vec![name.clone()],
            });
        }
    }
}

/// The sense connections on a shunt's net `net` (its other net is `other`).
///
/// A measurement input on the net, or a series resistor of at least
/// [`MIN_TAP_OHMS`] whose far end is on a net with one.
fn taps_on(parts: &[Part], shunt: &Part, net: i64, other: i64) -> Vec<Tap> {
    let mut taps: Vec<Tap> = Vec::new();
    for part in parts.iter().filter(|p| !std::ptr::eq(*p, shunt)) {
        for pin in part.c.pins.iter().filter(|p| p.net == Some(net)) {
            let pad = (part.c.reference.clone(), pin.number.clone());
            if part.is_input(pin) {
                taps.push(Tap {
                    pad,
                    receiver: part.c.reference.clone(),
                });
                continue;
            }
            if !part.ohms.is_some_and(|r| r >= MIN_TAP_OHMS) {
                continue;
            }
            let far: BTreeSet<i64> = part
                .c
                .pins
                .iter()
                .filter_map(|p| p.net)
                .filter(|&n| n != net && n != other)
                .collect();
            for receiver in parts.iter().filter(|r| {
                r.c.pins
                    .iter()
                    .any(|p| r.is_input(p) && p.net.is_some_and(|n| far.contains(&n)))
            }) {
                taps.push(Tap {
                    pad: pad.clone(),
                    receiver: receiver.c.reference.clone(),
                });
            }
        }
    }
    taps
}

/// The pads on `net` the shunt's current flows to or from: every pad but the
/// shunt's, the sense connections, capacitors and resistors of at least
/// [`MIN_TAP_OHMS`] (which carry no load current).
fn loads_on(parts: &[Part], shunt: &Part, net: i64, taps: &[Tap]) -> Vec<PadKey> {
    let mut loads: Vec<PadKey> = parts
        .iter()
        .filter(|p| !std::ptr::eq(*p, shunt))
        .filter(|p| !p.class.is_capacitor() && !p.ohms.is_some_and(|r| r >= MIN_TAP_OHMS))
        .flat_map(|p| {
            p.c.pins
                .iter()
                .filter(|pin| pin.net == Some(net))
                .map(|pin| (p.c.reference.clone(), pin.number.clone()))
        })
        .filter(|pad| !taps.iter().any(|t| t.pad == *pad))
        .collect();
    loads.sort();
    loads.dedup();
    loads
}

#[cfg(test)]
mod tests {
    use super::sense_name;

    #[test]
    fn measurement_inputs_are_named_as_parts_name_them() {
        for name in [
            "IN+",
            "-IN",
            "1IN+",
            "in_plus_a",
            "+INA",
            "PA0/ADC0",
            "AIN3",
            "VIN-",
            "RS+",
        ] {
            assert!(sense_name(name), "{name}");
        }
        for name in ["V+", "VCC", "OUT", "GND", "EN", "MAIN", "ADCREF", ""] {
            assert!(!sense_name(name), "{name}");
        }
    }
}
