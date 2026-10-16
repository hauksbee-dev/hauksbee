//! Which supply-named rails are really dead, and which the board feeds.
//!
//! One rule, shared by the run (`runner::dead_rails`) and the starter-spec
//! scaffold (`init`): a supply-named rail that a board resistor ties to a
//! powered node is not dead. The run solves it at whatever level the feed and
//! its load leave it, which is the board's own answer, and a `[[supply]]`
//! proposed there would fight the board. A regulator the board holds off stays
//! dead whatever else touches its output.

use std::collections::HashSet;

use hauksbee_engine::BoundBoard;
use hauksbee_ir::{Device, NodeId};

/// A rail the board feeds through a resistor from a powered node.
#[derive(Debug, Clone, PartialEq)]
pub struct ResistorFed {
    /// The supply-named rail.
    pub rail: String,
    /// The powered net the resistor ties it to.
    pub from: String,
    /// The resistor's reference.
    pub resistor: String,
    /// The resistor's value, in ohms.
    pub ohms: f64,
}

impl ResistorFed {
    /// Why the scaffold does not propose a `[[supply]]` for this rail.
    pub(crate) fn reason(&self) -> String {
        format!(
            "{} is fed from {} through {} ({}); not proposing a [[supply]] for it",
            self.rail,
            self.from,
            self.resistor,
            format_ohms(self.ohms)
        )
    }
}

/// Human ohms: 4700 -> "4.7k", 470 -> "470 ohm" (suffix-free below 1k).
fn format_ohms(ohms: f64) -> String {
    if ohms >= 1_000_000.0 {
        format!("{:.1}M", ohms / 1_000_000.0)
    } else if ohms >= 1_000.0 {
        format!("{:.1}k", ohms / 1_000.0)
    } else {
        format!("{ohms:.0}")
    }
}

/// The nodes something powers: every supply leg the binder stamped, plus every
/// net in `answered` (a net the spec declares a supply on or drives).
pub fn powered_nodes(bound: &BoundBoard, answered: &[&str]) -> HashSet<NodeId> {
    bound
        .supplies
        .iter()
        .map(|leg| leg.net_name.as_str())
        .chain(answered.iter().copied())
        .filter_map(|net| bound.node(net))
        .collect()
}

/// The first board resistor that ties `node` directly to one of the `powered`
/// nodes, as `(reference, ohms, the powered node)`.
fn feeding_resistor(
    bound: &BoundBoard,
    node: NodeId,
    powered: &HashSet<NodeId>,
) -> Option<(String, f64, NodeId)> {
    bound.circuit.iter().find_map(|(_, device)| match device {
        Device::Resistor {
            name, a, b, ohms, ..
        } => {
            if *a == node && powered.contains(b) {
                Some((name.clone(), *ohms, *b))
            } else if *b == node && powered.contains(a) {
                Some((name.clone(), *ohms, *a))
            } else {
                None
            }
        }
        _ => None,
    })
}

/// Split `unpowered` (the detector's supply-named rails nothing answers) into
/// the rails that are really dead and the ones a board resistor feeds.
///
/// `answered` is every net the spec already powers or drives; the binder's own
/// supply legs count as powered too. A held-off regulator rail is always dead.
pub fn split_dead_and_fed(
    bound: &BoundBoard,
    unpowered: Vec<String>,
    answered: &[&str],
) -> (Vec<String>, Vec<ResistorFed>) {
    let held_off: Vec<String> = hauksbee_engine::held_off_rails(bound)
        .into_iter()
        .map(|(net, _)| net)
        .collect();
    let powered = powered_nodes(bound, answered);
    let mut dead = Vec::new();
    let mut fed = Vec::new();
    for rail in unpowered {
        let source = if held_off.contains(&rail) {
            None
        } else {
            bound
                .node(&rail)
                .and_then(|node| feeding_resistor(bound, node, &powered))
        };
        match source {
            Some((resistor, ohms, from)) => fed.push(ResistorFed {
                from: bound.circuit.node_name(from).to_string(),
                rail,
                resistor,
                ohms,
            }),
            None => dead.push(rail),
        }
    }
    (dead, fed)
}
