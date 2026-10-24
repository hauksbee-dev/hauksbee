//! A switch or push button that shorts a supply when pressed.
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/checks.md.
//!
//! A two-position switch with nothing in series, closed between two nets that
//! each hold a level of their own, is a dead short while it is closed: two
//! regulators fight, a battery or USB bus is shorted to ground, or the switch
//! contacts and traces burn. The check reads the board's nets only:
//!
//! - **A rail and ground.** One terminal of the switch on a named positive
//!   supply (`+3V3`, `VBUS`, `5V`: a stated voltage, so a signal named after a
//!   rail, `3V3_EN`, is not one) and the other on ground. Judged only when the
//!   rail has a source on the board or at a connector, so a rail nothing feeds
//!   (a bare label) is not called short.
//! - **Two different rails.** Both terminals on named supplies of different
//!   stated voltage (more than 0.3 V apart), each with a source other than
//!   this switch. A rail whose only way in is through the switch (a battery
//!   switched onto the board's own rail, a power switch) is silent: that is
//!   the switch's job.
//!
//! Which nets a switch touches is decided from its pins' nets after the usual
//! reading of a 4-pin tactile switch (two pins to a side), so a part that
//! reaches three or more nets (a selector, a slide switch with a common
//! terminal, a DIP bank) is never judged; neither is a net the switch reaches
//! through a resistor, because net extraction never merges across a resistor
//! and the resistor is the series resistance this check asks for.
//!
//! A 4-pad tactile switch has its pads bonded in pairs inside the part (see
//! `bond_pairs`). Where the pairs are known (from placed pad geometry, or the
//! standard Omron numbering on a 6x6 or 12x12 footprint), a bonded pair on two
//! different nets is a switch that is always closed, and a switch with both
//! terminals on one net or one terminal unconnected never closes.
//!
//! A rail source is a pin typed `power_out`, a regulator card's `out` pad, or a
//! battery or connector pad on the rail: all of them put a level on the net
//! that the switch would short.
//!
//! The reset or strap line a switch serves with no pull resistor is the
//! `floating_inputs` check's (a control input whose only holder is a button
//! may float), not repeated here.

use std::collections::BTreeSet;

use hauksbee_extract::assembly::AssemblyState;
use hauksbee_extract::{
    is_connector_like, is_ground, is_test_point_or_hole, Component, ExtractedBoard, LintCheck,
    LintFinding, NetLintReport, Severity,
};
use hauksbee_models::netclass::{is_positive_supply, rail_voltage};
use hauksbee_models::{ComponentKind, ModelLibrary};

/// Rails closer than this are one level for the purposes of "different rails":
/// two names of the same supply (`+3V3` and `VDD3V3`) are not a short.
const DISTINCT_RAIL_V: f64 = 0.3;

/// A positive supply by a stated voltage, so a bare `VDD` of unknown voltage is
/// not judged (it may be any level, or be the same rail as its neighbour).
fn stated_rail(name: &str) -> Option<f64> {
    let v = rail_voltage(name)?;
    (is_positive_supply(name) && v > 0.0).then_some(v)
}

/// Whether `net` carries a level of its own, other than through `except`.
fn is_sourced(board: &ExtractedBoard, lib: &ModelLibrary, net: i64, except: &Component) -> bool {
    board.net_members(net).into_iter().any(|(o, q)| {
        if std::ptr::eq(o, except) {
            return false;
        }
        let Some(part) = AssemblyState::of(o).fitted() else {
            return false;
        };
        if q.kind.trim().eq_ignore_ascii_case("power_out") {
            return true;
        }
        if is_connector_like(o) && !is_test_point_or_hole(o) {
            return true;
        }
        if o.reference.to_ascii_uppercase().starts_with("BT") {
            return true;
        }
        hauksbee_bind::binder::resolve(lib, part)
            .model
            .is_some_and(|m| {
                m.kind == ComponentKind::Vreg
                    && m.pins
                        .get(&q.number)
                        .is_some_and(|r| r.eq_ignore_ascii_case("out"))
            })
    })
}

/// How a 4-pad tactile switch's internal bonds were learned.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BondSource {
    /// From the pads' placed positions: the layout's own geometry.
    Geometry,
    /// From the footprint's name, for a family numbered like the vendor drawing.
    Table,
}

/// The two internally bonded pad pairs of a 4-pad tactile switch, as indices
/// into `sw.pins`, and where that came from.
///
/// A 4-pad tact switch is one contact with two terminals on each side. The two
/// pads of a side are one conductor inside the part, so they are one net; the
/// contact closes between the two sides. On the standard packages (Omron B3F
/// 6x6 and 12x12 through-hole, p.4 and p.6, and B3S 6x6 SMD, p.2, whose
/// "Terminal Arrangement/Internal Connections" drawings join 4-3 and 2-1 across
/// the long pad spacing, 6.5 or 12.5 mm, and close between the rows 4.5 or
/// 5 mm apart) the bonded pads are the two on the same row of the long axis.
///
/// 1. **Geometry.** With every pad placed, the four pads form a rectangle with
///    unequal sides; the bonded pairs are the two pairs separated by the long
///    side. A square pattern is ambiguous and abstains.
/// 2. **Table.** With no positions, a footprint whose four pads are numbered
///    1 to 4 and which names a standard Omron family (`B3F`, `B3S`, `B3FS`) or a
///    6x6 or 12x12 tact size uses the datasheet numbering: pads 1-2 and 3-4
///    (the bottom-view drawing numbers each bonded row 2-1 and 4-3). KiCad's own
///    tact footprints repeat a pad number across the bonded pair (`SW_PUSH_6mm`
///    has two pads `1` and two `2`), so they are merged by the netlist already
///    and need no map.
fn bond_pairs(sw: &Component) -> Option<([[usize; 2]; 2], BondSource)> {
    let numbered: Vec<&str> = sw.pins.iter().map(|p| p.number.trim()).collect();
    let mut distinct = numbered.clone();
    distinct.sort_unstable();
    distinct.dedup();
    if sw.pins.len() != 4 || distinct.len() != 4 || distinct.contains(&"") {
        return None;
    }
    let at: Option<Vec<(f64, f64)>> = sw.pins.iter().map(|p| p.position).collect();
    // A schematic gives its pins schematic coordinates and no board side: only
    // a placed footprint's pads say how the part is built.
    if let Some(at) = at.filter(|_| !sw.layer.is_empty()) {
        return geometry_pairs(&at).map(|p| (p, BondSource::Geometry));
    }
    if !is_omron_numbered(sw) {
        return None;
    }
    let index = |n: &str| numbered.iter().position(|x| *x == n);
    Some((
        [[index("1")?, index("2")?], [index("3")?, index("4")?]],
        BondSource::Table,
    ))
}

/// Pair four points as the two pairs separated by the long side of their
/// rectangle; `None` when they are not a rectangle or are a square.
fn geometry_pairs(at: &[(f64, f64)]) -> Option<[[usize; 2]; 2]> {
    let d = |i: usize, j: usize| (at[i].0 - at[j].0).hypot(at[i].1 - at[j].1);
    let pairings = [[[0, 1], [2, 3]], [[0, 2], [1, 3]], [[0, 3], [1, 2]]];
    // Opposite sides of a rectangle are equal: two pairings have equal pair
    // lengths (its sides), the third is the pair of diagonals.
    let lens = pairings.map(|[a, b]| (d(a[0], a[1]), d(b[0], b[1])));
    let equal = |x: f64, y: f64| (x - y).abs() <= 0.05 * x.max(y).max(1e-9);
    let sides: Vec<usize> = (0..3).filter(|&k| equal(lens[k].0, lens[k].1)).collect();
    if sides.len() != 3 {
        return None;
    }
    let mut by_len = sides;
    by_len.sort_by(|&a, &b| lens[a].0.total_cmp(&lens[b].0));
    let [short, long, diagonal] = [by_len[0], by_len[1], by_len[2]];
    // A rectangle: both diagonals equal the longest pairing, and the two
    // sides differ.
    if equal(lens[short].0, lens[long].0)
        || !equal(lens[diagonal].0, lens[short].0.hypot(lens[long].0))
    {
        return None;
    }
    Some(pairings[long])
}

/// A tactile push button by its symbol, footprint or value, never a DIP, slide,
/// toggle, rocker or piano switch (those have four pads too, but each pair of
/// pads is its own contact, not one terminal of a single contact).
fn is_tact_switch(sw: &Component) -> bool {
    let blob = format!("{} {} {}", sw.footprint, sw.lib_id, sw.value).to_ascii_lowercase();
    let words: Vec<&str> = blob
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    let other = |w: &&str| {
        [
            "dip", "slide", "toggle", "rocker", "piano", "selector", "rotary",
        ]
        .iter()
        .any(|p| w.starts_with(p))
    };
    let tact = |w: &&str| {
        [
            "tact", "push", "button", "btn", "evq", "b3f", "b3s", "pts", "skqg", "skrp", "kmr",
            "tl3",
        ]
        .iter()
        .any(|p| w.starts_with(p))
    };
    !words.iter().any(other) && words.iter().any(tact)
}

/// A footprint, symbol or value naming a standard Omron-numbered tact family
/// or a 6x6 or 12x12 tact size.
fn is_omron_numbered(sw: &Component) -> bool {
    let blob = format!("{} {} {}", sw.footprint, sw.lib_id, sw.value).to_ascii_lowercase();
    blob.split(|c: char| !(c.is_ascii_alphanumeric() || c == '.'))
        .any(|w| {
            w.starts_with("b3f")
                || w.starts_with("b3s")
                || ["6x6", "6.0x6.0", "12x12", "12.0x12.0"]
                    .iter()
                    .any(|p| w.starts_with(p))
                || matches!(w, "6mm" | "12mm")
        })
}

/// A 4-pad tact switch wired against its internal bonds: a bonded pair across
/// two nets (always closed) or all four pads on one side of the contact
/// (never closes). `None` when the part is not a mappable 4-pad switch, or is
/// wired sensibly.
fn bond_finding(board: &ExtractedBoard, sw: &Component) -> Option<LintFinding> {
    if !is_tact_switch(sw) {
        return None;
    }
    let (pairs, source) = bond_pairs(sw)?;
    let name = |id: i64| board.net(id).map_or_else(String::new, |n| n.name.clone());
    let net = |i: usize| sw.pins[i].net.filter(|id| *id != 0);
    let pad = |i: usize| sw.pins[i].number.trim().to_string();
    let how = match source {
        BondSource::Geometry => {
            "the pads' placement (the two on one row of the long axis are one terminal)"
        }
        BondSource::Table => {
            "the standard footprint's numbering (pads 1-2 and 3-4 are each one terminal)"
        }
    };
    for [i, j] in pairs {
        if let (Some(x), Some(y)) = (net(i), net(j)) {
            if x != y {
                let (nx, ny) = (name(x), name(y));
                return Some(LintFinding {
                    check: LintCheck::SwitchShort,
                    severity: if source == BondSource::Geometry {
                        Severity::High
                    } else {
                        Severity::Medium
                    },
                    message: format!(
                        "{} is a 4-pad tactile switch whose pads {} and {} are one terminal inside the part ({how}), but they sit on different nets, '{nx}' and '{ny}': the switch is always closed, so those nets are permanently tied together whether or not it is pressed. Put the two nets on opposite terminals (one on each bonded pair), or re-check which pads the footprint joins",
                        sw.reference, pad(i), pad(j)
                    ),
                    refs: vec![sw.reference.clone()],
                    nets: vec![nx, ny],
                });
            }
        }
    }
    let side =
        |[i, j]: [usize; 2]| -> BTreeSet<i64> { [net(i), net(j)].into_iter().flatten().collect() };
    let (p, q) = (side(pairs[0]), side(pairs[1]));
    let one_side_open = p.is_empty() != q.is_empty();
    let one_net = p.len() == 1 && p == q;
    if one_side_open || one_net {
        let nets: Vec<String> = p.union(&q).map(|id| name(*id)).collect();
        let why = if one_side_open {
            "all its connected pads are on one terminal and the other terminal is not connected"
        } else {
            "both terminals are on the same net"
        };
        return Some(LintFinding {
            check: LintCheck::SwitchShort,
            severity: Severity::Medium,
            message: format!(
                "{} is a 4-pad tactile switch whose nets do not span its contact ({how}): {why}, so pressing it connects nothing and it never closes. Wire one net to each bonded pair of pads",
                sw.reference
            ),
            refs: vec![sw.reference.clone()],
            nets,
        });
    }
    None
}

/// Flag every two-terminal switch closed between two nets that each hold a level.
#[must_use]
pub fn switch_short_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    let mut report = NetLintReport::default();
    for sw in &board.components {
        if AssemblyState::of(sw).fitted().is_none() || !super::regulator_enable::is_switch(sw) {
            continue;
        }
        // A 4-pad tact switch is judged by its internal bonds first.
        if let Some(finding) = bond_finding(board, sw) {
            report.findings.push(finding);
            continue;
        }
        let nets: BTreeSet<i64> = sw
            .pins
            .iter()
            .filter_map(|p| p.net.filter(|id| *id != 0))
            .collect();
        let [a, b] = nets.iter().copied().collect::<Vec<_>>()[..] else {
            continue;
        };
        let name = |id: i64| board.net(id).map_or_else(String::new, |n| n.name.clone());
        let (na, nb) = (name(a), name(b));
        let message = if is_ground(&na) == is_ground(&nb) {
            let (Some(va), Some(vb)) = (stated_rail(&na), stated_rail(&nb)) else {
                continue;
            };
            if (va - vb).abs() < DISTINCT_RAIL_V {
                continue;
            }
            if !(is_sourced(board, lib, a, sw) && is_sourced(board, lib, b, sw)) {
                continue;
            }
            format!(
                "{} is a switch between the {va:.1} V rail '{na}' and the {vb:.1} V rail '{nb}', \
                 each fed from its own source, with nothing in series: pressed, it ties the two \
                 supplies together and they fight, so the higher rail sags into the lower or a \
                 regulator is damaged. Switch one rail's source (an enable or a load switch), or \
                 use a break-before-make selector, or put a resistor in series if only a sense \
                 level is wanted",
                sw.reference
            )
        } else {
            let (rail, ground) = if is_ground(&na) { (b, a) } else { (a, b) };
            let (n_rail, n_ground) = (name(rail), name(ground));
            let Some(v) = stated_rail(&n_rail) else {
                continue;
            };
            if !is_sourced(board, lib, rail, sw) {
                continue;
            }
            format!(
                "{} is a switch between the {v:.1} V rail '{n_rail}' and ground '{n_ground}' with \
                 nothing in series: pressed, it shorts the rail to ground, so its source collapses \
                 (a regulator trips or overheats, a battery or USB bus is shorted). If it is meant \
                 to pull a signal to ground, it must act on a signal pin that has its own pull-up \
                 resistor, not on a supply rail",
                sw.reference
            )
        };
        report.findings.push(LintFinding {
            check: LintCheck::SwitchShort,
            severity: Severity::High,
            message,
            refs: vec![sw.reference.clone()],
            nets: vec![na, nb],
        });
    }
    report
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{board, part};
    use super::*;

    const V5: i64 = 1;
    const V3: i64 = 2;
    const GND: i64 = 3;
    const SIG: i64 = 4;
    const SWITCHED: i64 = 5;
    const V3B: i64 = 6;

    const NETS: [(i64, &str); 6] = [
        (V5, "+5V"),
        (V3, "+3V3"),
        (GND, "GND"),
        (SIG, "/RESET"),
        (SWITCHED, "VBAT_SW"),
        (V3B, "VDD3V3"),
    ];

    fn source_on(net: i64, reference: &str) -> Component {
        let mut u = part(reference, "SomeReg", &[("1", net)]);
        u.pins[0].kind = "power_out".into();
        u
    }

    fn switch(reference: &str, pads: &[(&str, i64)]) -> Component {
        Component {
            lib_id: "Switch:SW_Push".into(),
            ..part(reference, "SW_Push", pads)
        }
    }

    fn run(parts: Vec<Component>) -> Vec<LintFinding> {
        switch_short_lint(&board(&NETS, parts), &ModelLibrary::builtin()).findings
    }

    #[test]
    fn a_button_across_a_sourced_rail_and_ground_is_a_short() {
        let found = run(vec![
            source_on(V3, "U1"),
            switch("SW1", &[("1", V3), ("2", GND)]),
        ]);
        assert_eq!(found.len(), 1, "{found:#?}");
        let f = &found[0];
        assert_eq!(f.check, LintCheck::SwitchShort);
        assert_eq!(f.severity, Severity::High);
        assert_eq!(f.refs, ["SW1"]);
        assert_eq!(f.nets, ["+3V3", "GND"]);
        assert!(
            f.message.contains("shorts the rail to ground"),
            "{}",
            f.message
        );
        // A four-pin tactile switch, two pins to a side, is the same button.
        let four = run(vec![
            source_on(V3, "U1"),
            switch("SW1", &[("1", V3), ("2", V3), ("3", GND), ("4", GND)]),
        ]);
        assert_eq!(four.len(), 1, "{four:#?}");
    }

    #[test]
    fn a_switch_across_two_sourced_rails_is_a_short() {
        let found = run(vec![
            source_on(V3, "U1"),
            source_on(V5, "U2"),
            switch("SW1", &[("1", V5), ("2", V3)]),
        ]);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert!(
            found[0].message.contains("ties the two supplies together"),
            "{}",
            found[0].message
        );
    }

    /// The reset or strap line a button serves must be held by a pull resistor
    /// when the part has none of its own: reported by the control-input checks,
    /// asserted here at the one surface every check shares.
    #[test]
    fn a_button_alone_on_a_reset_pin_with_no_internal_pull_is_flagged_and_a_pull_up_fixes_it() {
        // ESP32-WROOM-32 pad 3 is EN (no internal pull), pad 2 3V3, pad 1 GND.
        let module = || part("U1", "ESP32-WROOM-32", &[("1", GND), ("2", V3), ("3", SIG)]);
        let net_names = [(GND, "GND"), (V3, "+3V3"), (SIG, "EN")];
        let lib = ModelLibrary::builtin();
        let on_en = |parts: Vec<Component>| {
            let found = super::super::engine_lint(&board(&net_names, parts), &lib).findings;
            found
                .into_iter()
                .filter(|f| f.nets.iter().any(|n| n == "EN") && f.refs.iter().any(|r| r == "U1"))
                .collect::<Vec<_>>()
        };
        let button = || switch("SW1", &[("1", SIG), ("2", GND)]);
        let bare = on_en(vec![source_on(V3, "U2"), module(), button()]);
        assert!(!bare.is_empty(), "a button alone leaves EN floating");
        let pulled = on_en(vec![
            source_on(V3, "U2"),
            module(),
            button(),
            part("R1", "10k", &[("1", SIG), ("2", V3)]),
        ]);
        assert!(pulled.is_empty(), "{pulled:#?}");
    }

    #[test]
    fn a_reset_button_a_power_switch_a_selector_and_unsourced_rails_are_silent() {
        // A reset button to ground on a signal net.
        assert!(
            run(vec![switch("SW1", &[("1", SIG), ("2", GND)])]).is_empty(),
            "signal"
        );
        // A button from a rail to a signal net: the signal gets a level.
        assert!(
            run(vec![
                source_on(V3, "U1"),
                switch("SW1", &[("1", V3), ("2", SIG)])
            ])
            .is_empty(),
            "rail to signal"
        );
        // A power switch: the switched rail has no source but the switch.
        assert!(
            run(vec![
                source_on(V5, "U1"),
                switch("SW1", &[("1", V5), ("2", SWITCHED)])
            ])
            .is_empty(),
            "switched rail"
        );
        // A rail nothing feeds.
        assert!(
            run(vec![switch("SW1", &[("1", V3), ("2", GND)])]).is_empty(),
            "no source"
        );
        // Two names of one level.
        assert!(
            run(vec![
                source_on(V3, "U1"),
                source_on(V3B, "U2"),
                switch("SW1", &[("1", V3), ("2", V3B)])
            ])
            .is_empty(),
            "same level"
        );
        // A selector reaches three nets.
        assert!(
            run(vec![
                source_on(V3, "U1"),
                source_on(V5, "U2"),
                switch("SW2", &[("1", V5), ("2", V3), ("3", GND)])
            ])
            .is_empty(),
            "three nets"
        );
        // A button not fitted.
        let mut dnp = switch("SW1", &[("1", V3), ("2", GND)]);
        dnp.dnp = true;
        assert!(run(vec![source_on(V3, "U1"), dnp]).is_empty(), "DNP");
    }

    /// A 4-pad tact switch with distinct pad numbers on `footprint`, pads 1-4
    /// on `nets`; `place` gives their positions when the layout is known.
    fn tact(footprint: &str, nets: [i64; 4], place: Option<[(f64, f64); 4]>) -> Component {
        let mut sw = switch(
            "SW1",
            &[
                ("1", nets[0]),
                ("2", nets[1]),
                ("3", nets[2]),
                ("4", nets[3]),
            ],
        );
        sw.footprint = footprint.into();
        if let Some(at) = place {
            sw.layer = "F.Cu".into();
            for (pin, xy) in sw.pins.iter_mut().zip(at) {
                pin.position = Some(xy);
            }
        }
        sw
    }

    /// 6x6 THT pad pattern (6.5 x 4.5 mm), pads numbered across rows: the
    /// long-axis rows are 1-2 and 3-4.
    const ROWS_6X6: [(f64, f64); 4] = [(0.0, 0.0), (6.5, 0.0), (0.0, 4.5), (6.5, 4.5)];
    /// The same pads numbered down the columns: 1-3 and 2-4 are the rows.
    const COLUMNS_6X6: [(f64, f64); 4] = [(0.0, 0.0), (0.0, 4.5), (6.5, 0.0), (6.5, 4.5)];

    #[test]
    fn a_tact_switch_with_a_bonded_pad_pair_across_two_nets_is_always_closed() {
        // Table (no positions): the standard 6x6 footprint, pads 1-2 bonded.
        let found = run(vec![
            source_on(V3, "U1"),
            tact("SW_Tactile_6x6mm", [V3, SIG, GND, GND], None),
        ]);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert!(
            found[0].message.contains("always closed") && found[0].message.contains("pads 1 and 2"),
            "{}",
            found[0].message
        );
        assert_eq!(found[0].severity, Severity::Medium);
        // Geometry: the same wiring read from the placed pads (a name that no
        // table knows) is High.
        let found = run(vec![
            source_on(V3, "U1"),
            tact("Vendor_Button", [V3, SIG, GND, GND], Some(ROWS_6X6)),
        ]);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].severity, Severity::High);
        // Geometry beats the table: on a layout that numbers down the columns,
        // pads 1 and 3 are one terminal.
        let found = run(vec![tact(
            "SW_Tactile_6x6mm",
            [V3, GND, V3, GND],
            Some(COLUMNS_6X6),
        )]);
        assert!(found.is_empty(), "1 and 3 share a net: {found:#?}");
        let found = run(vec![tact(
            "SW_Tactile_6x6mm",
            [V3, GND, GND, GND],
            Some(COLUMNS_6X6),
        )]);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert!(
            found[0].message.contains("pads 1 and 3"),
            "{}",
            found[0].message
        );
    }

    #[test]
    fn a_tact_switch_wired_across_its_contact_is_silent() {
        // Rail on pad 1 and signal on pad 3 (bonded 1-2 and 3-4): a normal
        // button. Both pads of a side may share the net, or only one be used.
        for nets in [[V3, V3, SIG, SIG], [V3, 0, SIG, 0], [V3, V3, SIG, 0]] {
            let found = run(vec![
                source_on(V3, "U1"),
                tact("SW_Tactile_6x6mm", nets, None),
            ]);
            assert!(found.is_empty(), "table {nets:?}: {found:#?}");
            let found = run(vec![
                source_on(V3, "U1"),
                tact("Vendor_Button", nets, Some(ROWS_6X6)),
            ]);
            assert!(found.is_empty(), "geometry {nets:?}: {found:#?}");
        }
        // A footprint nobody can map (no positions, unknown name) and a
        // square pad pattern (the bond is ambiguous) abstain.
        let found = run(vec![tact("Vendor_Button", [V3, SIG, GND, GND], None)]);
        assert!(found.is_empty(), "unknown footprint: {found:#?}");
        let square = [(0.0, 0.0), (5.0, 0.0), (0.0, 5.0), (5.0, 5.0)];
        let found = run(vec![tact(
            "Vendor_Button",
            [V3, SIG, GND, GND],
            Some(square),
        )]);
        assert!(found.is_empty(), "square: {found:#?}");
        // A 2-position DIP switch has four pads too, but each pair is its own
        // contact: its rows are not one terminal.
        let mut dip = tact("SMT_DIP_SWITCH", [V3, SIG, GND, GND], Some(ROWS_6X6));
        dip.lib_id = "Switch:SW_DIP_x02".into();
        dip.value = "2_SMT_DIP_SWITCH".into();
        let found = run(vec![dip]);
        assert!(found.is_empty(), "DIP: {found:#?}");
        // Pin coordinates from a schematic (no board side) are symbol layout,
        // not pad geometry: only the table may speak, and this name is not in it.
        let mut sch = tact("Vendor_Button", [V3, SIG, GND, GND], Some(ROWS_6X6));
        sch.layer.clear();
        let found = run(vec![sch]);
        assert!(found.is_empty(), "schematic coordinates: {found:#?}");
        // KiCad's own tact footprints repeat a pad number across the bond: two
        // distinct numbers, already merged by the netlist; not mapped here.
        let kicad = switch("SW1", &[("1", V3), ("1", V3), ("2", SIG), ("2", SIG)]);
        assert!(run(vec![kicad]).is_empty());
    }

    #[test]
    fn a_tact_switch_that_never_closes_is_flagged() {
        // Both terminals on one net.
        let found = run(vec![tact("SW_Tactile_6x6mm", [SIG, SIG, SIG, SIG], None)]);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert!(
            found[0].message.contains("never closes"),
            "{}",
            found[0].message
        );
        // One terminal left unconnected.
        let found = run(vec![tact("SW_Tactile_6x6mm", [SIG, SIG, 0, 0], None)]);
        assert_eq!(found.len(), 1, "{found:#?}");
        // Not connected at all: nothing to say.
        let found = run(vec![tact("SW_Tactile_6x6mm", [0, 0, 0, 0], None)]);
        assert!(found.is_empty(), "{found:#?}");
    }
}
