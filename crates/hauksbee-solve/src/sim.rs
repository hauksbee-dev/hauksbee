//! Deck-to-results glue shared by the `hauksbee sim` CLI and the ngspice
//! differential harness.
//!
//! The loader ([`hauksbee_ir::SpiceLoader`]) turns a `.cir` into a [`Circuit`]
//! plus its directives; the solver ([`Transient`] / [`dc_operating_point`])
//! turns a circuit into waveforms or an operating point. This module is the
//! thin seam between them: it parses probe expressions (`V(a)`, `V(a,b)`,
//! `I(V1)`), runs the requested analysis, and returns a column-per-probe,
//! row-per-timepoint [`SimOutput`] that a CSV writer or a comparison harness
//! can consume without re-deriving how to read node voltages and branch
//! currents out of the two very different result shapes (an `x` vector for
//! `.op`, a [`Waveforms`] for `.tran`).
//!
//! Everything here is a wrapper over code that already exists. It adds no
//! physics; it only routes.

use crate::{dc_operating_point, SolverOptions, Transient, Workspace};
use hauksbee_ir::{Circuit, NodeId};

/// One output quantity requested from a run.
#[derive(Debug, Clone, PartialEq)]
pub enum Probe {
    /// `V(node)`: the node voltage (referenced to ground).
    NodeVoltage(String),
    /// `V(a,b)`: the difference `V(a) - V(b)`.
    NodeDiff(String, String),
    /// `I(name)`: the branch current of a voltage source or inductor named
    /// `name` (the only devices that carry a branch-current unknown).
    BranchCurrent(String),
}

impl Probe {
    /// The canonical column label, e.g. `V(out)`, `V(a,b)`, `I(V1)`.
    pub fn label(&self) -> String {
        match self {
            Probe::NodeVoltage(a) => format!("V({a})"),
            Probe::NodeDiff(a, b) => format!("V({a},{b})"),
            Probe::BranchCurrent(d) => format!("I({d})"),
        }
    }

    /// Parse one probe token. Accepts `V(a)`, `V(a,b)`, `I(name)` (any case for
    /// the `V`/`I` head), or a bare node name (treated as `V(name)`).
    pub fn parse(s: &str) -> Result<Probe, String> {
        let t = s.trim();
        if t.is_empty() {
            return Err("empty probe".to_string());
        }
        // Split a `HEAD(args)` call, keeping the original-case args.
        if let Some(open) = t.find('(') {
            if !t.ends_with(')') {
                return Err(format!("probe `{t}`: missing closing `)`"));
            }
            let head = t[..open].trim().to_ascii_lowercase();
            let inner = t[open + 1..t.len() - 1].trim();
            match head.as_str() {
                "v" => {
                    let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
                    match parts.as_slice() {
                        [a] if !a.is_empty() => Ok(Probe::NodeVoltage((*a).to_string())),
                        [a, b] if !a.is_empty() && !b.is_empty() => {
                            Ok(Probe::NodeDiff((*a).to_string(), (*b).to_string()))
                        }
                        _ => Err(format!(
                            "probe `{t}`: V() takes one node `V(a)` or two `V(a,b)`"
                        )),
                    }
                }
                "i" => {
                    if inner.is_empty() || inner.contains(',') {
                        Err(format!("probe `{t}`: I() takes one element name `I(V1)`"))
                    } else {
                        Ok(Probe::BranchCurrent(inner.to_string()))
                    }
                }
                other => Err(format!(
                    "probe `{t}`: unknown output function `{other}` (use V(...) or I(...))"
                )),
            }
        } else {
            // Bare node name.
            Ok(Probe::NodeVoltage(t.to_string()))
        }
    }
}

/// The result of a run: one column per probe, one row per timepoint (transient)
/// or one row (operating point). `time` is `Some` only for a transient.
#[derive(Debug, Clone)]
pub struct SimOutput {
    /// Column labels, one per probe, in request order.
    pub columns: Vec<String>,
    /// Sample times (s) for a transient; `None` for an operating point.
    pub time: Option<Vec<f64>>,
    /// `rows[i][j]` is probe `j`'s value at sample `i`.
    pub rows: Vec<Vec<f64>>,
}

impl SimOutput {
    /// The full series for column `label`, if present (handy for the harness).
    pub fn column(&self, label: &str) -> Option<Vec<f64>> {
        let j = self.columns.iter().position(|c| c == label)?;
        Some(self.rows.iter().map(|r| r[j]).collect())
    }
}

/// Every non-ground node voltage, in node order — the sane default when the
/// deck carries no `.print` and the user passed no `--print`.
pub fn default_probes(circuit: &Circuit) -> Vec<Probe> {
    (1..circuit.node_count())
        .map(|i| Probe::NodeVoltage(circuit.node_name(NodeId(i as u32)).to_string()))
        .collect()
}

/// Resolve a node name to its [`NodeId`], case-insensitively, honoring the `0`
/// and `gnd` ground aliases. Errors (never silently substitutes) if absent.
fn resolve_node(circuit: &Circuit, name: &str) -> Result<NodeId, String> {
    if name == "0" || name.eq_ignore_ascii_case("gnd") || name.eq_ignore_ascii_case("ground") {
        return Ok(NodeId::GROUND);
    }
    for id in 0..circuit.node_count() {
        let nid = NodeId(id as u32);
        if circuit.node_name(nid).eq_ignore_ascii_case(name) {
            return Ok(nid);
        }
    }
    Err(format!("no node named `{name}` in the deck"))
}

/// Resolve an element name to the device index that owns a branch-current
/// unknown, or an error naming why it does not.
fn resolve_branch_device(circuit: &Circuit, name: &str) -> Result<hauksbee_ir::DeviceId, String> {
    for (id, dev) in circuit.iter() {
        if dev.name().eq_ignore_ascii_case(name) {
            return Ok(id);
        }
    }
    Err(format!("no element named `{name}` in the deck"))
}

/// Run the DC operating point and read the requested probes off the solved
/// unknown vector. `Err` on non-convergence or an unresolvable probe.
pub fn run_op(
    circuit: &Circuit,
    opts: &SolverOptions,
    probes: &[Probe],
) -> Result<SimOutput, String> {
    let mut ws = Workspace::new(circuit);
    dc_operating_point(&mut ws, circuit, opts)?;

    let node_v = |name: &str| -> Result<f64, String> {
        let id = resolve_node(circuit, name)?;
        Ok(match ws.layout.node(id) {
            None => 0.0, // ground
            Some(i) => ws.x[i],
        })
    };

    let mut columns = Vec::with_capacity(probes.len());
    let mut row = Vec::with_capacity(probes.len());
    for p in probes {
        columns.push(p.label());
        let v = match p {
            Probe::NodeVoltage(a) => node_v(a)?,
            Probe::NodeDiff(a, b) => node_v(a)? - node_v(b)?,
            Probe::BranchCurrent(d) => {
                let id = resolve_branch_device(circuit, d)?;
                let bi = ws.layout.branch(id).ok_or_else(|| {
                    format!("element `{d}` carries no branch current (only V-sources and inductors do)")
                })?;
                ws.x[bi]
            }
        };
        row.push(v);
    }
    Ok(SimOutput {
        columns,
        time: None,
        rows: vec![row],
    })
}

/// Run a transient to `tstop` and read the requested probes off the waveforms.
/// `Err` on a solver failure or an unresolvable probe.
pub fn run_tran(
    circuit: &Circuit,
    opts: &SolverOptions,
    tstop: f64,
    probes: &[Probe],
) -> Result<SimOutput, String> {
    let wf = Transient::new(opts.clone()).run(circuit, tstop)?;
    let n = wf.time.len();

    let node_series = |name: &str| -> Result<Vec<f64>, String> {
        let id = resolve_node(circuit, name)?;
        if id.is_ground() {
            return Ok(vec![0.0; n]);
        }
        wf.node_voltages
            .get(id.0 as usize)
            .cloned()
            .ok_or_else(|| format!("node `{name}` has no waveform"))
    };

    let mut columns = Vec::with_capacity(probes.len());
    let mut series: Vec<Vec<f64>> = Vec::with_capacity(probes.len());
    for p in probes {
        columns.push(p.label());
        let s = match p {
            Probe::NodeVoltage(a) => node_series(a)?,
            Probe::NodeDiff(a, b) => {
                let sa = node_series(a)?;
                let sb = node_series(b)?;
                sa.iter().zip(&sb).map(|(x, y)| x - y).collect()
            }
            Probe::BranchCurrent(d) => wf
                .branch_currents
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(d))
                .map(|(_, v)| v.clone())
                .ok_or_else(|| {
                    format!("element `{d}` carries no branch current (only V-sources and inductors do)")
                })?,
        };
        series.push(s);
    }

    let mut rows = Vec::with_capacity(n);
    for i in 0..n {
        rows.push(series.iter().map(|s| s[i]).collect());
    }
    Ok(SimOutput {
        columns,
        time: Some(wf.time.clone()),
        rows,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_probe_forms() {
        assert_eq!(Probe::parse("V(out)").unwrap(), Probe::NodeVoltage("out".into()));
        assert_eq!(
            Probe::parse("v(a,b)").unwrap(),
            Probe::NodeDiff("a".into(), "b".into())
        );
        assert_eq!(Probe::parse("I(V1)").unwrap(), Probe::BranchCurrent("V1".into()));
        assert_eq!(Probe::parse("n20").unwrap(), Probe::NodeVoltage("n20".into()));
        assert!(Probe::parse("V(a").is_err());
        assert!(Probe::parse("X(a)").is_err());
    }
}
