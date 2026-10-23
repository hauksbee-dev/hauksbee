//! Which clock an MCU runs from, read off its oscillator pins.
//!
//! The pins are found by role and schematic name (`XTAL1`/`XTAL2`, `XIN`/
//! `XOUT`, `OSC_IN`/`OSC_OUT`, `OSC1`/`OSC2`, `XC1`/`XC2`, `XTAL_P`/`XTAL_N`),
//! never by part name; low-speed (RTC) oscillator pins are recognised and left
//! out, since they do not clock the core. The board then decides the verdict:
//! a fitted resonator across the pair (directly, or with one leg through a
//! series drive-limiting resistor), a fitted oscillator or driver on the
//! input, nothing external on a part whose card declares an internal oscillator,
//! or a visible fault (pin tied to a rail, pins shorted, a resonator leg that
//! does not reach the pin pair). Anything else stays unknown, never "valid".

use std::collections::HashMap;

use hauksbee_extract::{Component, ExtractedBoard, Pin};
use hauksbee_ir::NodeId;

/// What the board gives an MCU's core as a clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ClockKind {
    /// Not established: no oscillator pins identified, or parts the binder
    /// cannot read sit on them. Never counted as a valid clock.
    #[default]
    Unknown,
    /// A fitted crystal or resonator across the oscillator pin pair.
    Crystal,
    /// A fitted oscillator or other driver on the oscillator input pin.
    External,
    /// No external clock part and the card declares an internal oscillator. The
    /// part can run, but the firmware's or fuses' clock selection is not
    /// checked, so the evidence reports this as unknown, not valid.
    Internal,
    /// The part sits on a module whose card declares the clock source (a
    /// crystal soldered inside the module, cited to the vendor datasheet).
    /// The oscillator is not a board part and not on the module's header, so
    /// the pad map has no oscillator pins to read; the card is the evidence.
    Module,
    /// The board wires the clock pins so no clock can run.
    Broken,
}

/// The verdict and the sentence that justifies it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct McuClock {
    pub kind: ClockKind,
    pub detail: String,
    /// The fitted crystal's frequency, when the verdict is
    /// [`ClockKind::Crystal`] and its value field parses as one ("16MHz"; a
    /// bare part number does not). The AVR co-sim runs a crystal-fused core at
    /// this clock.
    pub crystal_hz: Option<u64>,
}

impl McuClock {
    /// An external crystal, resonator or oscillator is bound to the clock pins
    /// (what the Renode descriptor calls external-clock presence).
    #[must_use]
    pub const fn external_present(&self) -> bool {
        matches!(
            self.kind,
            ClockKind::Crystal | ClockKind::External | ClockKind::Module
        )
    }

    /// The clock a module card declares: `source` is the card's citation and
    /// `hz` the declared frequency.
    #[must_use]
    pub fn module(hz: Option<u64>, source: &str) -> Self {
        let rate = hz.map_or_else(String::new, |hz| {
            if hz % 1_000_000 == 0 {
                format!("{} MHz ", hz / 1_000_000)
            } else {
                format!("{hz} Hz ")
            }
        });
        Self {
            kind: ClockKind::Module,
            detail: format!(
                "module-internal {rate}clock source declared by the module card ({source})"
            ),
            crystal_hz: hz,
        }
    }

    const fn of(kind: ClockKind, detail: String) -> Self {
        Self {
            kind,
            detail,
            crystal_hz: None,
        }
    }
}

/// Board facts the classifier needs, supplied by the binder.
pub(crate) struct ClockBoard<'a> {
    pub board: &'a ExtractedBoard,
    pub node_of: &'a dyn Fn(Option<i64>) -> Option<NodeId>,
    /// Ground or a supply rail.
    pub is_rail: &'a dyn Fn(NodeId) -> bool,
    pub net_name: &'a dyn Fn(NodeId) -> String,
    /// Assembled with an established identity.
    pub fitted: &'a dyn Fn(&Component) -> bool,
    pub is_crystal: &'a dyn Fn(&Component) -> bool,
    /// A two-pad resistor, the series drive-limiting part some oscillators
    /// put between an oscillator pin and the crystal.
    pub is_resistor: &'a dyn Fn(&Component) -> bool,
}

/// `(is_output, is_low_speed)` for a pin whose role/name marks an oscillator pin.
fn orientation(text: &str) -> Option<(bool, bool)> {
    let lower = text.to_ascii_lowercase();
    let toks: Vec<&str> = lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect();
    let mut low = None;
    for (i, &t) in toks.iter().enumerate() {
        let next = toks.get(i + 1).copied();
        let hit = match (t, next) {
            ("xtal1" | "osc1" | "xin" | "oscin" | "xc1", _)
            | ("osc" | "hse" | "xtal", Some("in" | "p")) => Some((false, false)),
            ("xtal2" | "osc2" | "xout" | "oscout" | "xc2", _)
            | ("osc" | "hse" | "xtal", Some("out" | "n")) => Some((true, false)),
            ("tosc1" | "xl1" | "osc32in", _) | ("osc32" | "lse" | "xtal32", Some("in")) => {
                Some((false, true))
            }
            ("tosc2" | "xl2" | "osc32out", _) | ("osc32" | "lse" | "xtal32", Some("out")) => {
                Some((true, true))
            }
            _ => None,
        };
        match hit {
            Some((out, false)) => return Some((out, false)),
            Some(l) => low = Some(l),
            None => {}
        }
    }
    low
}

struct OscPin {
    label: String,
    node: Option<NodeId>,
}

/// Core-clock (not RTC) oscillator pins of `mcu`: (inputs, outputs).
fn main_oscillator_pins(
    mcu: &Component,
    pad_roles: &HashMap<String, String>,
    node_of: &dyn Fn(Option<i64>) -> Option<NodeId>,
) -> (Vec<OscPin>, Vec<OscPin>) {
    let (mut inputs, mut outputs) = (Vec::new(), Vec::new());
    for pin in &mcu.pins {
        let role = pad_roles.get(&pin.number).map_or("", String::as_str);
        let Some((out, false)) = orientation(&format!("{role} {}", pin.function)) else {
            continue;
        };
        let name = if pin.function.trim().is_empty() {
            role
        } else {
            pin.function.trim()
        };
        let osc = OscPin {
            label: format!("{name} (pad {})", pin.number),
            node: node_of(pin.net),
        };
        if out { &mut outputs } else { &mut inputs }.push(osc);
    }
    (inputs, outputs)
}

/// Pin kinds that drive a net.
fn drives(kind: &str) -> bool {
    matches!(
        kind.to_ascii_lowercase().as_str(),
        "output" | "bidirectional" | "tri_state" | "open_collector" | "open_emitter" | "power_out"
    )
}

/// Classify the clock `mcu` runs from. `internal` is the card's declared
/// internal oscillator, if any.
pub(crate) fn classify(
    cb: &ClockBoard<'_>,
    mcu: &Component,
    pad_roles: &HashMap<String, String>,
    internal: Option<&str>,
) -> McuClock {
    use ClockKind::{Broken, Crystal, External, Unknown};
    let (inputs, outputs) = main_oscillator_pins(mcu, pad_roles, cb.node_of);
    let Some(input) = inputs.first() else {
        return McuClock::of(Unknown, "no core oscillator pins identified".to_string());
    };
    let output = outputs.first();
    let (in_node, out_node) = (input.node, output.and_then(|o| o.node));
    let broken = |why: String| McuClock::of(Broken, why);
    let rail_of = |n: Option<NodeId>| n.filter(|&n| (cb.is_rail)(n));
    if let Some(n) = rail_of(in_node) {
        return broken(format!("{} is tied to {}", input.label, (cb.net_name)(n)));
    }
    if let (Some(a), Some(b), Some(o)) = (in_node, out_node, output) {
        if a == b {
            return broken(format!(
                "{} and {} are shorted together on {}",
                input.label,
                o.label,
                (cb.net_name)(a)
            ));
        }
    }

    // Assembled parts, other than this MCU, with a pin on a node.
    let on = |node: Option<NodeId>| -> Vec<(&Component, &Pin)> {
        let Some(node) = node else { return Vec::new() };
        cb.board
            .components
            .iter()
            .filter(|c| c.reference != mcu.reference && (cb.fitted)(c))
            .flat_map(|c| c.pins.iter().map(move |p| (c, p)))
            .filter(|(_, p)| (cb.node_of)(p.net) == Some(node))
            .collect()
    };
    let (on_in, on_out) = (on(in_node), on(out_node));
    let touches = |c: &Component, node| touches(cb, c, node);
    let crystals = crystals_on(cb, on_in.iter().chain(&on_out).map(|&(c, _)| c));
    let out_label = output.map_or("its other oscillator pin", |o| o.label.as_str());

    if let (Some(n), Some(c)) = (rail_of(out_node), crystals.first()) {
        return broken(format!(
            "{out_label} is tied to {} while {} sits on {}",
            (cb.net_name)(n),
            c.reference,
            input.label
        ));
    }
    if let Some(c) = crystals
        .iter()
        .find(|c| touches(c, in_node) && touches(c, out_node))
    {
        return McuClock {
            crystal_hz: crystal_hz(&c.value),
            ..McuClock::of(
                Crystal,
                format!(
                    "{} {} across {} and {out_label}",
                    c.reference, c.value, input.label
                ),
            )
        };
    }
    if let Some(clock) = crystal_through_series(
        cb,
        mcu,
        &crystals,
        (in_node, input.label.as_str()),
        (out_node, out_label),
    ) {
        return clock;
    }
    // Something that drives the input: a driving pin (clock buffer, other
    // MCU) or a powered 3+ pad crystal-footprint oscillator.
    let powered = |c: &Component| {
        c.pins
            .iter()
            .any(|p| (cb.node_of)(p.net).is_some_and(|n| !n.is_ground() && (cb.is_rail)(n)))
    };
    if let Some((c, p)) = on_in
        .iter()
        .find(|(c, p)| drives(&p.kind) || ((cb.is_crystal)(c) && c.pins.len() >= 3 && powered(c)))
    {
        return McuClock::of(
            External,
            format!("{} pad {} drives {}", c.reference, p.number, input.label),
        );
    }
    if let Some(c) = crystals.first() {
        let (here, missing) = if touches(c, in_node) {
            (input.label.as_str(), out_label)
        } else {
            (out_label, input.label.as_str())
        };
        return broken(format!(
            "{} reaches only {here}; its other leg is not on {missing}",
            c.reference
        ));
    }
    unsourced(
        &input.label,
        !on_in.is_empty() || !on_out.is_empty(),
        internal,
    )
}

/// A crystal's frequency from its value field, whole hertz.
fn crystal_hz(value: &str) -> Option<u64> {
    let hz = hauksbee_models::value::parse_frequency_hz(value)?;
    // A finite positive value well inside u64; whole hertz is the resolution
    // a core clock is stated at.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    (1.0..1e15).contains(&hz).then(|| hz.round() as u64)
}

/// Whether `c` has a pin on `node` (never, when there is no node).
fn touches(cb: &ClockBoard<'_>, c: &Component, node: Option<NodeId>) -> bool {
    node.is_some() && c.pins.iter().any(|p| (cb.node_of)(p.net) == node)
}

/// The distinct crystals among `parts`, in first-seen order.
fn crystals_on<'b>(
    cb: &ClockBoard<'_>,
    parts: impl Iterator<Item = &'b Component>,
) -> Vec<&'b Component> {
    let mut seen: Vec<&Component> = Vec::new();
    for c in parts {
        if (cb.is_crystal)(c) && !seen.iter().any(|s| s.reference == c.reference) {
            seen.push(c);
        }
    }
    seen
}

/// A crystal with one leg reaching its oscillator pin through a fitted series
/// resistor (the RP2040 hardware guide's 1 kOhm on XOUT, an STM32 HSE drive
/// limiter): the resistor's far pad sits on the crystal's other leg, off any
/// rail. `a` and `b` are the two oscillator pins, each a node and a label.
fn crystal_through_series(
    cb: &ClockBoard<'_>,
    mcu: &Component,
    crystals: &[&Component],
    a: (Option<NodeId>, &str),
    b: (Option<NodeId>, &str),
) -> Option<McuClock> {
    let touches = |c: &Component, node| touches(cb, c, node);
    let series = |pin: Option<NodeId>, other: Option<NodeId>| -> Option<(&Component, NodeId)> {
        let pin = pin?;
        cb.board.components.iter().find_map(|r| {
            if r.reference == mcu.reference || !(cb.fitted)(r) || !(cb.is_resistor)(r) {
                return None;
            }
            let nodes: Vec<Option<NodeId>> = r.pins.iter().map(|p| (cb.node_of)(p.net)).collect();
            let far = match nodes.as_slice() {
                [Some(a), Some(b)] if *a == pin && *b != pin => *b,
                [Some(a), Some(b)] if *b == pin && *a != pin => *a,
                _ => return None,
            };
            (Some(far) != other && !(cb.is_rail)(far)).then_some((r, far))
        })
    };
    [(a, b), (b, a)]
        .into_iter()
        .find_map(|((near, near_label), (far_pin, far_label))| {
            let (r, far) = series(far_pin, near)?;
            let c = crystals
                .iter()
                .find(|c| touches(c, near) && touches(c, Some(far)))?;
            Some(McuClock {
                crystal_hz: crystal_hz(&c.value),
                ..McuClock::of(
                    ClockKind::Crystal,
                    format!(
                        "{} {} across {near_label} and {far_label} (through series {} {})",
                        c.reference, c.value, r.reference, r.value
                    ),
                )
            })
        })
}

/// No crystal or oscillator found: parts that cannot be read leave the clock
/// unknown; bare pins fall back to the card's internal oscillator, if any.
fn unsourced(label: &str, parts_attached: bool, internal: Option<&str>) -> McuClock {
    if parts_attached {
        return McuClock::of(
            ClockKind::Unknown,
            format!("parts sit on {label} but no crystal or oscillator was recognised"),
        );
    }
    internal.map_or_else(
        || {
            McuClock::of(
                ClockKind::Unknown,
                format!("no external clock part on {label} and the card declares no internal oscillator"),
            )
        },
        |src| {
            McuClock::of(
                ClockKind::Internal,
                format!("{src}; no external clock part on {label}"),
            )
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oscillator_pin_names_are_read_by_role_not_by_part() {
        let main_in = Some((false, false));
        let main_out = Some((true, false));
        for t in [
            "XTAL1",
            "pb6_xtal1",
            "OSC_IN",
            "PH0-OSC_IN",
            "pf0_osc_in_adc10",
            "XIN",
            "OSC1",
            "XC1",
            "XTAL_P",
        ] {
            assert_eq!(orientation(t), main_in, "{t}");
        }
        for t in [
            "XTAL2",
            "osc_out",
            "PH1-OSC_OUT",
            "XOUT",
            "OSC2",
            "XC2",
            "XTAL_N",
        ] {
            assert_eq!(orientation(t), main_out, "{t}");
        }
        for t in ["PC14-OSC32_IN", "pc14_osc32_in", "TOSC1", "XL1", "LSE_IN"] {
            assert_eq!(orientation(t), Some((false, true)), "{t}");
        }
        for t in ["pc15_osc32_out", "TOSC2", "XL2"] {
            assert_eq!(orientation(t), Some((true, true)), "{t}");
        }
        for t in ["PA0", "NRST", "VDD", "oscillator_enable", ""] {
            assert_eq!(orientation(t), None, "{t}");
        }
    }

    #[test]
    fn crystal_frequency_is_read_from_its_value_field() {
        assert_eq!(crystal_hz("16MHz"), Some(16_000_000));
        assert_eq!(crystal_hz("8 MHz"), Some(8_000_000));
        assert_eq!(crystal_hz("32.768kHz"), Some(32_768));
        assert_eq!(crystal_hz("HC-49"), None);
        assert_eq!(crystal_hz(""), None);
    }
}
