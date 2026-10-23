//! A power FET wired backwards: its body diode bypasses the switch.
//!
//! A MOSFET is a switch only in one direction. Its body diode runs drain to
//! source in a P-channel part and source to drain in an N-channel one, and it
//! conducts whenever the supply side sits on its anode. Two board shapes put
//! the diode the wrong way round, and both read from topology and the card's
//! electrode roles alone (the binder's own resolver, so a KiCad pin function,
//! an Eagle pad named `G`/`D`/`S` and a bare SOT-23 pad number all resolve):
//!
//! - **A driven switch** (a gate held by an MCU pin or a driver): the supply
//!   side must be on the cathode. With the diode pointing from the supply to
//!   the load the load is powered through the diode with the gate off, so the
//!   rail can never be switched off.
//! - **Reverse-polarity protection** (a gate biased only by passives, so the
//!   FET is on): the drain must face the connector. A connector reached
//!   through a fuse, ferrite or other low-value series part counts, which the
//!   extract layer's direct-neighbour rule does not see.
//!
//! The supply side is the one that shows a source: a regulator output, a
//! battery or a power-output pin, then a connector, then (only when neither
//! side shows one) the one on a named rail. When the two sides show equal
//! evidence (a power-path FET between two sources, an ideal diode) the check
//! abstains. Long-form how-and-why: docs/how-and-why/hauksbee-engine/checks.md.

use std::collections::BTreeSet;

use hauksbee_bind::binder::role_nets;
use hauksbee_extract::{
    is_connector_like, is_ground, Component, ExtractedBoard, LintCheck, LintFinding, NetLintReport,
    Pin, Severity,
};
use hauksbee_models::refdes::RefClass;
use hauksbee_models::{ComponentKind, ModelLibrary};

use super::node_drive::{is_gpio_role, Index};
use super::regulator_enable::{pin_drives, resistor_ohms};

/// A series resistor at or below this reads as a wire between a connector
/// and the FET (a fuse substitute, a sense resistor), in ohms.
const WIRE_OHMS: f64 = 10.0;
/// Series hops walked from a net to find its source or connector.
const HOPS: usize = 2;

fn same(a: &Component, b: &Component) -> bool {
    a.reference == b.reference
}

/// A two-pin part that is a wire at DC for this question: a fuse, a ferrite
/// bead, an inductor or a resistor of a few ohms or less.
fn is_series_element(c: &Component) -> bool {
    if c.pins.len() != 2 {
        return false;
    }
    match RefClass::of(&c.reference) {
        RefClass::FerriteBead | RefClass::Inductor => true,
        RefClass::Resistor => resistor_ohms(c).is_some_and(|r| r <= WIRE_OHMS),
        _ => {
            let r = c.reference.to_ascii_uppercase();
            let lib = c.lib_id.to_ascii_lowercase();
            r.starts_with('F') && !r.starts_with("FB") && !r.starts_with("FID")
                || lib.contains("fuse")
                || lib.contains("polyfuse")
        }
    }
}

/// `net` and every net reached from it through up to [`HOPS`] series
/// elements, never crossing the FET `q`.
fn cluster(idx: &Index<'_>, net: i64, q: &Component) -> Vec<i64> {
    let mut seen = vec![net];
    // Ground is the reference every sense resistor and bead returns to, not a
    // source: walking out of it would lend it another rail's evidence.
    let mut frontier = if is_ground(idx.name(net)) {
        Vec::new()
    } else {
        vec![net]
    };
    for _ in 0..HOPS {
        let mut reached = Vec::new();
        for n in frontier {
            for (o, _) in idx.members(n) {
                if same(o, q) || !is_series_element(o) {
                    continue;
                }
                for far in o.pins.iter().filter_map(|p| p.net).filter(|id| *id != 0) {
                    if !seen.contains(&far) {
                        seen.push(far);
                        reached.push(far);
                    }
                }
            }
        }
        frontier = reached;
    }
    seen
}

/// How strongly the nets of a cluster show a power source: 2 for a regulator
/// output, a battery or a power-output pin, 1 for a connector, else 0.
fn source_tier(idx: &Index<'_>, nets: &[i64], q: &Component) -> u8 {
    let mut tier = 0;
    for &n in nets {
        if is_ground(idx.name(n)) {
            continue;
        }
        for (o, p) in idx.members(n) {
            if same(o, q) {
                continue;
            }
            let reg_out = idx.model(o).zip(idx.role(o, p)).is_some_and(|(m, r)| {
                m.kind == ComponentKind::Vreg && matches!(r, "out" | "vout" | "sw")
            });
            let r = o.reference.to_ascii_uppercase();
            if reg_out
                || p.kind.eq_ignore_ascii_case("power_out")
                || r.starts_with("BT")
                || r.starts_with("BAT")
            {
                tier = tier.max(2);
            } else if is_connector_like(o) {
                tier = tier.max(1);
            }
        }
    }
    tier
}

fn cluster_level(idx: &Index<'_>, nets: &[i64]) -> Option<f64> {
    nets.iter()
        .filter_map(|&n| idx.level(n))
        .filter(|v| *v > 0.0)
        .max_by(f64::total_cmp)
}

/// Which of the FET's two channel nets `first` and `second` is the supply side,
/// when the evidence is not a tie.
fn supply_side(idx: &Index<'_>, q: &Component, (first, second): (i64, i64)) -> Option<i64> {
    let (near_first, near_second) = (cluster(idx, first, q), cluster(idx, second, q));
    let tier_first = source_tier(idx, &near_first, q);
    let tier_second = source_tier(idx, &near_second, q);
    if tier_first != tier_second {
        return Some(if tier_first > tier_second {
            first
        } else {
            second
        });
    }
    let levels = (
        cluster_level(idx, &near_first),
        cluster_level(idx, &near_second),
    );
    match levels {
        (Some(volts_first), Some(volts_second)) if (volts_first - volts_second).abs() > 0.5 => {
            Some(if volts_first > volts_second {
                first
            } else {
                second
            })
        }
        (Some(_), None) if tier_first == 0 => Some(first),
        (None, Some(_)) if tier_first == 0 => Some(second),
        _ => None,
    }
}

/// A pin that drives its net: an output of any type, a GPIO, a logic output,
/// or the collector of a driver transistor.
fn drives(idx: &Index<'_>, o: &Component, p: &Pin) -> bool {
    if is_connector_like(o) {
        return false;
    }
    if p.kind.to_ascii_lowercase().contains("power") {
        return false;
    }
    if pin_drives(p) || p.kind.trim().eq_ignore_ascii_case("bidirectional") {
        return true;
    }
    let (Some(m), Some(role)) = (idx.model(o), idx.role(o, p)) else {
        return false;
    };
    match m.kind {
        ComponentKind::Mcu => is_gpio_role(role),
        ComponentKind::Digital | ComponentKind::ShiftRegister => {
            m.logic.outputs.iter().any(|x| x == role)
        }
        ComponentKind::BjtNpn
        | ComponentKind::BjtPnp
        | ComponentKind::Nmos
        | ComponentKind::Pmos => {
            matches!(role, "collector" | "drain")
        }
        _ => false,
    }
}

/// The first pin that drives the gate net, or one series gate resistor away.
fn gate_driver(idx: &Index<'_>, q: &Component, gate: i64) -> Option<String> {
    // A gate tied to a rail or ground is not switched by anything.
    if idx.level(gate).is_some() {
        return None;
    }
    let mut nets = vec![gate];
    for (o, _) in idx.members(gate) {
        if !same(o, q) && o.pins.len() == 2 && RefClass::of(&o.reference) == RefClass::Resistor {
            nets.extend(
                o.pins
                    .iter()
                    .filter_map(|p| p.net)
                    .filter(|n| *n != 0 && *n != gate && idx.level(*n).is_none()),
            );
        }
    }
    nets.iter().find_map(|&n| {
        idx.members(n)
            .iter()
            .find(|(o, p)| !same(o, q) && drives(idx, o, p))
            .map(|(o, p)| format!("{} pin {}", o.reference, p.number))
    })
}

/// A two-pin resistor, capacitor or diode: biases a node, never drives one.
fn is_bias_part(c: &Component) -> bool {
    c.pins.len() == 2
        && matches!(
            RefClass::of(&c.reference),
            RefClass::Resistor | RefClass::Capacitor | RefClass::Diode
        )
}

/// True when the gate net, and every net one bias part further out, holds
/// only bias parts (or a ground), ending on ground or the FET's own channel
/// nets: nothing drives the gate, so the FET sits permanently on.
fn gate_is_passive(idx: &Index<'_>, q: &Component, gate: i64, ends: [i64; 2]) -> bool {
    let stop = |n: i64| ends.contains(&n) || is_ground(idx.name(n));
    let mut frontier = vec![gate];
    let mut seen = vec![gate];
    for depth in 0..2 {
        let mut next = Vec::new();
        for net in frontier {
            let others: Vec<_> = idx
                .members(net)
                .iter()
                .filter(|(o, _)| !same(o, q))
                .collect();
            if depth == 0 && others.is_empty() && !is_ground(idx.name(net)) {
                return false;
            }
            if others.iter().any(|(o, _)| !is_bias_part(o)) {
                return false;
            }
            for (o, _) in others {
                for far in o.pins.iter().filter_map(|p| p.net).filter(|n| *n != 0) {
                    if !seen.contains(&far) && !stop(far) {
                        seen.push(far);
                        next.push(far);
                    }
                }
            }
        }
        frontier = next;
    }
    frontier.is_empty()
}

/// Whether a connector sits on the cluster of `net`.
fn reaches_connector(idx: &Index<'_>, net: i64, q: &Component) -> bool {
    cluster(idx, net, q).into_iter().any(|n| {
        idx.members(n)
            .iter()
            .any(|(o, _)| !same(o, q) && is_connector_like(o))
    })
}

/// A switching-converter power stage: an inductor sits on a channel net, so
/// the half-bridge's body diodes are part of the topology and the supply side
/// cannot be read from where the sources are.
fn on_inductor(idx: &Index<'_>, net: i64, q: &Component) -> bool {
    idx.members(net)
        .iter()
        .any(|(o, _)| !same(o, q) && RefClass::of(&o.reference) == RefClass::Inductor)
}

/// A second source feeds `net` through a diode (power-path OR-ing, an ideal
/// diode): the body diode conducting from the other side is the design.
fn diode_or_fed(idx: &Index<'_>, net: i64, q: &Component) -> bool {
    idx.members(net).iter().any(|(o, p)| {
        !same(o, q)
            && o.pins.len() == 2
            && RefClass::of(&o.reference) == RefClass::Diode
            && o.pins
                .iter()
                .filter(|x| !std::ptr::eq(*x, *p))
                .filter_map(|x| x.net)
                .filter(|&far| far != 0 && far != net && !is_ground(idx.name(far)))
                .any(|far| {
                    let near = cluster(idx, far, q);
                    source_tier(idx, &near, q) > 0 || cluster_level(idx, &near).is_some()
                })
    })
}

/// A resistor straight across the FET's channel (a pre-charge or soft-start
/// path): the part is not a bare protection element.
fn bypassed(idx: &Index<'_>, q: &Component, (s, d): (i64, i64)) -> bool {
    idx.members(s).iter().any(|(o, _)| {
        !same(o, q)
            && o.pins.len() == 2
            && RefClass::of(&o.reference) == RefClass::Resistor
            && o.pins.iter().any(|x| x.net == Some(d))
    })
}

const fn diode_note(pmos: bool) -> &'static str {
    if pmos {
        "drain to source"
    } else {
        "source to drain"
    }
}

/// Run the check.
#[must_use]
pub fn fet_orientation_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    let idx = Index::new(board, lib);
    let mut report = NetLintReport::default();
    let mut done = BTreeSet::new();
    for q in &board.components {
        let Some(model) = idx.model(q) else { continue };
        let pmos = match model.kind {
            ComponentKind::Pmos => true,
            ComponentKind::Nmos => false,
            _ => continue,
        };
        let roles = role_nets(q, model, lib.pin_rules());
        let (Some(&g), Some(&s), Some(&d)) =
            (roles.get("gate"), roles.get("source"), roles.get("drain"))
        else {
            continue;
        };
        if s == d || g == 0 || s == 0 || d == 0 || !done.insert(q.reference.clone()) {
            continue;
        }
        let name = |n: i64| idx.name(n).to_string();
        let anode = if pmos { d } else { s };
        let (anode_pin, cathode_pin) = if pmos {
            ("drain", "source")
        } else {
            ("source", "drain")
        };
        let channel = if pmos { "P" } else { "N" };
        if let Some(driver) = gate_driver(&idx, q, g) {
            // Driven switch: the supply must not sit on the diode's anode.
            let supply = supply_side(&idx, q, (s, d))
                .filter(|n| *n == anode && !is_ground(idx.name(*n)))
                .filter(|_| !on_inductor(&idx, s, q) && !on_inductor(&idx, d, q));
            if let Some(supply) = supply {
                let load = if supply == s { d } else { s };
                if diode_or_fed(&idx, load, q) {
                    continue;
                }
                report.findings.push(LintFinding {
                    check: LintCheck::PinFunction,
                    severity: Severity::High,
                    message: format!(
                        "{} is a {channel}-channel FET switched by {driver} but wired backwards: its {anode_pin} is on '{}', the supply side, and its {cathode_pin} on '{}'. The body diode ({}) then conducts from '{}' to '{}' whenever the supply is up, so '{}' stays powered with the gate off and the switch cannot turn it off. {}swap source and drain so the body diode points from the load back to the supply",
                        q.reference, name(supply), name(load), diode_note(pmos), name(supply), name(load), name(load), hauksbee_extract::REMEDY_MARKER
                    ),
                    refs: vec![q.reference.clone()],
                    nets: vec![name(s), name(d)],
                });
            } else if !pmos
                && is_ground(idx.name(d))
                && !is_ground(idx.name(s))
                && idx.level(s).is_none()
            {
                // A low-side N-FET with drain on ground: the diode clamps the
                // load node to ground whenever it is above it.
                report.findings.push(LintFinding {
                    check: LintCheck::PinFunction,
                    severity: Severity::Medium,
                    message: format!(
                        "{} is an N-channel FET switched by {driver} but its drain is on ground '{}' and its source on '{}': the body diode (source to drain) then holds '{}' within a diode drop of ground whatever the gate does. {}swap source and drain so the source is on ground",
                        q.reference, name(d), name(s), name(s), hauksbee_extract::REMEDY_MARKER
                    ),
                    refs: vec![q.reference.clone()],
                    nets: vec![name(s), name(d)],
                });
            }
            continue;
        }
        // Reverse-polarity protection: drain faces the connector.
        if gate_is_passive(&idx, q, g, [s, d])
            && reaches_connector(&idx, s, q)
            && !reaches_connector(&idx, d, q)
            && !bypassed(&idx, q, (s, d))
        {
            report.findings.push(LintFinding {
                check: LintCheck::PinFunction,
                severity: Severity::Medium,
                message: format!(
                    "{} is an always-on FET (its gate is biased only by passives), as used for reverse-polarity protection, but its source reaches the connector net '{}' (directly or through a fuse, bead or low-value resistor) and its drain '{}': the body diode then points from the load back to the input and a reversed supply is not blocked. {}swap source and drain so the drain faces the connector",
                    q.reference, name(s), name(d), hauksbee_extract::REMEDY_MARKER
                ),
                refs: vec![q.reference.clone()],
                nets: vec![name(s), name(d)],
            });
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{board, pin_fn};

    const GND: i64 = 1;
    const V5: i64 = 2;
    const SW: i64 = 10;
    const GATE: i64 = 11;

    fn part(reference: &str, value: &str, lib_id: &str, pins: &[(&str, i64)]) -> Component {
        Component {
            reference: reference.into(),
            value: value.into(),
            lib_id: lib_id.into(),
            footprint: "X".into(),
            pins: pins.iter().map(|(n, net)| pin_fn(n, *net, "")).collect(),
            ..Default::default()
        }
    }

    fn fet(value: &str, lib_id: &str, (s, d, g): (i64, i64, i64)) -> Component {
        let mut q = part("Q1", value, lib_id, &[]);
        q.pins = vec![
            pin_fn("1", g, "G"),
            pin_fn("2", s, "S"),
            pin_fn("3", d, "D"),
        ];
        q
    }

    /// A P-FET with `s` on the source net, `d` on the drain net.
    fn pfet(s: i64, d: i64, g: i64) -> Component {
        fet("AO3401A", "Device:Q_PMOS_GSD", (s, d, g))
    }

    fn nfet(s: i64, d: i64, g: i64) -> Component {
        fet("2N7002", "Device:Q_NMOS_GSD", (s, d, g))
    }

    fn header(net: i64) -> Component {
        part(
            "J1",
            "Conn",
            "Connector:Conn_01x02",
            &[("1", net), ("2", GND)],
        )
    }

    fn mcu(gate: i64) -> Component {
        part(
            "U2",
            "ATmega328P",
            "MCU:ATmega328P",
            &[("2", gate), ("7", V5), ("8", GND)],
        )
    }

    fn load(net: i64) -> Component {
        part("R2", "1k", "Device:R", &[("1", net), ("2", GND)])
    }

    fn lint(parts: Vec<Component>, extra: &[(i64, &str)]) -> Vec<LintFinding> {
        let mut nets = vec![(GND, "GND"), (V5, "+5V"), (SW, "SW_OUT"), (GATE, "GATE")];
        nets.extend_from_slice(extra);
        let b = board("t", &nets, parts);
        fet_orientation_lint(&b, &ModelLibrary::builtin()).findings
    }

    #[test]
    fn a_driven_high_side_pfet_with_drain_on_the_supply_cannot_switch_off() {
        let found = lint(
            vec![header(V5), pfet(SW, V5, GATE), mcu(GATE), load(SW)],
            &[],
        );
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].severity, Severity::High);
        assert_eq!(found[0].refs, ["Q1"]);
        assert!(
            found[0].message.contains("U2 pin 2")
                && found[0].message.contains("cannot turn it off"),
            "{}",
            found[0].message
        );
    }

    #[test]
    fn a_correctly_oriented_or_undriven_or_tied_fet_is_silent() {
        // Source on the supply, drain on the load: the diode points back at the supply.
        let ok = lint(
            vec![header(V5), pfet(V5, SW, GATE), mcu(GATE), load(SW)],
            &[],
        );
        assert!(ok.is_empty(), "{ok:#?}");
        // Reversed wiring, but nothing drives the gate and no connector
        // touches the source: not a switch and not protection.
        let undriven = lint(
            vec![
                header(V5),
                pfet(SW, V5, GATE),
                part("R1", "100k", "Device:R", &[("1", GATE), ("2", V5)]),
                load(SW),
            ],
            &[],
        );
        assert!(undriven.is_empty(), "{undriven:#?}");
        // A power path between two sources shows equal evidence on both
        // sides: the check abstains.
        let two_sources = lint(
            vec![
                header(V5),
                part(
                    "J2",
                    "Conn",
                    "Connector:Conn_01x02",
                    &[("1", SW), ("2", GND)],
                ),
                pfet(SW, V5, GATE),
                mcu(GATE),
            ],
            &[],
        );
        assert!(two_sources.is_empty(), "{two_sources:#?}");
    }

    #[test]
    fn a_driven_nfet_high_side_with_source_on_the_supply_is_flagged_and_the_good_one_is_not() {
        let bad = lint(
            vec![header(V5), nfet(V5, SW, GATE), mcu(GATE), load(SW)],
            &[],
        );
        assert_eq!(bad.len(), 1, "{bad:#?}");
        let good = lint(
            vec![header(V5), nfet(SW, V5, GATE), mcu(GATE), load(SW)],
            &[],
        );
        assert!(good.is_empty(), "{good:#?}");
        // A low-side N-FET (source on ground) is the ordinary orientation.
        let low = lint(
            vec![header(V5), nfet(GND, SW, GATE), mcu(GATE), load(SW)],
            &[],
        );
        assert!(low.is_empty(), "{low:#?}");
    }

    #[test]
    fn a_low_side_nfet_with_drain_on_ground_clamps_its_load() {
        let bad = lint(vec![nfet(SW, GND, GATE), mcu(GATE)], &[]);
        assert_eq!(bad.len(), 1, "{bad:#?}");
        assert_eq!(bad[0].severity, Severity::Medium);
    }

    const VIN: i64 = 20;
    const VFUSED: i64 = 21;
    const VPROT: i64 = 22;
    const RPP_GATE: i64 = 23;
    const RAILS: &[(i64, &str)] = &[
        (VIN, "VIN"),
        (VFUSED, "VIN_FUSED"),
        (VPROT, "VPROT"),
        (RPP_GATE, "RPPG"),
    ];

    fn rpp(s: i64, d: i64) -> Vec<Component> {
        vec![
            part(
                "J1",
                "Conn",
                "Connector:Conn_01x02",
                &[("1", VIN), ("2", GND)],
            ),
            part("F1", "1A", "Device:Fuse", &[("1", VIN), ("2", VFUSED)]),
            pfet(s, d, RPP_GATE),
            part("R1", "100k", "Device:R", &[("1", RPP_GATE), ("2", GND)]),
            part("R2", "1k", "Device:R", &[("1", VPROT), ("2", GND)]),
        ]
    }

    #[test]
    fn a_reverse_polarity_pfet_with_drain_and_source_swapped_behind_a_fuse_is_flagged() {
        let found = lint(rpp(VFUSED, VPROT), RAILS);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert!(
            found[0].message.contains("always-on FET"),
            "{}",
            found[0].message
        );
        assert_eq!(found[0].refs, ["Q1"]);
    }

    #[test]
    fn a_correct_reverse_polarity_pfet_behind_a_fuse_is_silent() {
        let found = lint(rpp(VPROT, VFUSED), RAILS);
        assert!(found.is_empty(), "{found:#?}");
    }

    const OTHER: i64 = 30;
    const SWNODE: i64 = 31;

    /// The shapes that read as reversed but are not: each stays silent, and
    /// the same wiring without the extra part is still flagged.
    #[test]
    fn converter_stages_or_ing_level_shifters_and_sense_returns_are_silent() {
        let extra: &[(i64, &str)] = &[(OTHER, "VBUS_B"), (SWNODE, "SWNODE")];
        let reversed = vec![header(V5), pfet(SW, V5, GATE), mcu(GATE), load(SW)];
        assert_eq!(lint(reversed.clone(), extra).len(), 1);
        // An inductor on a channel net: a converter half-bridge.
        let mut stage = reversed.clone();
        stage.push(part("L1", "10u", "Device:L", &[("1", SW), ("2", SWNODE)]));
        let found = lint(stage, extra);
        assert!(found.is_empty(), "inductor: {found:#?}");
        // A second source feeds the load net through a diode: power-path OR-ing.
        let mut or_ing = reversed;
        or_ing.push(part(
            "J2",
            "Conn",
            "Connector:Conn_01x02",
            &[("1", OTHER), ("2", GND)],
        ));
        or_ing.push(part("D1", "SS14", "Device:D", &[("1", OTHER), ("2", SW)]));
        let found = lint(or_ing, extra);
        assert!(found.is_empty(), "diode OR: {found:#?}");
        // A gate held by a regulator's power output is tied, not switched.
        let mut tied = vec![header(V5), pfet(SW, V5, GATE), load(SW)];
        let mut reg = part("U4", "LDO", "Regulator:LDO", &[("1", GATE)]);
        reg.pins[0].kind = "power_output".into();
        tied.push(reg);
        let found = lint(tied, extra);
        assert!(found.is_empty(), "tied gate: {found:#?}");
        // A sense resistor from ground to a supply does not make ground the
        // supply side of a low-side N-FET.
        let mut low = vec![header(V5), nfet(GND, SW, GATE), mcu(GATE), load(SW)];
        low.push(part("R9", "0.01", "Device:R", &[("1", GND), ("2", V5)]));
        let found = lint(low, extra);
        assert!(found.is_empty(), "sense return: {found:#?}");
    }

    #[test]
    fn a_resistor_across_the_channel_makes_it_a_soft_start_not_protection() {
        let mut parts = rpp(VFUSED, VPROT);
        parts.push(part("R9", "62", "Device:R", &[("1", VFUSED), ("2", VPROT)]));
        let found = lint(parts, RAILS);
        assert!(found.is_empty(), "{found:#?}");
    }
}
