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

/// Flag every two-terminal switch closed between two nets that each hold a level.
#[must_use]
pub fn switch_short_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    let mut report = NetLintReport::default();
    for sw in &board.components {
        if AssemblyState::of(sw).fitted().is_none() || !super::regulator_enable::is_switch(sw) {
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
}
