//! Naming the smallest thing that can be blamed for a failed solve.
//!
//! "The analog solver failed on 10 chunks" is true and useless. A 259-part
//! board that will not solve leaves the user bisecting by model class, which is
//! how the anyshake/explorer board (ten resistors with the literal value `0`)
//! burned an afternoon. The solver already knows more than it says: at the
//! moment Newton gives up it holds the per-unknown step vector, so it knows
//! WHICH unknown refused to settle, and the circuit knows which devices touch
//! that unknown and which of them stamp a pathological conductance.
//!
//! This module turns that state into one short clause that names the smallest
//! identifiable thing: the net, the devices on it, and any element whose
//! conductance is so far outside the board's own distribution that it is the
//! obvious suspect. It never guesses a cause it cannot point at; when the
//! solver has no per-unknown state (a structurally singular factorization, say)
//! the blame is the node alone, which is still infinitely more than a count.
//!
//! Everything here is read-only over `&Circuit` and `&Layout`, allocates only
//! on the failure path, and is called only when a solve has already failed, so
//! it cannot perturb a converging run.

use crate::system::Layout;
use hauksbee_ir::{Circuit, Device, NodeId};

/// Conductance ratio above the board's median resistor conductance at which a
/// link is called out as the stiff suspect. A 0 Ω jumper clamped to the 1 µΩ
/// floor sits 1e9x above a 1 kΩ board median, so this threshold is not a close
/// call: it fires on the genuinely pathological and stays quiet on an ordinary
/// mix of shunts and pull-ups (a 10 mΩ sense resistor next to a 10 kΩ pull-up
/// is only 1e6x, and a board with real milliohm shunts raises its own median
/// denominator, which is why the test is relative and not an absolute floor).
const STIFF_RATIO: f64 = 1e8;

/// Resistance (ohms) at or below which a link is a candidate stiff suspect at
/// all. Above this a large ratio just means the board spans many decades,
/// which is normal and not worth naming.
const STIFF_OHMS: f64 = 1e-3;

/// How many suspects to name before eliding. Ten 0 Ω jumpers should read as
/// "R1, R2, R3 and 7 more", not as a wall.
const MAX_NAMED: usize = 3;

/// Human name for a node-block unknown index.
///
/// Node-block unknown `k` is netlist node `k + 1` (ground is never an unknown),
/// except for device-private internal unknowns appended past the netlist nodes,
/// which have no `NodeId` and are named by index.
pub fn name_node_unknown(circuit: &Circuit, unknown: usize) -> String {
    let id = NodeId((unknown + 1) as u32);
    if (unknown + 1) < circuit.node_count() {
        format!("net '{}'", circuit.node_name(id))
    } else {
        format!("device-internal unknown #{unknown}")
    }
}

/// Names of the devices connected to a netlist node, in circuit order.
pub fn devices_on_node(circuit: &Circuit, node: NodeId) -> Vec<String> {
    circuit
        .devices
        .iter()
        .filter(|d| d.nodes().contains(&node))
        .map(|d| d.name().to_string())
        .collect()
}

/// One stiff link: a device whose stamped conductance is far outside the
/// board's own distribution.
#[derive(Debug, Clone)]
pub struct StiffLink {
    /// Device reference, e.g. `R107`.
    pub name: String,
    /// The resistance it stamps, in ohms.
    pub ohms: f64,
}

/// Resistors whose conductance is pathologically high relative to the board's
/// median resistor conductance.
///
/// This is the anyshake case in one function: a literal `0` value that reached
/// the solver as a 1 µΩ short stamps 1e6 S into a matrix whose other entries
/// are milli-siemens, and the resulting condition number is what stops Newton.
/// Returns an empty vec on any board with fewer than three resistors (no
/// meaningful median to compare against) or when nothing stands out.
pub fn stiff_links(circuit: &Circuit) -> Vec<StiffLink> {
    let mut ohms: Vec<f64> = circuit
        .devices
        .iter()
        .filter_map(|d| match d {
            Device::Resistor { ohms, .. } if *ohms > 0.0 => Some(*ohms),
            _ => None,
        })
        .collect();
    if ohms.len() < 3 {
        return Vec::new();
    }
    ohms.sort_by(|a, b| a.partial_cmp(b).expect("filtered to finite positives"));
    let median = ohms[ohms.len() / 2];
    if !(median > 0.0) {
        return Vec::new();
    }
    circuit
        .devices
        .iter()
        .filter_map(|d| match d {
            Device::Resistor { name, ohms, .. }
                if *ohms > 0.0 && *ohms <= STIFF_OHMS && median / *ohms >= STIFF_RATIO =>
            {
                Some(StiffLink {
                    name: name.clone(),
                    ohms: *ohms,
                })
            }
            _ => None,
        })
        .collect()
}

/// Render a suspect list as `R1, R2, R3 and 7 more`.
fn elide(names: &[String]) -> String {
    if names.len() <= MAX_NAMED {
        return names.join(", ");
    }
    format!(
        "{} and {} more",
        names[..MAX_NAMED].join(", "),
        names.len() - MAX_NAMED
    )
}

/// The blame clause for a failed solve: the worst-stepping unknown, the devices
/// on it, and any stiff-link suspects on the board.
///
/// `stall` is `(worst undamped node step in volts, node-block unknown index)`
/// from the last Newton iteration, when the solver had one; `None` when it
/// failed before ever computing a step (a singular factorization on the first
/// iterate), in which case only the board-wide suspects can be named.
///
/// Returns `None` when there is genuinely nothing to name, so callers append
/// nothing rather than an empty parenthetical.
pub fn blame_clause(
    circuit: &Circuit,
    _layout: &Layout,
    stall: Option<(f64, usize)>,
) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some((step, unknown)) = stall {
        if step.is_finite() && step > 0.0 {
            let where_ = name_node_unknown(circuit, unknown);
            let mut clause = format!("worst-moving unknown {where_} (last step {step:.3e} V)");
            let id = NodeId((unknown + 1) as u32);
            if (unknown + 1) < circuit.node_count() {
                let on = devices_on_node(circuit, id);
                if !on.is_empty() {
                    clause.push_str(&format!(", devices on it: {}", elide(&on)));
                }
            }
            parts.push(clause);
        }
    }
    let stiff = stiff_links(circuit);
    if !stiff.is_empty() {
        let names: Vec<String> = stiff.iter().map(|s| s.name.clone()).collect();
        let worst = stiff
            .iter()
            .map(|s| s.ohms)
            .fold(f64::INFINITY, f64::min);
        parts.push(format!(
            "suspect near-zero-ohm link(s) {} (down to {worst:.3e} ohm, stamping {:.3e} S into the matrix)",
            elide(&names),
            1.0 / worst
        ));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hauksbee_ir::SourceKind;

    fn board_with_zero_links(link_ohms: f64) -> Circuit {
        let mut c = Circuit::new();
        let vcc = c.node("VCC");
        let mid = c.node("MID");
        let out = c.node("OUT");
        c.add(Device::Vsource {
            name: "V1".into(),
            p: vcc,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(5.0),
        });
        c.add(Device::Resistor {
            name: "R1".into(),
            a: vcc,
            b: mid,
            ohms: 1000.0,
            tc1: None,
        });
        c.add(Device::Resistor {
            name: "R2".into(),
            a: mid,
            b: NodeId::GROUND,
            ohms: 2000.0,
            tc1: None,
        });
        c.add(Device::Resistor {
            name: "R3".into(),
            a: mid,
            b: out,
            ohms: link_ohms,
            tc1: None,
        });
        c
    }

    #[test]
    fn a_microohm_link_is_named_as_the_stiff_suspect() {
        let c = board_with_zero_links(1e-6);
        let stiff = stiff_links(&c);
        assert_eq!(stiff.len(), 1, "exactly the 1 uohm link stands out");
        assert_eq!(stiff[0].name, "R3");
    }

    #[test]
    fn a_milliohm_jumper_is_not_flagged() {
        // The bind-time R0 treatment lands links at 1 mohm, which is a real
        // jumper resistance and must NOT read as pathological: otherwise every
        // repaired board carries a permanent false accusation.
        let c = board_with_zero_links(1e-3);
        assert!(
            stiff_links(&c).is_empty(),
            "1 mohm is a physical jumper, not a matrix poison"
        );
    }

    #[test]
    fn an_ordinary_board_names_nothing() {
        let c = board_with_zero_links(4700.0);
        assert!(stiff_links(&c).is_empty());
        let layout = Layout::new(&c);
        assert!(blame_clause(&c, &layout, None).is_none());
    }

    #[test]
    fn a_stalled_unknown_is_named_with_its_devices() {
        let c = board_with_zero_links(4700.0);
        let layout = Layout::new(&c);
        // Unknown 1 is netlist node 2 == MID.
        let clause = blame_clause(&c, &layout, Some((0.42, 1)))
            .expect("a stalled unknown always yields a clause");
        assert!(clause.contains("net 'MID'"), "names the net: {clause}");
        assert!(clause.contains("R1"), "names devices on it: {clause}");
        assert!(clause.contains("R3"), "names devices on it: {clause}");
    }

    #[test]
    fn many_suspects_are_elided() {
        let names: Vec<String> = (1..=10).map(|i| format!("R{i}")).collect();
        let s = elide(&names);
        assert_eq!(s, "R1, R2, R3 and 7 more");
    }
}
