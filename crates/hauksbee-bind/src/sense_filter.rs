//! The rail a converter's sense pin filters, read off the board.
//!
//! Some controllers have no output pin of their own. The LTC4020's power stage
//! output is the rail at the far side of the 100 ohm filter on its CSP sense
//! pin, and a card says so with `out_via = "sense_filter"` (see
//! [`hauksbee_models::behavioral::OutVia`]). [`sense_filter_net`] finds that
//! rail. The binder stamps the converter there, and the static envelope check
//! puts the regulator's range there, from this one reading.

use hauksbee_extract::assembly::AssemblyState;
use hauksbee_extract::Component;
use hauksbee_models::refdes::RefClass;

use crate::program_resistor::{classify_series, electrical_terminal_nets, is_inert_marker, Series};

/// Two resistors lead from the sense pin's net to different nets, and nothing
/// on the board says which one is the filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AmbiguousFilter;

/// The net the sense pin of `part` on `pin` filters.
///
/// The pin's net is a filter stub when every other fitted part on it is a
/// capacitor or a two-terminal resistor. The rail is then the far net of the
/// one resistor that leads neither to ground nor back to a pin of `part`
/// itself: a divider resistor from the sense pin to the part's own feedback
/// pin (the LTC4020's RMAX1, CSP to VFBMAX) is not the filter. A net with any
/// other fitted part on it (a regulator, a FET, a connector) is the rail
/// itself, and so is a stub with no such resistor: the pin is wired straight to
/// what it senses. Resistors of any value count, as in `fet_orientation`'s
/// sensed nets. An open link conducts nothing and does not count, and neither
/// does a test point, a hole or any other part with one connected pad.
///
/// `node_of` maps a board net id to the caller's node type (`None` for an
/// unconnected or unknown net), so the binder can ask in circuit nodes and a
/// static check in net ids.
///
/// # Errors
/// [`AmbiguousFilter`] when two such resistors lead to different nets.
pub fn sense_filter_net<N: Copy + PartialEq>(
    components: &[Component],
    part: &Component,
    pin: N,
    node_of: impl Fn(i64) -> Option<N>,
    is_ground: impl Fn(N) -> bool,
) -> Result<N, AmbiguousFilter> {
    let node = |net: Option<i64>| net.and_then(&node_of);
    let own: Vec<N> = part.pins.iter().filter_map(|p| node(p.net)).collect();
    // Every candidate first: a part that makes the net the rail wins wherever
    // it sits in the list, so the answer never depends on the board's order.
    let mut far: Vec<N> = Vec::new();
    for c in components {
        if c.reference == part.reference
            || matches!(AssemblyState::of(c), AssemblyState::DnpAbsent(_))
            || !c.pins.iter().any(|p| node(p.net) == Some(pin))
            || is_inert_marker(c)
        {
            continue;
        }
        match RefClass::of(&c.reference) {
            RefClass::Capacitor => continue,
            RefClass::Resistor => {}
            _ => return Ok(pin),
        }
        let Some(ends) = electrical_terminal_nets(c) else {
            return Ok(pin);
        };
        if matches!(classify_series(c), Series::Open) {
            continue;
        }
        let Some(other) = <[i64; 2]>::from(ends)
            .into_iter()
            .filter_map(&node_of)
            .find(|n| *n != pin)
        else {
            continue;
        };
        if !is_ground(other) && !own.contains(&other) && !far.contains(&other) {
            far.push(other);
        }
    }
    match far.as_slice() {
        [] => Ok(pin),
        [rail] => Ok(*rail),
        _ => Err(AmbiguousFilter),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hauksbee_extract::Pin;

    fn part(reference: &str, value: &str, nets: &[(&str, i64)]) -> Component {
        Component {
            reference: reference.to_string(),
            value: value.to_string(),
            pins: nets
                .iter()
                .map(|(number, net)| Pin {
                    number: (*number).to_string(),
                    net: Some(*net),
                    ..Pin::default()
                })
                .collect(),
            ..Component::default()
        }
    }

    // Nets: 1 rail (30V_GATE), 2 CSP stub, 3 VFBMAX, 4 CSN stub, 0 ground.
    fn resolve(extra: Vec<Component>) -> Result<i64, AmbiguousFilter> {
        let u2 = part("U2", "LTC4020", &[("23", 2), ("26", 3), ("22", 4)]);
        let mut board = vec![
            u2.clone(),
            part("R175", "100", &[("1", 1), ("2", 2)]),
            part("C31", "100n", &[("1", 4), ("2", 2)]),
        ];
        board.extend(extra);
        sense_filter_net(&board, &u2, 2, |n| (n != 0).then_some(n), |n| n == 0)
    }

    #[test]
    fn the_rail_is_the_far_side_of_the_filter_resistor() {
        assert_eq!(
            resolve(Vec::new()),
            Ok(1),
            "R175 leads from CSP to the rail"
        );
    }

    #[test]
    fn a_divider_back_to_the_part_and_a_resistor_to_ground_are_not_the_filter() {
        let rmax1 = part("R56", "1M", &[("1", 2), ("2", 3)]);
        let to_ground = part("R9", "10k", &[("1", 2), ("2", 0)]);
        assert_eq!(resolve(vec![rmax1, to_ground]), Ok(1));
    }

    #[test]
    fn a_pin_with_no_filter_or_on_a_net_with_active_parts_is_the_rail_itself() {
        let u2 = part("U2", "LTC4020", &[("23", 1)]);
        let direct = vec![u2.clone(), part("C1", "10u", &[("1", 1), ("2", 0)])];
        let rail = |board: &[Component]| {
            sense_filter_net(board, &u2, 1, |n| (n != 0).then_some(n), |n| n == 0)
        };
        assert_eq!(rail(&direct), Ok(1));
        let mut loaded = direct;
        loaded.push(part("R1", "10k", &[("1", 1), ("2", 5)]));
        // Wired by two pins: a part with one connected pad is an inert marker
        // (a test point, a hole) and conducts nowhere, so it is skipped.
        loaded.push(part("U7", "LM5017", &[("1", 1), ("2", 0)]));
        assert_eq!(
            rail(&loaded),
            Ok(1),
            "a regulator on the net makes it the rail"
        );
    }

    #[test]
    fn a_test_point_on_the_stub_does_not_make_it_the_rail() {
        let tp = part("TP3", "TestPoint", &[("1", 2)]);
        assert_eq!(resolve(vec![tp]), Ok(1));
    }

    #[test]
    fn two_filters_to_different_nets_are_ambiguous_and_an_open_link_is_ignored() {
        let second = part("R2", "100", &[("1", 2), ("2", 7)]);
        assert_eq!(resolve(vec![second.clone()]), Err(AmbiguousFilter));
        // An active part anywhere on the net makes it the rail, in any order.
        let active = part("Q1", "SI7461DP", &[("1", 2), ("4", 8)]);
        assert_eq!(resolve(vec![second.clone(), active.clone()]), Ok(2));
        assert_eq!(resolve(vec![active, second]), Ok(2));
        // A second resistor to the same rail is one filter, not two.
        let parallel = part("R5", "100", &[("1", 1), ("2", 2)]);
        assert_eq!(resolve(vec![parallel]), Ok(1));
        let open = part("R3", "open", &[("1", 2), ("2", 7)]);
        assert_eq!(resolve(vec![open]), Ok(1));
        let mut dnp = part("R4", "100", &[("1", 2), ("2", 7)]);
        dnp.dnp = true;
        assert_eq!(resolve(vec![dnp]), Ok(1));
    }
}
