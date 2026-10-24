//! Sibling-topology diff: one instance of a repeated cell wired differently
//! from its twins.
//!
//! A board that stamps the same subcircuit many times (a mirror cell, an LED
//! driver channel, a sensor tile) has one right wiring and many copies of it.
//! A copy that deviates is almost never a design choice: it is the one cell
//! where a pin swap, a wrong net label or a mirrored footprint slipped in, and
//! it passes every per-part check because each part, taken alone, is fine.
//!
//! ## Evidence the check rests on
//!
//! - **Cell anchors** are parts with at least three connected pads (a
//!   transistor, a dual transistor, a switch, a logic or analog IC) that share
//!   one symbol, value and footprint. Two-terminal passives are never anchors:
//!   a pull-up resistor repeated on a dozen unrelated buses is not a cell.
//! - A pad's **wiring signature** is what its net touches, with the instance
//!   erased: a rail or ground by name, otherwise the set of neighbouring parts'
//!   symbols and pad numbers (two-terminal passives by class only, so a
//!   different value or an R/C pad swap is not a deviation). A net with twenty
//!   or more members reads as a bus.
//! - A group is a **repeated cell** only when at least `MIN_CELLS` anchors
//!   exist and one whole-part signature covers at least 85 % of them. Several
//!   well-populated variants (two cell types, odd and even channels) fail
//!   that test and the check stays silent.
//! - An **outlier** differs from the majority on at least two pads, which is
//!   what a swap or a mis-assigned pair looks like. A lone differing pad is
//!   what a chain end or a unique enable looks like, so it is not reported.
//!
//! A different component value, a different instance number and a low repeat
//! count never fire. When two outlier pads carry each other's majority wiring
//! the message says they look swapped.
//!
//! ## Systemic variants
//!
//! A swap made once in a library symbol is copied into every instance, so no
//! whole part is an outlier. The same evidence is read per transistor unit:
//! a (base, collector, emitter) wiring class used by a minority of the units
//! of one part type is reported when exchanging two of its terminals gives a
//! class that at least `MIN_CELLS` units, and at least twice as many, use.
//! That is the shape of a base and collector transposed in one symbol, and it
//! is reported once with the affected parts listed.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;

use hauksbee_extract::assembly::AssemblyState;
use hauksbee_extract::{
    is_capacitor, is_connector_like, is_ground, is_plain_inductor, is_plain_resistor,
    is_test_point_or_hole, is_unconnected_net, Component, LintCheck, LintFinding, Severity,
};
use hauksbee_models::ComponentKind;

use super::node_drive::{bjt_units, Index};

/// Fewest identical parts that make a repeated cell.
const MIN_CELLS: usize = 8;
/// Share of a group the majority signature must cover.
const MAJORITY: f64 = 0.85;
/// A net with this many members is a bus, not a local connection.
const BUS_FANOUT: usize = 20;
/// Parts whose nets are listed in one finding.
const MAX_REFS: usize = 12;

type Signature = BTreeMap<String, String>;

fn pads(c: &Component) -> BTreeMap<&str, Option<i64>> {
    let mut out: BTreeMap<&str, Option<i64>> = BTreeMap::new();
    for p in &c.pins {
        let slot = out.entry(p.number.as_str()).or_insert(None);
        *slot = slot.or(p.net.filter(|&n| n != 0));
    }
    out
}

/// How one neighbouring pad reads in a signature: two-terminal passives by
/// class only (a different value or an R/C pad swap is not a deviation).
fn descriptor(c: &Component, pad: &str) -> String {
    if is_plain_resistor(c) {
        "R".into()
    } else if is_capacitor(c) {
        "C".into()
    } else if is_plain_inductor(c) {
        "L".into()
    } else {
        format!("{}.{pad}", c.lib_id)
    }
}

fn is_rail_name(name: &str) -> bool {
    is_ground(name) || hauksbee_models::netclass::is_supply(name)
}

/// What a net touches, with the instance erased.
fn net_class(idx: &Index<'_>, part: &Component, net: Option<i64>) -> String {
    let Some(net) = net.filter(|&n| n != 0) else {
        return "open".into();
    };
    let name = idx.name(net);
    if is_unconnected_net(name) {
        return "open".into();
    }
    if is_rail_name(name) {
        return format!("rail {}", hauksbee_extract::norm(name));
    }
    let members = idx.members(net);
    if members.len() >= BUS_FANOUT {
        return "bus".into();
    }
    let others: BTreeSet<String> = members
        .iter()
        .filter(|(c, _)| c.reference != part.reference)
        .map(|(c, p)| descriptor(c, &p.number))
        .collect();
    if others.is_empty() {
        "alone".into()
    } else {
        others.into_iter().collect::<Vec<_>>().join(" | ")
    }
}

fn signature(idx: &Index<'_>, part: &Component) -> Signature {
    pads(part)
        .into_iter()
        .map(|(pad, net)| (pad.to_string(), net_class(idx, part, net)))
        .collect()
}

/// Pads where two signatures differ, in pad order.
fn differing<'a>(a: &'a Signature, b: &'a Signature) -> Vec<&'a str> {
    a.iter()
        .filter(|(pad, class)| b.get(*pad) != Some(*class))
        .map(|(pad, _)| pad.as_str())
        .collect()
}

/// Flag instances of a repeated cell wired differently from their twins.
pub(super) fn sibling_findings(idx: &Index<'_>) -> Vec<LintFinding> {
    let mut out = outlier_parts(idx);
    let flagged: BTreeSet<String> = out.iter().flat_map(|f| f.refs.clone()).collect();
    out.extend(swapped_units(idx, &flagged));
    out
}

fn outlier_parts(idx: &Index<'_>) -> Vec<LintFinding> {
    let mut groups: BTreeMap<(&str, &str, &str), Vec<&Component>> = BTreeMap::new();
    for c in &idx.board.components {
        let anchor = AssemblyState::of(c).is_present()
            && !is_test_point_or_hole(c)
            && !is_connector_like(c)
            && pads(c).values().filter(|n| n.is_some()).count() >= 3;
        if anchor {
            groups
                .entry((c.lib_id.trim(), c.value.trim(), c.footprint.trim()))
                .or_default()
                .push(c);
        }
    }
    let mut out = Vec::new();
    for members in groups.values().filter(|m| m.len() >= MIN_CELLS) {
        let sigs: Vec<Signature> = members.iter().map(|c| signature(idx, c)).collect();
        let mut tally: HashMap<&Signature, usize> = HashMap::new();
        for s in &sigs {
            *tally.entry(s).or_default() += 1;
        }
        let Some((majority, count)) = tally.into_iter().max_by(|a, b| {
            a.1.cmp(&b.1)
                // Equal counts: the lexicographically smaller signature wins, so the
                // verdict never depends on hash order.
                .then_with(|| b.0.cmp(a.0))
        }) else {
            continue;
        };
        #[allow(clippy::cast_precision_loss)] // part counts, far below 2^52
        if (count as f64) < MAJORITY * members.len() as f64 {
            continue;
        }
        for (comp, sig) in members.iter().zip(&sigs) {
            let diff = differing(sig, majority);
            if diff.len() >= 2 {
                out.push(part_finding(idx, comp, sig, majority, &diff, count));
            }
        }
    }
    out
}

fn part_finding(
    idx: &Index<'_>,
    comp: &Component,
    sig: &Signature,
    majority: &Signature,
    diff: &[&str],
    twins: usize,
) -> LintFinding {
    let label = |pad: &str| {
        comp.pins
            .iter()
            .find(|p| p.number == pad && !p.function.trim().is_empty())
            .map_or_else(
                || format!("pad {pad}"),
                |p| format!("pad {pad} ({})", p.function.trim()),
            )
    };
    let swapped: Vec<(&str, &str)> = diff
        .iter()
        .enumerate()
        .flat_map(|(n, a)| diff[n + 1..].iter().map(move |b| (*a, *b)))
        .filter(|(a, b)| sig[*a] == majority[*b] && sig[*b] == majority[*a])
        .collect();
    let detail = diff
        .iter()
        .map(|pad| {
            format!(
                "{} is on [{}] where its twins have [{}]",
                label(pad),
                sig[*pad],
                majority[*pad]
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    let swap_note = swapped.iter().fold(String::new(), |mut note, (a, b)| {
        let _ = write!(note, " {} and {} look swapped.", label(a), label(b));
        note
    });
    let mut nets: Vec<String> = comp
        .pins
        .iter()
        .filter(|p| diff.contains(&p.number.as_str()))
        .filter_map(|p| p.net.filter(|&n| n != 0))
        .map(|n| idx.name(n).to_string())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    nets.truncate(8);
    LintFinding {
        check: LintCheck::SiblingMismatch,
        severity: Severity::Medium,
        message: format!(
            "{} {} is wired differently from its {twins} twins of the same repeated cell: {detail}.{swap_note} \
             A lone deviating instance is usually a pin swap or a mislabelled net, not a design choice; \
             compare it with a neighbouring cell.",
            comp.reference, comp.value
        ),
        refs: vec![comp.reference.clone()],
        nets,
    }
}

/// One transistor unit's wiring class: what its base, collector and emitter
/// touch.
type UnitClass = [String; 3];

const TERMINALS: [&str; 3] = ["base", "collector", "emitter"];

/// A transistor unit with its label, part and wiring class.
type UnitRow<'a> = (String, &'a Component, UnitClass);

fn exchange(class: &UnitClass, a: usize, b: usize) -> UnitClass {
    let mut out = class.clone();
    out.swap(a, b);
    out
}

/// Transistor units of a minority wiring class whose terminal-exchanged twin
/// class is the established one for that part type.
fn swapped_units(idx: &Index<'_>, skip: &BTreeSet<String>) -> Vec<LintFinding> {
    let mut groups: BTreeMap<(&str, &str, &str), Vec<UnitRow<'_>>> = BTreeMap::new();
    for c in &idx.board.components {
        let bjt = idx
            .model(c)
            .is_some_and(|m| matches!(m.kind, ComponentKind::BjtNpn | ComponentKind::BjtPnp));
        if !bjt || !AssemblyState::of(c).is_present() {
            continue;
        }
        for u in bjt_units(idx, c) {
            let class = [u.base, u.collector, u.emitter].map(|p| net_class(idx, c, p.net));
            groups
                .entry((c.lib_id.trim(), c.value.trim(), c.footprint.trim()))
                .or_default()
                .push((u.label, c, class));
        }
    }
    let mut out = Vec::new();
    for units in groups.values() {
        let mut tally: BTreeMap<&UnitClass, usize> = BTreeMap::new();
        for (_, _, class) in units {
            *tally.entry(class).or_default() += 1;
        }
        for (&minority, &n) in &tally {
            let Some((a, b, twin, twin_n)) = [(0, 1), (0, 2), (1, 2)]
                .into_iter()
                .filter(|&(a, b)| minority[a] != minority[b])
                .filter_map(|(a, b)| {
                    let other = exchange(minority, a, b);
                    tally
                        .get_key_value(&other)
                        .map(|(k, &m)| (a, b, (*k).clone(), m))
                })
                .filter(|&(_, _, _, m)| m >= MIN_CELLS && m >= 2 * n)
                .max_by_key(|&(_, _, _, m)| m)
            else {
                continue;
            };
            let hit: Vec<&UnitRow<'_>> = units
                .iter()
                .filter(|(_, c, class)| class == minority && !skip.contains(&c.reference))
                .collect();
            if !hit.is_empty() {
                out.push(unit_finding(idx, &hit, minority, &twin, (a, b), twin_n));
            }
        }
    }
    out
}

fn unit_finding(
    idx: &Index<'_>,
    hit: &[&UnitRow<'_>],
    class: &UnitClass,
    twin: &UnitClass,
    (a, b): (usize, usize),
    twin_n: usize,
) -> LintFinding {
    let labels: Vec<&str> = hit.iter().map(|(l, _, _)| l.as_str()).collect();
    let shown = labels[..labels.len().min(4)].join(", ");
    let more = if labels.len() > 4 {
        format!(" and {} more", labels.len() - 4)
    } else {
        String::new()
    };
    let parts: BTreeSet<&str> = hit.iter().map(|(_, c, _)| c.reference.as_str()).collect();
    let mut nets = BTreeSet::new();
    for (_, c, _) in hit.iter().take(MAX_REFS) {
        for u in bjt_units(idx, c) {
            for p in [u.base, u.collector, u.emitter] {
                if let Some(n) = p.net.filter(|&n| n != 0) {
                    nets.insert(idx.name(n).to_string());
                }
            }
        }
    }
    let wiring =
        |c: &UnitClass| format!("base [{}], collector [{}], emitter [{}]", c[0], c[1], c[2]);
    LintFinding {
        check: LintCheck::SiblingMismatch,
        severity: Severity::Medium,
        message: format!(
            "{} transistor unit(s) ({shown}{more}) of one part type have {}, while {twin_n} other units of \
             the same type have {}: the {} and {} look swapped. A pin assignment copied into every \
             instance (a symbol or footprint pin-map error) passes each per-part check; compare the pad \
             functions with the datasheet.",
            labels.len(),
            wiring(class),
            wiring(twin),
            TERMINALS[a],
            TERMINALS[b],
        ),
        refs: parts.into_iter().map(str::to_string).collect(),
        nets: nets.into_iter().take(8).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{board, pin_fn};
    use hauksbee_extract::ExtractedBoard;
    use hauksbee_models::ModelLibrary;

    /// A dual transistor cell: B2 is wired to a private `drive` net and C2 to
    /// a private `load` net, unless `swap` crosses them. E1/E2 sit on GND.
    fn dual(n: i64, swap: bool) -> Component {
        let (b2, c2) = if swap {
            (200 + n, 100 + n)
        } else {
            (100 + n, 200 + n)
        };
        Component {
            reference: format!("IC{n}"),
            value: "BCM847BS".into(),
            lib_id: "BCM857BS-7-F".into(),
            footprint: "SOT-363".into(),
            pins: vec![
                pin_fn("1", 1, "E1"),
                pin_fn("2", 300 + n, "B1"),
                pin_fn("3", c2, "C2"),
                pin_fn("4", 1, "E2"),
                pin_fn("5", b2, "B2"),
                pin_fn("6", 400 + n, "C1"),
            ],
            ..Default::default()
        }
    }

    /// The parts each dual's private nets reach: `drive` meets a switch pad,
    /// `load` meets a resistor. Nets 300+/400+ meet a resistor each.
    fn cell_parts(n: i64) -> Vec<Component> {
        let two = |r: String, a: i64, b: i64| Component {
            reference: r,
            value: "47".into(),
            lib_id: "Device:R".into(),
            footprint: "R_0402".into(),
            pins: vec![pin_fn("1", a, ""), pin_fn("2", b, "")],
            ..Default::default()
        };
        vec![
            Component {
                reference: format!("SW{n}"),
                value: "3157".into(),
                lib_id: "Switch:SPDT".into(),
                footprint: "SOT-23-6".into(),
                pins: vec![
                    pin_fn("4", 100 + n, "A"),
                    pin_fn("2", 1, "GND"),
                    pin_fn("5", 2, "VCC"),
                ],
                ..Default::default()
            },
            two(format!("R{n}"), 200 + n, 3),
            two(format!("RB{n}"), 300 + n, 3),
            two(format!("RC{n}"), 400 + n, 3),
        ]
    }

    fn cells(count: i64, swapped: &[i64]) -> ExtractedBoard {
        let mut parts = Vec::new();
        for n in 1..=count {
            parts.push(dual(n, swapped.contains(&n)));
            parts.extend(cell_parts(n));
        }
        let mut nets = vec![(1, "GND"), (2, "+5V"), (3, "BIAS")];
        let private: Vec<(i64, String)> = (1..=count)
            .flat_map(|n| [100 + n, 200 + n, 300 + n, 400 + n])
            .map(|id| (id, format!("Net-{id}")))
            .collect();
        nets.extend(private.iter().map(|(id, n)| (*id, n.as_str())));
        board("cells", &nets, parts)
    }

    fn lint(b: &ExtractedBoard) -> Vec<LintFinding> {
        let lib = ModelLibrary::builtin();
        sibling_findings(&Index::new(b, &lib))
    }

    #[test]
    fn a_dual_with_two_pads_swapped_against_its_twins_is_named() {
        let found = lint(&cells(12, &[7]));
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].refs, ["IC7"]);
        assert_eq!(found[0].severity, Severity::Medium);
        assert!(
            found[0].message.contains("11 twins"),
            "{}",
            found[0].message
        );
        assert!(
            found[0].message.contains("pad 3 (C2)") && found[0].message.contains("look swapped"),
            "{}",
            found[0].message
        );
    }

    #[test]
    fn identical_cells_and_a_low_repeat_count_stay_silent() {
        assert!(lint(&cells(12, &[])).is_empty());
        // The same swap in a board with too few cells is not a pattern.
        assert!(lint(&cells(7, &[3])).is_empty());
    }

    #[test]
    fn two_equal_variants_and_value_only_differences_are_not_deviations() {
        // Half the cells swapped: two well-populated variants, no majority.
        assert!(lint(&cells(12, &[1, 2, 3, 4, 5, 6])).is_empty());
        // A different resistor value on one cell changes nothing structural.
        let mut b = cells(12, &[]);
        for c in &mut b.components {
            if c.reference == "R4" {
                c.value = "10k".into();
            }
        }
        assert!(lint(&b).is_empty());
    }

    #[test]
    fn a_swap_shared_by_a_minority_of_cells_is_one_finding_naming_the_units() {
        // Three of twelve duals carry the same base/collector transposition:
        // no whole-part majority, but the swapped unit class is the exchange
        // of the one nine other units use.
        let found = lint(&cells(12, &[2, 5, 9]));
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].refs, ["IC2", "IC5", "IC9"]);
        assert!(
            found[0].message.contains("base and collector look swapped")
                && found[0].message.contains("IC2 Q2"),
            "{}",
            found[0].message
        );
        // Half swapped is two real variants, not a swap: silent.
        assert!(lint(&cells(12, &[1, 2, 3, 4, 5, 6])).is_empty());
    }
}
