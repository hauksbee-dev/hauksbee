//! Scoped judgement: which net checks an unmodelled key part can change.
//!
//! A key part with no usable model (an active IC or a transistor) used to make
//! the whole `--check` verdict `invalid`. Most of a board's checks cannot be
//! changed by that part, so refusing them all hides real answers. This module
//! splits each named net's two claims (its wiring/value checks and its
//! electrical checks: levels, ratings, rails) into those an open part can
//! change and those it provably cannot, and the verdict judges only the second
//! kind. A finding on a judged check still fails the board. With nothing
//! found, the answer is PARTLY JUDGED: never a pass (`ok` false, `--strict`
//! exit 3), and only when at least one electrical check was judged; wiring and
//! value checks alone leave the verdict `invalid`, with a line saying how many
//! of them came out clean.
//!
//! The rule is deliberately coarse, so beyond the one condition it states (an
//! open load on a held rail draws within what the source gives) it can only
//! over-refuse, never over-judge:
//! * wiring/value checks depend on an open part on every net it has a pin on
//!   (what that pin is, drives or needs is unknown);
//! * electrical checks depend on it on every net its pins reach through any
//!   part's pins, modelled or not, regulators and logic included. Three cuts
//!   stop that reach, each marked dependent itself:
//!   - the analysis ground: the solver's fixed reference node on every
//!     board, so an open part's return current cannot move it;
//!   - a held rail (`held_rails`): a rail a modelled source holds (a supply
//!     the analysis applies, or a modelled regulator fed from a held rail)
//!     and on which no open part has a pin that could drive it. An open load
//!     there is named, not followed, and the verdict states the condition
//!     that it draws within what the source gives;
//!   - a signal pin of a part whose pins the analysis treats as separate
//!     ports (`NO_TRANSFER`: a processor, an ADC, a connector); a rail
//!     pin of such a part still carries.
//!
//! Without the second and third cuts every open part reached every net on
//! nearly every real board (one rail joins all parts), so nothing was ever
//! judged; 42 of 107 census boards were refused that way.

use std::collections::{BTreeMap, BTreeSet};

use hauksbee_extract::ExtractedBoard;
use serde::Serialize;

use crate::binder::{is_canonical_ground, BoundBoard};

/// The scope line a single-check surface (`--drc`, `--thermal`, `--ac`)
/// carries beside its verdict: that verdict covers `check` alone, so a pass
/// there is not a pass for the board.
#[must_use]
pub fn single_check_note(check: &str) -> String {
    format!(
        "SCOPE: this verdict covers {check} only; the other checks were not run, so it is \
         not a verdict on the board. Run --check for the whole-board verdict."
    )
}

/// Model kinds (as the binder names them) whose pins the analysis treats as
/// separate ports: what an open part does on one signal pin cannot move the
/// level the part holds on another. A processor's outputs follow its
/// firmware, not its inputs; an ADC's input is a sampled load; a connector or
/// an ignored part stamps nothing between its contacts. Logic blocks are not
/// listed: a gate's output follows its input. A rail still moves everything
/// it feeds, these included.
const NO_TRANSFER: [&str; 4] = ["mcu", "adc", "connector", "ignore"];

/// Whether a pin name says the pin takes power from its net (a supply input),
/// so the part loads that rail and cannot be its source.
fn supply_input(function: &str) -> bool {
    let f = function.trim().to_ascii_uppercase();
    let f = f.trim_start_matches(['A', 'D']).trim_start_matches("IO");
    ["VDD", "VCC", "VIN"].iter().any(|p| f.starts_with(p))
}

/// The rails a modelled source holds whatever an open part on them does:
/// a rail the analysis supplies (a supply leg) or a regulator it models feeds
/// from a held rail, unless an open part has a pin on the rail that could
/// drive it (it may be that rail's source). An open part's load on a held
/// rail is named, not followed: the rail's other loads stay judged, on the
/// stated condition that the open part draws within what the source gives.
fn held_rails<'a>(
    bound: &'a BoundBoard,
    sourced: &BTreeSet<&'a str>,
    is_open: &dyn Fn(&str) -> bool,
    open_drives: &BTreeSet<&str>,
) -> BTreeSet<&'a str> {
    type Group<'g> = (BTreeSet<&'g str>, Vec<(&'g str, &'g str)>);
    let mut groups: Vec<Group<'a>> = (bound.report.supply_groups.iter())
        // A group joined through an open pass part (a switch or pass FET with
        // no model) may not be one rail at all: never held.
        .filter(|g| !g.passes.iter().any(|p| is_open(p.as_str())))
        .map(|g| {
            let nets = g.nets.iter().map(String::as_str).collect();
            let feeds = g
                .feeds
                .iter()
                .map(|(p, i)| (p.as_str(), i.as_str()))
                .collect();
            (nets, feeds)
        })
        .collect();
    for &net in sourced {
        if !groups.iter().any(|(nets, _)| nets.contains(net)) {
            groups.push((BTreeSet::from([net]), Vec::new()));
        }
    }
    let mut held = vec![false; groups.len()];
    loop {
        let mut changed = false;
        for (i, (nets, feeds)) in groups.iter().enumerate() {
            if held[i] || nets.iter().any(|n| open_drives.contains(n)) {
                continue;
            }
            let fed = !feeds.is_empty()
                && feeds.iter().all(|(part, input)| {
                    !is_open(part)
                        && (groups.iter().zip(&held)).any(|((ns, _), &h)| h && ns.contains(input))
                });
            if fed || nets.iter().any(|n| sourced.contains(n)) {
                held[i] = true;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    (groups.into_iter().zip(held))
        .filter(|(_, h)| *h)
        .flat_map(|((nets, _), _)| nets)
        .collect()
}

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
    /// Rails an open part loads that a modelled source holds (see
    /// `held_rails`): the rail itself is in `electrical_dependent`, but the
    /// reach stopped there, so the rest of what the rail feeds is judged on
    /// the condition the verdict states. Named in [`Self::condition`]; not a
    /// separate report field.
    #[serde(skip)]
    #[schemars(skip)]
    pub held_rails: Vec<String>,
}

impl JudgementScope {
    /// The scope for `open` (the verdict-critical unmodelled parts), or `None`
    /// when there are none, so a fully modelled board is unchanged.
    #[must_use]
    pub fn of(board: &ExtractedBoard, bound: &BoundBoard, open: &[String]) -> Option<Self> {
        let sourced = bound.supplies.iter().map(|l| l.net_name.as_str()).collect();
        Self::of_sourced(board, bound, open, &sourced)
    }

    /// [`Self::of`] with the supply-leg nets given (`bound.supplies` in a run).
    pub(crate) fn of_sourced<'a>(
        board: &'a ExtractedBoard,
        bound: &'a BoundBoard,
        open: &[String],
        sourced: &BTreeSet<&'a str>,
    ) -> Option<Self> {
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
        let is_open = |r: &str| open.iter().any(|o| o.trim() == r);
        let parts: Vec<(&str, BTreeSet<&str>)> = (board.components.iter())
            .map(|c| {
                let nets = c.pins.iter().filter_map(|p| names.get(&p.net?).copied());
                (c.reference.trim(), nets.collect())
            })
            .collect();
        let wiring: BTreeSet<&str> = (parts.iter())
            .filter(|(r, _)| is_open(r))
            .flat_map(|(_, nets)| nets.iter().copied())
            .collect();
        // Nets an open part has a pin on that could drive them (an untyped pin
        // counts): such a part may be the source of that rail.
        let open_drives: BTreeSet<&str> = (board.components.iter())
            .filter(|c| is_open(c.reference.trim()))
            .flat_map(|c| c.pins.iter())
            .filter(|p| crate::bind_report::pin_can_drive(&p.kind) && !supply_input(&p.function))
            .filter_map(|p| names.get(&p.net?).copied())
            .collect();
        let held = held_rails(bound, sourced, &is_open, &open_drives);
        let rails: BTreeSet<&str> = (bound.report.supply_groups.iter())
            .flat_map(|g| g.nets.iter().map(String::as_str))
            .chain(sourced.iter().copied())
            .collect();
        let carries = |reference: &str, from: &str| {
            // A rail moves every part it feeds, whatever the part; a signal
            // moves only what can pass it on (see `NO_TRANSFER`).
            rails.contains(from)
                || is_open(reference)
                || bound
                    .component_kinds
                    .get(reference)
                    .is_none_or(|k| !NO_TRANSFER.contains(&k.as_str()))
        };
        let mut electrical = wiring.clone();
        let mut frontier: Vec<&str> = (wiring.iter().copied())
            .filter(|n| !ground(n) && !held.contains(*n))
            .collect();
        while let Some(net) = frontier.pop() {
            for (reference, nets) in parts.iter().filter(|(_, nets)| nets.contains(net)) {
                if !carries(reference, net) {
                    continue;
                }
                for &next in nets {
                    if electrical.insert(next) && !ground(next) && !held.contains(next) {
                        frontier.push(next);
                    }
                }
            }
        }
        let owned = |set: BTreeSet<&str>| set.into_iter().map(str::to_string).collect();
        let held_touched: Vec<String> = (electrical.iter())
            .filter(|n| held.contains(**n))
            .map(|n| (*n).to_string())
            .collect();
        Some(Self {
            open_parts: open.to_vec(),
            nets: names.values().collect::<BTreeSet<_>>().len(),
            wiring_dependent: owned(wiring),
            electrical_dependent: owned(electrical),
            held_rails: held_touched,
        })
    }

    /// Net claims judged: both claims of every net, less the dependent ones.
    #[must_use]
    pub const fn judged(&self) -> usize {
        (2 * self.nets)
            .saturating_sub(self.wiring_dependent.len() + self.electrical_dependent.len())
    }

    /// Nets whose wiring/value checks were judged.
    #[must_use]
    pub const fn wiring_judged(&self) -> usize {
        self.nets.saturating_sub(self.wiring_dependent.len())
    }

    /// Nets whose electrical checks (levels, ratings, rails) were judged.
    #[must_use]
    pub const fn electrical_judged(&self) -> usize {
        self.nets.saturating_sub(self.electrical_dependent.len())
    }

    /// Whether this scope lets the verdict judge the rest of the board for
    /// exactly these blockers: at least one electrical check was judged (the
    /// wiring/value checks alone never make a partly judged answer), and the
    /// scope was cut around every blocker the verdict would otherwise refuse
    /// over.
    #[must_use]
    pub fn scopes(&self, blockers: &[String]) -> bool {
        self.electrical_judged() > 0 && self.open_parts == blockers
    }

    /// The verdict reason of a run this scope judged with nothing found: it
    /// is not a pass, so it says so first.
    #[must_use]
    pub fn partly_judged_reason(&self) -> String {
        format!(
            "PARTLY JUDGED (not a pass): nothing found blocks ordering among the checks this \
             run could judge. {}",
            self.condition()
        )
    }

    /// For a scope too narrow to judge any electrical check: one line, as
    /// information only, saying how many wiring/value checks came out clean.
    /// `None` when none did.
    #[must_use]
    pub fn wiring_note(&self) -> Option<String> {
        let judged = self.wiring_judged();
        (judged > 0 && self.electrical_judged() == 0).then(|| {
            let (parts, does) = match self.open_parts.as_slice() {
                [one] => (one.clone(), "does"),
                many => (crate::result::refs_summary(many), "do"),
            };
            format!(
                "For information only, not a pass: the wiring and value checks on the {judged} \
                 of {} net(s) {parts} {does} not connect to found nothing that fails, but {} can \
                 reach every net through the circuit, so no electrical check could be judged.",
                self.nets,
                self.pronoun(),
            )
        })
    }

    const fn pronoun(&self) -> &'static str {
        if self.open_parts.len() == 1 {
            "it"
        } else {
            "they"
        }
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
        let it = self.pronoun();
        let has = if self.open_parts.len() == 1 {
            "has"
        } else {
            "have"
        };
        let held = if self.held_rails.is_empty() {
            String::new()
        } else {
            let draws = if self.open_parts.len() == 1 {
                "draws"
            } else {
                "draw"
            };
            format!(
                " That assumes {it} {draws} no more than the source of {} can supply; the \
                 rest of what that rail feeds was judged on that condition.",
                sample(&self.held_rails),
            )
        };
        format!(
            "Partly judged: {parts} {has} no usable model, so the checks {it} could change are \
             not judged: wiring and value checks on the {} net(s) {it} connects to ({}), and \
             electrical checks (levels, ratings, rails) on the {} of {} net(s) {it} can reach \
             through the circuit ({}). The other {} of {} net checks were judged.{held} Draft \
             the model(s) with `hauksbee models extract --unmodelled <board>` to judge the rest.",
            self.wiring_dependent.len(),
            sample(&self.wiring_dependent),
            self.electrical_dependent.len(),
            self.nets,
            sample(&self.electrical_dependent),
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
        assert!(s.wiring_note().is_none());
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
        assert!(s.wiring_note().is_none());
    }

    /// [`BASE`] with U2 modelled as a regulator feeding VOUT from VIN, VIN a
    /// supply the analysis applies, R1 a pull-up from SDA to VOUT, and the open
    /// part U3 on VOUT with its VOUT pin typed `kind`.
    fn held(open_pin_kind: &str, u2_open: bool) -> JudgementScope {
        let mut b = board(&{
            let mut parts = BASE.to_vec();
            parts.push(("U3", &["VOUT", "GND"]));
            parts
        });
        let u3 = b
            .components
            .iter_mut()
            .find(|c| c.reference == "U3")
            .unwrap();
        u3.pins[0].kind = open_pin_kind.into();
        let mut bound = BoundBoard::default();
        bound
            .report
            .supply_groups
            .push(hauksbee_bind::rail_owner::SupplyGroup {
                nets: ["VOUT".to_string()].into(),
                passes: std::collections::BTreeSet::new(),
                feeds: [("U2".to_string(), "VIN".to_string())].into(),
            });
        let mut open = vec!["U3".to_string()];
        if u2_open {
            open.push("U2".into());
        }
        let sourced = ["VIN"].into();
        JudgementScope::of_sourced(&b, &bound, &open, &sourced).unwrap()
    }

    #[test]
    fn an_open_load_on_a_held_rail_leaves_the_rails_other_loads_judged() {
        // Legal: U3 only takes power from VOUT (a power_in pin), and a modelled
        // regulator on a supplied input holds VOUT, so the reach stops there:
        // SDA (pulled up from VOUT) and VIN stay judged, on a stated condition.
        let s = held("power_in", false);
        assert_eq!(s.electrical_dependent, ["GND", "VOUT"]);
        assert_eq!(s.held_rails, ["VOUT"]);
        let c = s.condition();
        assert!(
            c.contains("draws no more than the source of VOUT can supply"),
            "{c}"
        );
    }

    #[test]
    fn an_open_part_that_could_drive_a_rail_is_followed_through_it() {
        // Faulty: an untyped (or output) pin may be the rail's own source, so
        // the rail is not held and SDA and VIN depend on U3 as before.
        let s = held("", false);
        assert!(!s.held_rails.contains(&"VOUT".to_string()));
        for net in ["SDA", "VOUT", "VIN"] {
            assert!(s.electrical_dependent.contains(&net.to_string()), "{net}");
        }
    }

    #[test]
    fn a_rail_fed_by_an_open_regulator_is_not_held() {
        // Faulty: U2 has no model, so nothing modelled holds VOUT.
        let s = held("power_in", true);
        assert!(s.held_rails.is_empty());
        assert!(s.electrical_dependent.contains(&"SDA".to_string()));
    }

    /// An MCU-like part U1 on `SDA` and `LED_K` (two signal pins) and `VOUT` (its
    /// rail, a supply group), bound as `kind`, with an open part `U3` on `u3_net`.
    fn through_part(kind: &str, u3_net: &'static str) -> JudgementScope {
        let u1_nets: [&str; 4] = ["SDA", "LED_K", "VOUT", "GND"];
        let u3_nets: [&str; 2] = [u3_net, "GND"];
        let parts: Vec<(&str, &[&str])> = vec![("U1", &u1_nets[..]), ("U3", &u3_nets[..])];
        let mut b = board(&parts);
        b.components[0].pins[2].function = "VDD".into();
        let mut bound = BoundBoard::default();
        bound.component_kinds.insert("U1".into(), kind.into());
        bound
            .report
            .supply_groups
            .push(hauksbee_bind::rail_owner::SupplyGroup {
                nets: ["VOUT".to_string()].into(),
                ..Default::default()
            });
        let none = std::collections::BTreeSet::new();
        JudgementScope::of_sourced(&b, &bound, &["U3".into()], &none).unwrap()
    }

    #[test]
    fn a_processor_does_not_pass_an_open_parts_signal_to_its_other_pins() {
        // Legal: what U3 does on SDA cannot move the level U1 holds on LED_K
        // or its rail.
        let s = through_part("mcu", "SDA");
        assert_eq!(s.electrical_dependent, ["GND", "SDA"]);
    }

    #[test]
    fn a_rail_moves_a_processor_whole_and_a_gate_passes_a_signal_on() {
        // Faulty/controls: an open part on the processor's rail reaches every
        // pin of it; a logic part's output follows its input.
        for (kind, net) in [("mcu", "VOUT"), ("logic", "SDA")] {
            let s = through_part(kind, net);
            for n in ["SDA", "LED_K", "VOUT"] {
                assert!(
                    s.electrical_dependent.contains(&n.to_string()),
                    "{kind} {net}: {n}"
                );
            }
        }
    }

    #[test]
    fn wiring_checks_alone_never_make_a_partly_judged_answer() {
        // U3 bridges both halves of the board, so it reaches every net
        // electrically while touching only two: the untouched nets' wiring
        // checks are counted as information, never as a scope.
        let s = scope(("U3", &["VOUT", "VB"]));
        assert_eq!(s.electrical_judged(), 0);
        assert_eq!(s.wiring_judged(), 4);
        assert!(!s.scopes(&["U3".into()]));
        let note = s.wiring_note().expect("four wiring checks came out clean");
        assert!(
            note.starts_with("For information only, not a pass")
                && note.contains("4 of 6 net(s) U3 does not connect to"),
            "{note}"
        );
    }
}
