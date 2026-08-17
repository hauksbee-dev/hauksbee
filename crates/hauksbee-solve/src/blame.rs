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
    let layout = Layout::new(circuit);
    name_node_unknown_with_layout(circuit, &layout, unknown)
}

fn name_node_unknown_with_layout(circuit: &Circuit, layout: &Layout, unknown: usize) -> String {
    if let Some(id) = layout.node_id(unknown) {
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

/// An ideal voltage source that pins a node's voltage against ground.
#[derive(Debug, Clone)]
pub struct NodeSource {
    /// Device reference, e.g. `Vsupply_VDD` or `Vci_drive_RES`.
    pub name: String,
    /// The voltage it commands at `t = 0`. Every `SourceKind` can be evaluated,
    /// so this is always known; a time-varying source's later value differs, but
    /// the bias-point conflict is what makes the matrix singular.
    pub volts: f64,
    /// True when the commanded value varies with time, so the reported voltage
    /// is the `t = 0` value rather than the whole story.
    pub time_varying: bool,
}

/// Two or more ideal sources fixing the same node: a genuinely singular
/// topology, and the mechanism by which a requested drive silently loses.
#[derive(Debug, Clone)]
pub struct SourceConflict {
    /// The contested node.
    pub node: NodeId,
    /// Its net name.
    pub net: String,
    /// Every ideal source pinning it, in circuit order.
    pub sources: Vec<NodeSource>,
}

impl SourceConflict {
    /// The source whose commanded voltage the settled node voltage matches, if
    /// exactly one does. This is the honest way to say "which won": not by
    /// guessing at pivot order, but by reading the answer the solve produced.
    pub fn winner(&self, settled_volts: f64) -> Option<&NodeSource> {
        // A generous window: the point is to identify WHICH source the node
        // followed, not to grade the solve's accuracy.
        let tol = 1e-3 + 1e-3 * settled_volts.abs();
        let mut hits = self
            .sources
            .iter()
            .filter(|s| (s.volts - settled_volts).abs() <= tol);
        let first = hits.next()?;
        if hits.next().is_some() {
            // Two sources commanding the same voltage: nothing was overridden
            // in any meaningful sense, so name no winner.
            None
        } else {
            Some(first)
        }
    }

    /// One loud line naming every contender and, when the settled voltage is
    /// known, which one the net actually followed.
    pub fn describe(&self, settled_volts: Option<f64>) -> String {
        let contenders = self
            .sources
            .iter()
            .map(|s| {
                if s.time_varying {
                    format!("{} ({:.3} V at t=0, time-varying)", s.name, s.volts)
                } else {
                    format!("{} ({:.3} V)", s.name, s.volts)
                }
            })
            .collect::<Vec<_>>()
            .join(" vs ");
        let mut line = format!(
            "net '{}' is pinned by {} ideal sources at once: {}",
            self.net,
            self.sources.len(),
            contenders
        );
        match settled_volts {
            Some(v) => match self.winner(v) {
                Some(w) => {
                    let lost = self
                        .sources
                        .iter()
                        .filter(|s| s.name != w.name)
                        .map(|s| s.name.clone())
                        .collect::<Vec<_>>()
                        .join(", ");
                    line.push_str(&format!(
                        "; the net settled at {v:.3} V, so {} won and {lost} had no effect",
                        w.name
                    ));
                }
                None => line.push_str(&format!(
                    "; the net settled at {v:.3} V, which matches no single source,                      so the result is not any of the requested voltages"
                )),
            },
            None => line.push_str(
                "; the matrix is singular there and the solve cannot honour both",
            ),
        }
        line
    }
}

/// Every ideal ground-referenced voltage source pinning `node`.
pub fn sources_on_node(circuit: &Circuit, node: NodeId) -> Vec<NodeSource> {
    if node.is_ground() {
        return Vec::new();
    }
    circuit
        .devices
        .iter()
        .filter_map(|d| match d {
            Device::Vsource { name, p, n, kind } => {
                // A source is only PINNING this node if its other terminal is
                // ground. Two sources in series up a divider chain are not in
                // conflict, and must not be reported as one.
                let pins = (*p == node && n.is_ground()) || (*n == node && p.is_ground());
                pins.then(|| {
                    let v0 = kind.eval(0.0);
                    let volts = if *n == node { -v0 } else { v0 };
                    NodeSource {
                        name: name.clone(),
                        volts,
                        time_varying: !matches!(kind, hauksbee_ir::SourceKind::Dc(_)),
                    }
                })
            }
            _ => None,
        })
        .collect()
}

/// Nodes pinned by more than one ideal source.
///
/// This is one defect wearing two hats. As a topology fault it is the textbook
/// singular MNA system: two identical constraint rows, no solution, and a
/// refusal that until now named nothing. As an honesty fault it is how a
/// requested override loses: forcing a net to 20 V next to a 3.3 V rail leaves
/// the net reading 3.300 V, and without this nobody says so.
pub fn source_conflicts(circuit: &Circuit) -> Vec<SourceConflict> {
    let mut out: Vec<SourceConflict> = Vec::new();
    for k in 1..circuit.node_count() {
        let node = NodeId(k as u32);
        let sources = sources_on_node(circuit, node);
        if sources.len() > 1 {
            out.push(SourceConflict {
                node,
                net: circuit.node_name(node).to_string(),
                sources,
            });
        }
    }
    out
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
    layout: &Layout,
    stall: Option<(f64, usize)>,
) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some((step, unknown)) = stall {
        if step.is_finite() && step > 0.0 {
            let where_ = name_node_unknown_with_layout(circuit, layout, unknown);
            let mut clause = format!("worst-moving unknown {where_} (last step {step:.3e} V)");
            if let Some(id) = layout.node_id(unknown) {
                let on = devices_on_node(circuit, id);
                if !on.is_empty() {
                    clause.push_str(&format!(", devices on it: {}", elide(&on)));
                }
            }
            parts.push(clause);
        }
    }
    // A node pinned by two ideal sources is singular by construction. Name it
    // FIRST: it is the most specific and most actionable thing a failed solve
    // can say, and it is a topology fault no amount of solver work can fix.
    for c in source_conflicts(circuit) {
        parts.push(c.describe(None));
    }
    let stiff = stiff_links(circuit);
    if !stiff.is_empty() {
        let names: Vec<String> = stiff.iter().map(|s| s.name.clone()).collect();
        let worst = stiff.iter().map(|s| s.ohms).fold(f64::INFINITY, f64::min);
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
    fn two_sources_on_one_net_are_named_with_the_winner() {
        let mut c = Circuit::new();
        let res = c.node("RES");
        c.add(Device::Vsource {
            name: "Vsupply_RES".into(),
            p: res,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(3.3),
        });
        c.add(Device::Vsource {
            name: "Vci_drive_RES".into(),
            p: res,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(20.0),
        });
        let conflicts = source_conflicts(&c);
        assert_eq!(conflicts.len(), 1);
        let c0 = &conflicts[0];
        assert_eq!(c0.net, "RES");

        // The reported symptom: the net reads 3.300 V, so the supply won and the
        // 20 V drive had no effect. Both must be named, and the loser must be
        // named as the loser.
        let msg = c0.describe(Some(3.3));
        assert!(msg.contains("Vsupply_RES"), "{msg}");
        assert!(msg.contains("Vci_drive_RES"), "{msg}");
        assert!(msg.contains("3.300 V"), "{msg}");
        assert!(msg.contains("20.000 V"), "{msg}");
        assert!(
            msg.contains("Vsupply_RES won") && msg.contains("Vci_drive_RES had no effect"),
            "must say which won and which lost: {msg}"
        );
        assert_eq!(c0.winner(3.3).map(|w| w.name.as_str()), Some("Vsupply_RES"));
        assert_eq!(
            c0.winner(20.0).map(|w| w.name.as_str()),
            Some("Vci_drive_RES"),
            "the same conflict read at 20 V names the drive as the winner"
        );
    }

    #[test]
    fn sources_in_series_are_not_a_conflict() {
        // Two sources stacked into a divider chain share no node against ground
        // and must never be reported: a false accusation on every stacked-supply
        // board would train the user to ignore the note.
        let mut c = Circuit::new();
        let mid = c.node("MID");
        let top = c.node("TOP");
        c.add(Device::Vsource {
            name: "V1".into(),
            p: mid,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(5.0),
        });
        c.add(Device::Vsource {
            name: "V2".into(),
            p: top,
            n: mid,
            kind: SourceKind::Dc(5.0),
        });
        assert!(source_conflicts(&c).is_empty());
    }

    #[test]
    fn a_conflicted_net_is_named_in_the_blame_clause() {
        let mut c = Circuit::new();
        let res = c.node("RES");
        c.add(Device::Vsource {
            name: "Vsupply_RES".into(),
            p: res,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(3.3),
        });
        c.add(Device::Vsource {
            name: "Vdrive_RES".into(),
            p: res,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(20.0),
        });
        let layout = Layout::new(&c);
        let clause = blame_clause(&c, &layout, None)
            .expect("a singular topology must always name something");
        assert!(clause.contains("net 'RES'"), "{clause}");
        assert!(clause.contains("singular"), "{clause}");
    }

    #[test]
    fn many_suspects_are_elided() {
        let names: Vec<String> = (1..=10).map(|i| format!("R{i}")).collect();
        let s = elide(&names);
        assert_eq!(s, "R1, R2, R3 and 7 more");
    }
}
