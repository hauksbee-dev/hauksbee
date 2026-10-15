//! Scoped judgement: which net checks an unmodelled key part can change.
//!
//! A key part with no usable model (an active IC or a transistor) used to make
//! the whole `--check` verdict `invalid`. Most of a board's checks cannot be
//! changed by that part, so refusing them all hides real answers. This module
//! splits each named net's two claims (its wiring/value checks and its
//! electrical checks: levels, ratings, rails) into those an open part can
//! change and those it provably cannot, and the verdict judges only the second
//! kind.
//!
//! The rule is deliberately coarse, so it can only over-refuse, never
//! over-judge:
//! * wiring/value checks depend on an open part on every net it has a pin on
//!   (what that pin is, drives or needs is unknown);
//! * electrical checks depend on it on every net its pins reach through ANY
//!   part's pins, modelled or not, regulators and ICs included, because an
//!   unknown load can pull a rail down and with it everything the rail feeds.
//!   The only cut is the analysis ground: it is the solver's fixed reference
//!   node on every board, modelled or not, so an open part's return current
//!   cannot move it in the analysis. A ground net it touches is still marked
//!   dependent itself.

use std::collections::{BTreeMap, BTreeSet};

use hauksbee_extract::ExtractedBoard;
use serde::Serialize;

use crate::binder::{is_canonical_ground, BoundBoard};

/// What a run with unmodelled key parts could still judge, per named net.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct JudgementScope {
    /// The key parts with no usable model the judgement is scoped around.
    pub open_parts: Vec<String>,
    /// Named nets on the board. Each carries two claims: its wiring/value
    /// checks and its electrical checks.
    pub nets: usize,
    /// Nets an open part has a pin on: their wiring/value checks are not judged.
    pub wiring_dependent: Vec<String>,
    /// Nets an open part can reach through any part's pins (stopping at the
    /// analysis ground): their electrical checks are not judged.
    pub electrical_dependent: Vec<String>,
}

impl JudgementScope {
    /// The scope for `open` (the verdict-critical unmodelled parts), or `None`
    /// when there are none, so a fully modelled board is unchanged.
    #[must_use]
    pub fn of(board: &ExtractedBoard, bound: &BoundBoard, open: &[String]) -> Option<Self> {
        if open.is_empty() {
            return None;
        }
        let names: BTreeMap<i64, &str> = (board.nets.iter())
            .filter(|n| n.id != 0 && !n.name.trim().is_empty())
            .map(|n| (n.id, n.name.as_str()))
            .collect();
        let ground = |name: &str| {
            is_canonical_ground(name)
                || bound.net_nodes.get(name) == Some(&hauksbee_ir::NodeId::GROUND)
        };
        let parts: Vec<(&str, BTreeSet<&str>)> = (board.components.iter())
            .map(|c| {
                let nets = c.pins.iter().filter_map(|p| names.get(&p.net?).copied());
                (c.reference.trim(), nets.collect())
            })
            .collect();
        let wiring: BTreeSet<&str> = (parts.iter())
            .filter(|(r, _)| open.iter().any(|o| o.trim() == *r))
            .flat_map(|(_, nets)| nets.iter().copied())
            .collect();
        let mut electrical = wiring.clone();
        let mut frontier: Vec<&str> = wiring.iter().copied().filter(|n| !ground(n)).collect();
        while let Some(net) = frontier.pop() {
            for (_, nets) in parts.iter().filter(|(_, nets)| nets.contains(net)) {
                for &next in nets {
                    if electrical.insert(next) && !ground(next) {
                        frontier.push(next);
                    }
                }
            }
        }
        let owned = |set: BTreeSet<&str>| set.into_iter().map(str::to_string).collect();
        Some(Self {
            open_parts: open.to_vec(),
            nets: names.values().collect::<BTreeSet<_>>().len(),
            wiring_dependent: owned(wiring),
            electrical_dependent: owned(electrical),
        })
    }

    /// Net claims judged: both claims of every net, less the dependent ones.
    #[must_use]
    pub const fn judged(&self) -> usize {
        (2 * self.nets)
            .saturating_sub(self.wiring_dependent.len() + self.electrical_dependent.len())
    }

    /// Whether this scope lets the verdict judge the rest of the board for
    /// exactly these blockers: some claim was judged, and it was cut around
    /// every blocker the verdict would otherwise refuse over.
    #[must_use]
    pub fn scopes(&self, blockers: &[String]) -> bool {
        self.judged() > 0 && self.open_parts == blockers
    }

    /// One sentence naming the open parts, what was not judged, what was, and
    /// the action that judges the rest.
    #[must_use]
    pub fn condition(&self) -> String {
        let sample = |nets: &[String]| {
            let shown: Vec<&str> = nets.iter().take(4).map(String::as_str).collect();
            let more = nets.len().saturating_sub(shown.len());
            let more = if more > 0 {
                format!(" and {more} more")
            } else {
                String::new()
            };
            format!("{}{more}", shown.join(", "))
        };
        let parts = crate::result::refs_summary(&self.open_parts);
        let (has, it) = if self.open_parts.len() == 1 {
            ("has", "it")
        } else {
            ("have", "they")
        };
        let reach = self.electrical_dependent.len();
        let electrical = if reach == self.nets {
            format!("on all {reach} nets, since {it} can reach every one through the circuit, so NO electrical check was judged")
        } else {
            format!(
                "on the {reach} of {} net(s) {it} can reach through the circuit ({})",
                self.nets,
                sample(&self.electrical_dependent)
            )
        };
        format!(
            "Partly judged: {parts} {has} no usable model, so the checks {it} could change are \
             not judged: wiring and value checks on the {} net(s) {it} connects to ({}), and \
             electrical checks (levels, ratings, rails) {electrical}. The other {} of {} net \
             checks were judged. Draft the model(s) with `hauksbee models extract --unmodelled \
             <board>` to judge the rest.",
            self.wiring_dependent.len(),
            sample(&self.wiring_dependent),
            self.judged(),
            2 * self.nets,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::JudgementScope;
    use crate::binder::BoundBoard;
    use hauksbee_extract::{Component, ExtractedBoard, Net, Pin};

    const NETS: [&str; 6] = ["GND", "VIN", "VOUT", "SDA", "VB", "LED_K"];

    /// A board of `(reference, nets)` parts over [`NETS`] (net id = index + 1).
    fn board(parts: &[(&str, &[&str])]) -> ExtractedBoard {
        let id = |name: &str| (1..).zip(NETS).find(|(_, n)| *n == name).unwrap().0;
        let component = |(reference, nets): &(&str, &[&str])| Component {
            reference: (*reference).to_string(),
            pins: (1..)
                .zip(nets.iter())
                .map(|(i, net)| Pin {
                    number: format!("{i}"),
                    net: Some(id(net)),
                    ..Pin::default()
                })
                .collect(),
            ..Component::default()
        };
        ExtractedBoard {
            name: "synthetic".into(),
            nets: (NETS.iter()).map(|n| Net::new(id(n), *n)).collect(),
            components: parts.iter().map(component).collect(),
        }
    }

    /// An LDO (U2: VIN -> VOUT), a pull-up on SDA to VOUT, and a separate
    /// rail VB that lights an LED, joined to the rest only through ground.
    const BASE: [(&str, &[&str]); 5] = [
        ("U2", &["VIN", "GND", "VOUT"]),
        ("R1", &["SDA", "VOUT"]),
        ("J1", &["VB", "GND"]),
        ("R2", &["VB", "LED_K"]),
        ("D1", &["LED_K", "GND"]),
    ];

    fn scope(open: (&'static str, &'static [&'static str])) -> JudgementScope {
        let mut parts = BASE.to_vec();
        parts.push(open);
        let open = vec![open.0.to_string()];
        JudgementScope::of(&board(&parts), &BoundBoard::default(), &open).unwrap()
    }

    #[test]
    fn a_fully_modelled_board_has_no_scope() {
        assert!(JudgementScope::of(&board(&BASE), &BoundBoard::default(), &[]).is_none());
    }

    #[test]
    fn a_rail_joined_only_through_ground_stays_judged() {
        // (a) U3 sits on the LDO side; the VB rail and its LED meet it only at
        // ground, so their checks are judged and a fault there is still found.
        let s = scope(("U3", &["VOUT", "GND"]));
        assert_eq!(s.wiring_dependent, ["GND", "VOUT"]);
        assert_eq!(s.electrical_dependent, ["GND", "SDA", "VIN", "VOUT"]);
        assert_eq!(s.judged(), 12 - 2 - 4);
        assert!(s.scopes(&["U3".into()]));
        let c = s.condition();
        assert!(
            c.contains("U3 has no usable model") && c.contains("6 of 12"),
            "{c}"
        );
    }

    #[test]
    fn an_open_part_on_a_rail_takes_that_rail_and_its_loads() {
        // (b) U3 on the VB rail: the rail and everything on it is not judged.
        let s = scope(("U3", &["VB", "GND"]));
        assert!(s.wiring_dependent.contains(&"VB".to_string()));
        for net in ["VB", "LED_K"] {
            assert!(s.electrical_dependent.contains(&net.to_string()), "{net}");
        }
        assert!(!s.electrical_dependent.contains(&"VOUT".to_string()));
    }

    #[test]
    fn a_load_on_a_regulated_rail_reaches_back_through_the_regulator() {
        // (c) An unknown load on SDA can sink current through the pull-up
        // from VOUT, so VOUT, U2's input rail and SDA all depend on it.
        let s = scope(("U3", &["SDA", "GND"]));
        assert_eq!(s.wiring_dependent, ["GND", "SDA"]);
        for net in ["SDA", "VOUT", "VIN"] {
            assert!(s.electrical_dependent.contains(&net.to_string()), "{net}");
        }
    }

    #[test]
    fn an_open_part_on_every_net_judges_nothing() {
        let s = scope(("U3", &NETS));
        assert_eq!(s.judged(), 0);
        assert!(!s.scopes(&["U3".into()]));
    }
}
