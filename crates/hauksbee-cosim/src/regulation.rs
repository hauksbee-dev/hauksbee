//! Simple-regulator validity.
//!
//! A simple vreg is an ideal DC source that ignores
//! its input, so its setpoint is only a result when the input has INDEPENDENT
//! headroom. This module establishes that or reports why not; it never
//! substitutes a voltage of its own for an unsupported output.

use hauksbee_bind::bind_report::{OpenPart, SimpleRegulator};
use hauksbee_ir::{Circuit, Device, NodeId, SourceKind};
use hauksbee_solve::{run_op, Probe, SolverOptions};
use std::collections::HashMap;
use std::fmt::Write as _;

/// One unsupported regulator: the shared reason, and everything it invalidates.
/// `refs[0]` is the regulator itself; the rest are MCUs powered from `nets`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RegulationTaint {
    pub reason: String,
    pub nets: Vec<String>,
    pub refs: Vec<String>,
    pub since_s: f64,
}

/// Input minus return in the regulator's polarity, or why no independent solve.
pub type Verdict = Result<f64, String>;

/// `(output, return, signed setpoint)` read back from the stamped source.
///
/// NaN when it is no longer a DC source, which no headroom check can pass. On
/// an adjustable regulator the source's `n` is ADJ or a private node, so the
/// board return the card named is returned in its place: everything downstream
/// measures the rail and its headroom against the same node.
#[must_use]
pub fn terminals(circuit: &Circuit, reg: &SimpleRegulator) -> (NodeId, NodeId, f64) {
    match &circuit.devices[reg.source.0 as usize] {
        Device::Vsource { p, n, kind, .. } => {
            let n = reg.adj_return.unwrap_or(*n);
            match kind {
                SourceKind::Dc(v) => (*p, n, *v),
                _ => (*p, n, f64::NAN),
            }
        }
        _ => (NodeId::GROUND, NodeId::GROUND, f64::NAN),
    }
}

#[must_use]
pub fn headroom_ok(reg: &SimpleRegulator, headroom_v: f64) -> bool {
    // A datasheet-guaranteed row sits exactly on the bound (VI = VO + dropout),
    // and the solved input carries Newton residual, so a bare `>=` refuses a row
    // the part is specified to meet. The slack is relative to the bound, a few
    // orders above solver noise and far below any shortfall worth reporting: a
    // 7 V bound admits 6.99999999 V and still refuses 6.9999 V.
    let slack = |bound: f64| bound.abs().max(1.0) * 1e-9;
    headroom_v.is_finite()
        && headroom_v >= reg.min_in_v - slack(reg.min_in_v)
        && headroom_v <= reg.max_in_v + slack(reg.max_in_v)
}

/// Decide `supported` from scratch, re-enabling regulators to a fixpoint.
///
/// Works on a CLONE with every regulator source swapped in place for a 0 A
/// current source (device ids stay valid), re-enabling only regulators whose
/// input passes, to a fixpoint: a cascade validates in dependency order and a
/// regulator-only cycle never gains a member.
///
/// A `barred` (already tainted) regulator stays removed, so nothing it feeds
/// regains support. At most `regs.len() + 1` solves; returns the verdict of each
/// one left unsupported.
pub fn evaluate(
    circuit: &Circuit,
    regs: &mut [SimpleRegulator],
    supported: &mut [bool],
    barred: &[bool],
    opts: &SolverOptions,
) -> Vec<(usize, Verdict)> {
    supported.fill(false);
    // An adjustable regulator's rail is the board divider's, so the input it
    // needs is a SOLVED value rather than the bind-time constant a fixed part
    // has: read |V(out) - V(return)| off the circuit as stamped, before the swap
    // below removes the reference, and write it back so the headroom test, the
    // post-chunk monitor and the refusal text all quote one figure.
    let rail = |r: &SimpleRegulator| {
        let (p, n, _) = terminals(circuit, r);
        Probe::NodeDiff(circuit.node_name(p).into(), circuit.node_name(n).into())
    };
    let adj = regs.iter().any(|r| r.adj_return.is_some());
    let solved = adj.then(|| run_op(circuit, opts, &regs.iter().map(rail).collect::<Vec<_>>()));
    for (i, reg) in regs.iter_mut().enumerate() {
        if let (Some(Ok(v)), Some(_)) = (&solved, reg.adj_return) {
            reg.min_in_v = v.rows[0][i].abs() + reg.dropout_v.unwrap_or(0.0);
        }
    }
    let regs = &*regs;
    let (mut probe, mut probes, mut signs) = (circuit.clone(), Vec::new(), Vec::new());
    for reg in regs {
        let (p, n, vout) = terminals(circuit, reg);
        let name = |node| circuit.node_name(node).to_string();
        probes.push(Probe::NodeDiff(name(reg.input), name(n)));
        signs.push(vout.signum());
        let (kind, id) = (SourceKind::Dc(0.0), reg.source.0 as usize);
        let name = circuit.devices[id].name().to_string();
        if p != n {
            probe.devices[id] = Device::Isource { name, p, n, kind };
        }
        // The pass draw reads the branch this swap just removed, and IADJ would keep
        // pushing into a divider the part no longer drives: both go with it,
        // onto ground, where a 0 A source connects nothing at all.
        for h in reg.helpers.iter().map(|h| h.0 as usize) {
            let name = circuit.devices[h].name().to_string();
            let (p, n, kind) = (NodeId::GROUND, NodeId::GROUND, SourceKind::Dc(0.0));
            probe.devices[h] = Device::Isource { name, p, n, kind };
        }
    }
    // The swap would orphan a branch-current reader (F/H/B/K). A regulator's
    // own pass draw is one BY CONSTRUCTION and is neutralised with the
    // source above; any other reader is unaccounted for, and nothing is granted.
    let helper = |id| regs.iter().any(|r| r.helpers.contains(&id));
    let mut readers = circuit
        .iter()
        .filter(|(id, _)| !helper(*id))
        .flat_map(|(_, dev)| dev.controlling_sources());
    let orphaned = readers.any(|id| regs.iter().any(|r| r.source == id));
    loop {
        let solved = if orphaned {
            Err("a controlled source reads a regulator's branch current".to_string())
        } else {
            run_op(&probe, opts, &probes).map_err(|e| e.to_string())
        };
        let (mut unsupported, mut granted) = (Vec::new(), false);
        for (i, reg) in regs.iter().enumerate() {
            if supported[i] || barred[i] {
                continue;
            }
            let verdict = solved.as_ref().map(|out| signs[i] * out.rows[0][i]);
            if verdict.is_ok_and(|v| headroom_ok(reg, v)) {
                (supported[i], granted) = (true, true);
                for id in std::iter::once(&reg.source).chain(&reg.helpers) {
                    let id = id.0 as usize;
                    probe.devices[id] = circuit.devices[id].clone();
                }
            } else {
                unsupported.push((i, verdict.map_err(Clone::clone)));
            }
        }
        if !granted || unsupported.is_empty() {
            return unsupported;
        }
    }
}

/// The one disclosure every surface repeats. `revoked_at_s` marks a verdict
/// from the post-chunk monitor (the full solve) rather than from [`evaluate`].
#[must_use]
pub fn reason(
    circuit: &Circuit,
    reg: &SimpleRegulator,
    verdict: &Verdict,
    revoked_at_s: Option<f64>,
) -> String {
    let (p, n, vout) = terminals(circuit, reg);
    let (part, input, max) = (&reg.reference, circuit.node_name(reg.input), reg.max_in_v);
    let out = circuit.node_name(if p.is_ground() { n } else { p });
    let side = if vout < 0.0 { "below" } else { "above" };
    let seen = revoked_at_s.map_or_else(
        || "with regulator ideal sources removed".to_string(),
        |t| format!("first at t={t:.6} s"),
    );
    // An adjustable part has no output limit and no rail on its card: `min_in_v`
    // is the rail the BOARD's divider produced and `vout` is only the reference
    // held across it, so neither may be worded as a setpoint for the output net.
    let (limit, held) = match reg.adj_return {
        None => (
            "upper output limit",
            format!("The {vout:.3} V on {out} is the ideal source's setpoint"),
        ),
        Some(_) => (
            "solved output",
            format!(
                "The voltage on {out} is set by an ideal {vout:.3} V reference across this \
                 part's external divider"
            ),
        ),
    };
    let need = reg.dropout_v.map_or_else(
        || format!("{limit}; the model has no dropout figure"),
        |d| {
            format!(
                "{:.3} V {limit} + {d:.3} V declared dropout",
                reg.min_in_v - d
            )
        },
    );
    let lone = |why: String| {
        format!(
            "input {input} has no independent DC support ({why}); declare the supply that feeds {input}"
        )
    };
    let clause = match verdict {
        Err(e) => lone(e.clone()),
        Ok(x) if x.abs() < 1e-3 && revoked_at_s.is_none() => lone(format!("{x:.3} V {seen}")),
        Ok(x) if *x > max => {
            format!(
                "input {input} is {x:.3} V {side} its return, {seen}, exceeding the {max:.3} V rating"
            )
        }
        Ok(x) => format!(
            "input {input} is {x:.3} V {side} its return, {seen}, below the {:.3} V this model needs \
             ({need}, not a guaranteed bound)",
            reg.min_in_v
        ),
    };
    format!(
        "{part} regulation of {out} is unsupported: {clause}. {held}, \
         not a simulated result; below-dropout, startup and recovery behaviour \
         are not modelled. Results on {out} and everything it powers are invalid for this run, \
         and stay invalid if the input recovers; start a fresh run."
    )
}

/// The structural walk both taints share: non-source conduction paths (so it
/// covers loads switched on later), stopping at ground, at `supply_nets`, and at
/// terminals of still-trusted voltage sources.
pub struct Walk {
    stop: Vec<NodeId>,
    next: HashMap<NodeId, Vec<NodeId>>,
}

impl Walk {
    #[must_use]
    pub fn new(
        circuit: &Circuit,
        regs: &[SimpleRegulator],
        supported: &[bool],
        supply_nets: &[NodeId],
    ) -> Self {
        let mut stop = [&[NodeId::GROUND], supply_nets].concat();
        let distrusted = |id| {
            regs.iter()
                .zip(supported)
                .any(|(r, ok)| !ok && r.source == id)
        };
        // A regulator's pass draw is a source, not a path: read as one IN <->
        // ADJ (or IN <-> its return) it would leak the regulator's taint onto
        // its own input net.
        let helper = |id| regs.iter().any(|r| r.helpers.contains(&id));
        let mut next: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
        for (id, dev) in circuit.iter() {
            match dev {
                Device::Vsource { p, n, .. } if !distrusted(id) => stop.extend([*p, *n]),
                _ if helper(id) => {}
                Device::Vsource { .. } | Device::Isource { .. } | Device::Cccs { .. } => {}
                _ => {
                    let nodes = dev.conduction_nodes();
                    for at in &nodes {
                        next.entry(*at).or_default().extend(&nodes);
                    }
                }
            }
        }
        Self { stop, next }
    }

    /// The nodes this walk never enters: ground, the declared supply nets and
    /// the terminals of every still-trusted voltage source. Something holds
    /// each of them independently of what is wired to it.
    #[must_use]
    pub fn stops(&self) -> &[NodeId] {
        &self.stop
    }

    fn reach(&self, mut seen: Vec<NodeId>) -> Vec<NodeId> {
        let mut at = 0;
        while let Some(node) = seen.get(at).copied() {
            at += 1;
            for x in self.next.get(&node).into_iter().flatten() {
                if !self.stop.contains(x) && !seen.contains(x) {
                    seen.push(*x);
                }
            }
        }
        seen
    }

    /// Everything regulator `reg`'s output can power.
    #[must_use]
    pub fn regulator(&self, circuit: &Circuit, reg: &SimpleRegulator) -> Vec<NodeId> {
        let (p, n, _) = terminals(circuit, reg);
        let seed = |x: &NodeId| !self.stop.contains(x) || (*x == p && !p.is_ground());
        self.reach([p, n].into_iter().filter(seed).collect())
    }

    /// What an unmodelled part could drive if it were present, or `None` when
    /// the evidence says it cannot: no pin on a `powered` node (nothing to
    /// drive WITH, so it can only load), or no drive-capable pin off the
    /// reference nodes. Returns the powering node and the reached nodes.
    pub fn open_driver(
        &self,
        part: &OpenPart,
        powered: &dyn Fn(NodeId) -> bool,
    ) -> Option<(NodeId, Vec<NodeId>)> {
        let from = part.pins.iter().map(|p| p.0).find(|n| powered(*n))?;
        let mut seeds = Vec::new();
        for (node, drives) in &part.pins {
            if *drives && !self.stop.contains(node) && !seeds.contains(node) {
                seeds.push(*node);
            }
        }
        (!seeds.is_empty()).then(|| (from, self.reach(seeds)))
    }
}

/// Marks [`open_driver_reason`], for the surfaces that pick a next action by it.
pub const OPEN_DRIVER: &str = "has no model, so it is absent from the simulated circuit";

/// The one disclosure every surface repeats for an open part that would drive.
#[must_use]
pub fn open_driver_reason(r: &str, v: &str, from: &str, nets: &[String]) -> String {
    let more = nets.len().saturating_sub(6);
    let mut list = nets[..nets.len() - more].join(", ");
    if more > 0 {
        let _ = write!(list, " and {more} more");
    }

    format!(
        "{r} ({v}) {OPEN_DRIVER}, yet it is powered from {from} and can drive {list}. Any \
         voltage printed for those nets is the board without {r}, not a simulated result: a \
         failure there proves nothing, and a pass holds only if {r} leaves them alone. Give {r} a model (`hauksbee models new`, or \
         `hauksbee models extract` from its datasheet), or declare the supply for the rail it \
         produces, then rerun."
    )
}

#[cfg(test)]
mod tests {
    // MIC5205-3.3 as bound: 18 V regulates (the envelope flags it), past 20 V abs max revokes.
    #[test]
    fn regulator_input_limit_is_the_abs_max() {
        let reg = super::SimpleRegulator {
            reference: "U1".into(),
            source: hauksbee_ir::DeviceId(0),
            input: super::NodeId::GROUND,
            min_in_v: 3.65,
            max_in_v: 20.0,
            dropout_v: Some(0.35),
            adj_return: None,
            helpers: Vec::new(),
            pass_level: None,
            enable: Vec::new(),
        };
        let ok = |v| super::headroom_ok(&reg, v);
        assert_eq!([3.6, 18.0, 21.0].map(ok), [false, true, false]);
    }
}
