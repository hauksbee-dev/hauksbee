//! Simple-regulator validity. A simple vreg is an ideal DC source that ignores
//! its input, so its setpoint is only a result when the input has INDEPENDENT
//! headroom. This module establishes that or reports why not; it never
//! substitutes a voltage of its own for an unsupported output.

use hauksbee_bind::bind_report::SimpleRegulator;
use hauksbee_ir::{Circuit, Device, NodeId, SourceKind};
use hauksbee_solve::{run_op, Probe, SolverOptions};

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

/// `(output, return, signed setpoint)` read back from the stamped source. NaN
/// when it is no longer a DC source, which no headroom check can pass.
pub fn terminals(circuit: &Circuit, reg: &SimpleRegulator) -> (NodeId, NodeId, f64) {
    match &circuit.devices[reg.source.0 as usize] {
        Device::Vsource { p, n, kind, .. } => match kind {
            SourceKind::Dc(v) => (*p, *n, *v),
            _ => (*p, *n, f64::NAN),
        },
        _ => (NodeId::GROUND, NodeId::GROUND, f64::NAN),
    }
}

pub fn headroom_ok(reg: &SimpleRegulator, headroom_v: f64) -> bool {
    headroom_v.is_finite() && (reg.min_in_v..=reg.max_in_v).contains(&headroom_v)
}

/// Decide `supported` from scratch on a CLONE with every regulator source swapped
/// in place for a 0 A current source (device ids stay valid), re-enabling only
/// regulators whose input passes, to a fixpoint: a cascade validates in dependency
/// order and a regulator-only cycle never gains a member. A `barred` (already
/// tainted) regulator stays removed, so nothing it feeds regains support. At most
/// `regs.len() + 1` solves; returns the verdict of each one left unsupported.
pub fn evaluate(
    circuit: &Circuit,
    regs: &[SimpleRegulator],
    supported: &mut [bool],
    barred: &[bool],
    opts: &SolverOptions,
) -> Vec<(usize, Verdict)> {
    supported.fill(false);
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
    }
    // The swap would orphan a branch-current reader (F/H/B/K). No binder path
    // stamps one on a regulator source; if one ever does, nothing is granted.
    let mut readers = circuit.devices.iter().flat_map(Device::controlling_sources);
    let orphaned = readers.any(|id| regs.iter().any(|r| r.source == id));
    loop {
        let solved = match orphaned {
            true => Err("a controlled source reads a regulator's branch current".to_string()),
            false => run_op(&probe, opts, &probes).map_err(|e| e.to_string()),
        };
        let (mut unsupported, mut granted) = (Vec::new(), false);
        for (i, reg) in regs.iter().enumerate() {
            if supported[i] || barred[i] {
                continue;
            }
            let verdict = solved.as_ref().map(|out| signs[i] * out.rows[0][i]);
            if verdict.is_ok_and(|v| headroom_ok(reg, v)) {
                (supported[i], granted) = (true, true);
                let id = reg.source.0 as usize;
                probe.devices[id] = circuit.devices[id].clone();
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
pub fn reason(
    circuit: &Circuit,
    reg: &SimpleRegulator,
    verdict: &Verdict,
    revoked_at_s: Option<f64>,
) -> String {
    let (p, n, vout) = terminals(circuit, reg);
    let (r, i, max) = (&reg.reference, circuit.node_name(reg.input), reg.max_in_v);
    let out = circuit.node_name(if p.is_ground() { n } else { p });
    let side = if vout < 0.0 { "below" } else { "above" };
    let seen = revoked_at_s.map_or_else(
        || "with regulator ideal sources removed".to_string(),
        |t| format!("first at t={t:.6} s"),
    );
    let need = match reg.dropout_v {
        None => "upper output limit; the model has no dropout figure".to_string(),
        Some(d) => format!(
            "{:.3} V upper output limit + {d:.3} V typical dropout",
            reg.min_in_v - d
        ),
    };
    let lone = |why: String| {
        format!(
            "input {i} has no independent DC support ({why}); declare the supply that feeds {i}"
        )
    };
    let clause = match verdict {
        Err(e) => lone(e.clone()),
        Ok(x) if x.abs() < 1e-3 && revoked_at_s.is_none() => lone(format!("{x:.3} V {seen}")),
        Ok(x) if *x > max => {
            format!(
                "input {i} is {x:.3} V {side} its return, {seen}, exceeding the {max:.3} V rating"
            )
        }
        Ok(x) => format!(
            "input {i} is {x:.3} V {side} its return, {seen}, below the {:.3} V this model needs \
             ({need}, not a guaranteed bound)",
            reg.min_in_v
        ),
    };
    format!(
        "{r} regulation of {out} is unsupported: {clause}. The {vout:.3} V on {out} is the ideal \
         source's setpoint, not a simulated result; below-dropout, startup and recovery behaviour \
         are not modelled. Results on {out} and everything it powers are invalid for this run, \
         and stay invalid if the input recovers; start a fresh run."
    )
}

/// Everything regulator `index`'s output can power: a structural walk over
/// non-source conduction paths (so it covers loads switched on later), stopping at
/// ground, at `supply_nets`, and at terminals of still-trusted voltage sources.
pub fn tainted_nodes(
    circuit: &Circuit,
    regs: &[SimpleRegulator],
    supported: &[bool],
    supply_nets: &[NodeId],
    index: usize,
) -> Vec<NodeId> {
    let mut stop = [&[NodeId::GROUND], supply_nets].concat();
    let distrusted = |id| {
        regs.iter()
            .zip(supported)
            .any(|(r, ok)| !ok && r.source == id)
    };
    for (id, dev) in circuit.iter() {
        if let (Device::Vsource { p, n, .. }, false) = (dev, distrusted(id)) {
            stop.extend([*p, *n]);
        }
    }
    let (p, n, _) = terminals(circuit, &regs[index]);
    let seed = |x: &NodeId| !stop.contains(x) || (*x == p && !p.is_ground());
    let mut seen: Vec<NodeId> = [p, n].into_iter().filter(seed).collect();
    let mut next = 0;
    while let Some(&at) = seen.get(next) {
        next += 1;
        for dev in &circuit.devices {
            let nodes = dev.conduction_nodes();
            let source = matches!(dev, Device::Vsource { .. } | Device::Isource { .. });
            if !source && nodes.contains(&at) {
                let fresh = |x: &NodeId| !stop.contains(x) && !seen.contains(x);
                let fresh: Vec<_> = nodes.into_iter().filter(fresh).collect();
                seen.extend(fresh);
            }
        }
    }
    seen
}
