//! Deck-to-results glue shared by `hauksbee sim` and the ngspice harness: parse
//! probes (`V(a)`, `V(a,b)`, `I(V1)`), run the requested analysis, and return a
//! column-per-probe [`SimOutput`]. No physics lives here; it only routes.

use crate::{
    dc_operating_point, AcAnalysis, AcSpec, SolveError, SolvePhase, SolveResult, SolverOptions,
    Sweep, Transient, Workspace,
};
use hauksbee_ir::evidence::{
    ErrorBudget, IntegrationMethod, IntegrationTolerance, Residual, TimeWindow, WindowMethod,
};
use hauksbee_ir::{AcDirective, AcSweep, Circuit, DcDirective, Device, NodeId, SourceKind};

/// The KCL residual a reported operating point must satisfy (1e-6 A): far above
/// `abstol`, far below the amp-scale imbalance a forced-off junction leaves.
/// `newton.rs` uses the same bound to declare convergence.
pub(crate) const OP_KCL_TOL: f64 = 1e-6;

/// One output quantity requested from a run.
#[derive(Debug, Clone, PartialEq)]
pub enum Probe {
    /// `V(node)`.
    NodeVoltage(String),
    /// `V(a,b)` = `V(a) - V(b)`.
    NodeDiff(String, String),
    /// `I(name)`: the branch current of a voltage source or inductor.
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

    /// Parse `V(a)`, `V(a,b)`, `I(name)` (any case), or a bare node name.
    pub fn parse(s: &str) -> SolveResult<Probe> {
        let t = s.trim();
        if t.is_empty() {
            return Err(SolveError::invalid("empty probe"));
        }
        let Some(open) = t.find('(') else {
            return Ok(Probe::NodeVoltage(t.to_string()));
        };
        if !t.ends_with(')') {
            return Err(SolveError::invalid(format!(
                "probe `{t}`: missing closing `)`"
            )));
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
                    _ => Err(SolveError::invalid(format!(
                        "probe `{t}`: V() takes one node `V(a)` or two `V(a,b)`"
                    ))),
                }
            }
            "i" if !inner.is_empty() && !inner.contains(',') => {
                Ok(Probe::BranchCurrent(inner.to_string()))
            }
            "i" => Err(SolveError::invalid(format!(
                "probe `{t}`: I() takes one element name `I(V1)`"
            ))),
            other => Err(SolveError::invalid(format!(
                "probe `{t}`: unknown output function `{other}` (use V(...) or I(...))"
            ))),
        }
    }
}

/// One column per probe, one row per sample; `time` is `Some` for a transient.
#[derive(Debug, Clone)]
pub struct SimOutput {
    pub columns: Vec<String>,
    pub time: Option<Vec<f64>>,
    /// `rows[i][j]` is probe `j` at sample `i`.
    pub rows: Vec<Vec<f64>>,
    /// Numerical provenance: tolerances, plus a residual and time window when
    /// this path measured them.
    pub error_budget: ErrorBudget,
}

impl SimOutput {
    /// The full series for column `label`, if present.
    pub fn column(&self, label: &str) -> Option<Vec<f64>> {
        let j = self.columns.iter().position(|c| c == label)?;
        Some(self.rows.iter().map(|r| r[j]).collect())
    }
}

fn integration_method(method: crate::Integration) -> IntegrationMethod {
    match method {
        crate::Integration::Trapezoidal => IntegrationMethod::Trapezoidal,
        crate::Integration::Gear2 => IntegrationMethod::Gear2,
        crate::Integration::BackwardEuler => IntegrationMethod::BackwardEuler,
    }
}

pub(crate) fn error_budget(opts: &SolverOptions) -> SolveResult<ErrorBudget> {
    IntegrationTolerance::new(opts.reltol, opts.vntol, opts.abstol, opts.chgtol)
        .map(ErrorBudget::new)
        .map_err(|error| SolveError::invalid(format!("invalid solver error budget: {error}")))
}

fn residual_label(circuit: &Circuit, layout: &crate::Layout, unknown: usize) -> String {
    (1..circuit.node_count())
        .map(|n| NodeId(n as u32))
        .find(|n| layout.node(*n) == Some(unknown))
        .map(|n| circuit.node_name(n).to_string())
        .unwrap_or_else(|| format!("unknown #{unknown}"))
}

fn residual(max_abs: f64, label: String) -> SolveResult<Residual> {
    Residual::new(max_abs, label)
        .map_err(|error| SolveError::internal(format!("invalid residual: {error}")))
}

/// Every non-ground node voltage, in node order: the default probe set.
pub fn default_probes(circuit: &Circuit) -> Vec<Probe> {
    (1..circuit.node_count())
        .map(|i| Probe::NodeVoltage(circuit.node_name(NodeId(i as u32)).to_string()))
        .collect()
}

fn resolve_node(circuit: &Circuit, name: &str) -> SolveResult<NodeId> {
    if name.eq_ignore_ascii_case("ground") {
        return Ok(NodeId::GROUND);
    }
    circuit
        .find_node(name)
        .ok_or_else(|| SolveError::invalid(format!("no node named `{name}` in the deck")))
}

fn resolve_branch_device(circuit: &Circuit, name: &str) -> SolveResult<hauksbee_ir::DeviceId> {
    circuit
        .iter()
        .find(|(_, dev)| dev.name().eq_ignore_ascii_case(name))
        .map(|(id, _)| id)
        .ok_or_else(|| SolveError::invalid(format!("no element named `{name}` in the deck")))
}

fn no_branch(d: &str) -> SolveError {
    SolveError::invalid(format!(
        "element `{d}` carries no branch current (only V-sources and inductors do)"
    ))
}

/// A `.temp` card sets the analysis temperature unless the caller already moved
/// `temperature_c` off the 27 C default (an explicit control outranks the deck).
fn opts_with_deck_temp(circuit: &Circuit, opts: &SolverOptions) -> SolverOptions {
    let mut o = opts.clone();
    if o.temperature_c == 27.0 && circuit.temp_c != 27.0 {
        o.temperature_c = circuit.temp_c;
    }
    o
}

/// Run the DC operating point and read the probes off the solved vector.
pub fn run_op(circuit: &Circuit, opts: &SolverOptions, probes: &[Probe]) -> SolveResult<SimOutput> {
    let opts = &opts_with_deck_temp(circuit, opts);
    let mut ws = Workspace::new(circuit);
    dc_operating_point(&mut ws, circuit, opts)?;

    // `dc_operating_point` may return Ok with a staged-DC relaxed surrogate
    // (fine for seeding a transient); a reported `.op` must be a genuine root,
    // so check the real KCL residual of the adopted point.
    let res = ws.dc_residual_inf_norm(circuit, opts);
    if !res.is_finite() || res > OP_KCL_TOL {
        let (worst, node) = ws.dc_residual_argmax(circuit, opts);
        let via = if ws.used_staged_dc() {
            " (a staged-DC relaxed surrogate was adopted; it seeds transients \
             but is not a converged operating point)"
        } else {
            ""
        };
        return Err(SolveError::NonConvergence {
            message: format!(
                ".op did not converge: KCL residual {worst:.3e} A at unknown #{node} \
                 exceeds {OP_KCL_TOL:.0e} A{via}"
            ),
            phase: SolvePhase::Dc,
            time: None,
            dt: None,
            iterations: None,
            blame: None,
        });
    }

    let node_v = |name: &str| -> SolveResult<f64> {
        let id = resolve_node(circuit, name)?;
        Ok(ws.layout.node(id).map_or(0.0, |i| ws.x[i]))
    };
    let mut row = Vec::with_capacity(probes.len());
    for p in probes {
        row.push(match p {
            Probe::NodeVoltage(a) => node_v(a)?,
            Probe::NodeDiff(a, b) => node_v(a)? - node_v(b)?,
            Probe::BranchCurrent(d) => {
                let id = resolve_branch_device(circuit, d)?;
                ws.x[ws.layout.branch(id).ok_or_else(|| no_branch(d))?]
            }
        });
    }
    let (max_abs, at) = ws.dc_residual_argmax(circuit, opts);
    let mut budget = error_budget(opts)?;
    if max_abs.is_finite() {
        budget = budget.with_residual(residual(max_abs, residual_label(circuit, &ws.layout, at))?);
    }
    Ok(SimOutput {
        columns: probes.iter().map(Probe::label).collect(),
        time: None,
        rows: vec![row],
        error_budget: budget,
    })
}

/// Run a transient to `tstop` and read the probes off the waveforms.
pub fn run_tran(
    circuit: &Circuit,
    opts: &SolverOptions,
    tstop: f64,
    probes: &[Probe],
) -> SolveResult<SimOutput> {
    let effective = opts_with_deck_temp(circuit, opts);
    let (wf, diagnostics) = Transient::new(effective).run_with_diagnostics(circuit, tstop)?;
    let n = wf.time.len();

    let node_series = |name: &str| -> SolveResult<Vec<f64>> {
        let id = resolve_node(circuit, name)?;
        if id.is_ground() {
            return Ok(vec![0.0; n]);
        }
        wf.node_voltages
            .get(id.0 as usize)
            .cloned()
            .ok_or_else(|| SolveError::internal(format!("node `{name}` has no waveform")))
    };
    let mut series: Vec<Vec<f64>> = Vec::with_capacity(probes.len());
    for p in probes {
        series.push(match p {
            Probe::NodeVoltage(a) => node_series(a)?,
            Probe::NodeDiff(a, b) => {
                let (sa, sb) = (node_series(a)?, node_series(b)?);
                sa.iter().zip(&sb).map(|(x, y)| x - y).collect()
            }
            Probe::BranchCurrent(d) => wf
                .branch_currents
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(d))
                .map(|(_, v)| v.clone())
                .ok_or_else(|| no_branch(d))?,
        });
    }
    let rows = (0..n)
        .map(|i| series.iter().map(|s| s[i]).collect())
        .collect();

    let window = TimeWindow::new(0.0, tstop)
        .map_err(|error| SolveError::internal(format!("invalid transient window: {error}")))?;
    let method = WindowMethod::new(window, integration_method(opts.integration))
        .map_err(|error| SolveError::internal(format!("invalid transient method: {error}")))?;
    let mut budget = error_budget(&opts_with_deck_temp(circuit, opts))?.with_method(method);
    if let Some((max_abs, at)) = diagnostics
        .final_residual
        .filter(|(value, _)| value.is_finite())
    {
        let layout = crate::Layout::new(circuit);
        budget = budget.with_residual(residual(max_abs, residual_label(circuit, &layout, at))?);
    }
    Ok(SimOutput {
        columns: probes.iter().map(Probe::label).collect(),
        time: Some(wf.time.clone()),
        rows,
        error_budget: budget,
    })
}

/// `start` to `stop` inclusive by `step`. The interval count is floored with a
/// small tolerance so a ratio like `9.999999…` (0.1 is inexact) keeps `stop`.
fn dc_sweep_values(start: f64, stop: f64, step: f64) -> Vec<f64> {
    let ratio = (stop - start) / step;
    let eps = 1e-9 * ratio.abs().max(1.0);
    let n = (ratio + eps).floor().max(0.0) as u64;
    (0..=n).map(|i| start + step * i as f64).collect()
}

fn set_source_value(circuit: &mut Circuit, id: hauksbee_ir::DeviceId, value: f64) {
    match &mut circuit.devices[id.0 as usize] {
        Device::Vsource { kind, .. } | Device::Isource { kind, .. } => {
            *kind = SourceKind::Dc(value);
        }
        _ => unreachable!("`.dc` sweep target resolved to a non-source device"),
    }
}

/// Run a `.dc` sweep: an operating point per sweep value, re-stamping the
/// source each time. Columns: the inner swept value, the probes, and (for a
/// nested sweep) the outer value; the outer sweep's blocks are concatenated.
pub fn run_dc(
    circuit: &Circuit,
    opts: &SolverOptions,
    dc: &DcDirective,
    probes: &[Probe],
) -> SolveResult<SimOutput> {
    let inner_vals = dc_sweep_values(dc.inner.start, dc.inner.stop, dc.inner.step);
    let outer_vals = match &dc.outer {
        Some(o) => dc_sweep_values(o.start, o.stop, o.step),
        None => vec![f64::NAN],
    };
    let mut columns = vec![dc.inner.name.clone()];
    columns.extend(probes.iter().map(Probe::label));
    if let Some(outer) = &dc.outer {
        columns.push(outer.name.clone());
    }

    let mut scratch = circuit.clone();
    let mut rows = Vec::with_capacity(inner_vals.len() * outer_vals.len());
    let mut worst: Option<Residual> = None;
    for &ov in &outer_vals {
        if let Some(outer) = &dc.outer {
            set_source_value(&mut scratch, outer.source, ov);
        }
        for &iv in &inner_vals {
            set_source_value(&mut scratch, dc.inner.source, iv);
            let point = run_op(&scratch, opts, probes)?;
            if let Some(r) = point.error_budget.residual() {
                if worst.as_ref().is_none_or(|w| r.max_abs() > w.max_abs()) {
                    worst = Some(r.clone());
                }
            }
            let mut row = vec![iv];
            row.extend_from_slice(&point.rows[0]);
            if dc.outer.is_some() {
                row.push(ov);
            }
            rows.push(row);
        }
    }
    let mut budget = error_budget(&opts_with_deck_temp(circuit, opts))?;
    if let Some(r) = worst {
        budget = budget.with_residual(r);
    }
    Ok(SimOutput {
        columns,
        time: None,
        rows,
        error_budget: budget,
    })
}

/// Run a `.ac` sweep: per frequency, each probe's magnitude (linear) and phase
/// (degrees) as two columns after `frequency`. Refuses a deck with no `AC`
/// stimulus (the response would be an all-zeros non-answer) and branch-current
/// probes (the AC analysis reports node phasors only).
pub fn run_ac(
    circuit: &Circuit,
    opts: &SolverOptions,
    ac: &AcDirective,
    probes: &[Probe],
) -> SolveResult<SimOutput> {
    if circuit.ac_stimulus.is_empty() {
        return Err(SolveError::refused(
            "`.ac` analysis has no AC stimulus: no source carries an `AC <mag> [phase]` \
             spec, so the small-signal drive is identically zero and the response would be \
             a meaningless all-zeros table. Add `AC 1` to the driving source (e.g. \
             `VIN in 0 AC 1`).",
        ));
    }
    enum Target {
        Node(NodeId),
        Diff(NodeId, NodeId),
    }
    let mut targets = Vec::with_capacity(probes.len());
    let mut columns = vec!["frequency".to_string()];
    for p in probes {
        targets.push(match p {
            Probe::NodeVoltage(a) => Target::Node(resolve_node(circuit, a)?),
            Probe::NodeDiff(a, b) => {
                Target::Diff(resolve_node(circuit, a)?, resolve_node(circuit, b)?)
            }
            Probe::BranchCurrent(d) => {
                return Err(SolveError::refused(format!(
                    "AC output `I({d})` is not supported: the AC analysis reports node-voltage \
                     phasors, not branch currents. Probe a node voltage (e.g. `V(out)`)."
                )))
            }
        });
        columns.push(p.label());
        columns.push(format!("{}:phase_deg", p.label()));
    }

    let spec = AcSpec {
        fstart: ac.fstart,
        fstop: ac.fstop,
        points: ac.points,
        sweep: match ac.sweep {
            AcSweep::Decade => Sweep::Decade,
            AcSweep::Octave => Sweep::Octave,
            AcSweep::Linear => Sweep::Linear,
        },
    };
    let resp = AcAnalysis::new(opts_with_deck_temp(circuit, opts)).run(circuit, &spec)?;
    let phasor = |pt: &crate::AcPoint, id: NodeId| -> num_complex::Complex64 {
        if id.is_ground() {
            return num_complex::Complex64::new(0.0, 0.0);
        }
        pt.node_phasor
            .get(id.0 as usize)
            .copied()
            .unwrap_or_else(|| num_complex::Complex64::new(0.0, 0.0))
    };
    let rows = resp
        .points
        .iter()
        .map(|pt| {
            let mut row = vec![pt.freq];
            for t in &targets {
                let v = match t {
                    Target::Node(id) => phasor(pt, *id),
                    Target::Diff(a, b) => phasor(pt, *a) - phasor(pt, *b),
                };
                row.push(v.norm());
                row.push(v.arg().to_degrees());
            }
            row
        })
        .collect();
    Ok(SimOutput {
        columns,
        time: None,
        rows,
        error_budget: error_budget(&opts_with_deck_temp(circuit, opts))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use hauksbee_ir::SpiceLoader;

    #[test]
    fn dc_sweep_walks_the_source_and_reads_probes() {
        let net = "dc\nVin in 0 DC 0\nE1 out 0 in 0 2.0\nRl out 0 1k\n.dc Vin 0 3 1\n.end\n";
        let (c, d) = SpiceLoader::load_with_directives(net).unwrap();
        let probes = [Probe::NodeVoltage("out".into())];
        let out = run_dc(
            &c,
            &SolverOptions::default(),
            d.dc.as_ref().unwrap(),
            &probes,
        )
        .unwrap();
        assert_eq!(out.columns, vec!["Vin", "V(out)"]);
        assert_eq!(out.rows.len(), 4);
        for (i, row) in out.rows.iter().enumerate() {
            let vin = i as f64;
            assert!((row[0] - vin).abs() < 1e-9, "sweep value");
            assert!((row[1] - 2.0 * vin).abs() < 1e-6, "gain-2 output");
        }
    }

    #[test]
    fn dc_sweep_includes_endpoint_under_float_drift() {
        let vals = dc_sweep_values(0.0, 1.0, 0.1);
        assert_eq!(vals.len(), 11, "must include both endpoints: {vals:?}");
        assert!((vals.last().copied().unwrap() - 1.0).abs() < 1e-9);
        let vals2 = dc_sweep_values(0.0, 0.95, 0.1);
        assert_eq!(vals2.len(), 10, "no phantom endpoint: {vals2:?}");
    }

    #[test]
    fn dc_nested_sweep_concatenates_blocks() {
        let net = "dc\nVin in 0 DC 0\nVg g 0 DC 0\nRi in 0 1k\nRg g 0 1k\n\
                   .dc Vin 0 2 1 Vg 0 1 1\n.end\n";
        let (c, d) = SpiceLoader::load_with_directives(net).unwrap();
        let probes = [Probe::NodeVoltage("in".into())];
        let out = run_dc(
            &c,
            &SolverOptions::default(),
            d.dc.as_ref().unwrap(),
            &probes,
        )
        .unwrap();
        assert_eq!(out.rows.len(), 6);
        let axis: Vec<f64> = out.rows.iter().map(|r| r[0]).collect();
        assert_eq!(axis, vec![0.0, 1.0, 2.0, 0.0, 1.0, 2.0]);
        assert_eq!(out.columns, vec!["Vin", "V(in)", "Vg"]);
        let outer: Vec<f64> = out.rows.iter().map(|r| r[2]).collect();
        assert_eq!(outer, vec![0.0, 0.0, 0.0, 1.0, 1.0, 1.0]);
    }

    #[test]
    fn ac_refuses_a_deck_with_no_ac_stimulus() {
        let net = "ac\nVin in 0 DC 1\nR1 in out 1k\nC1 out 0 159.155n\n.ac dec 5 10 1e6\n.end\n";
        let (c, d) = SpiceLoader::load_with_directives(net).unwrap();
        let probes = [Probe::NodeVoltage("out".into())];
        let err = run_ac(
            &c,
            &SolverOptions::default(),
            d.ac.as_ref().unwrap(),
            &probes,
        )
        .unwrap_err();
        assert!(err.to_string().contains("no AC stimulus"), "{err}");
    }

    #[test]
    fn ac_lowpass_hits_the_corner() {
        // fc = 1 kHz: |H| ~ 0.707 and phase ~ -45 deg at the first sweep point.
        let net = "ac\nVin in 0 AC 1\nR1 in out 1k\nC1 out 0 159.155n\n.ac lin 2 1000 2000\n.end\n";
        let (c, d) = SpiceLoader::load_with_directives(net).unwrap();
        let probes = [Probe::NodeVoltage("out".into())];
        let out = run_ac(
            &c,
            &SolverOptions::default(),
            d.ac.as_ref().unwrap(),
            &probes,
        )
        .unwrap();
        assert_eq!(out.columns, vec!["frequency", "V(out)", "V(out):phase_deg"]);
        let row = &out.rows[0];
        assert!((row[0] - 1000.0).abs() < 1e-6);
        assert!(
            (row[1] - std::f64::consts::FRAC_1_SQRT_2).abs() < 1e-3,
            "mag {}",
            row[1]
        );
        assert!((row[2] + 45.0).abs() < 0.2, "phase {}", row[2]);
    }

    #[test]
    fn parse_probe_forms() {
        assert_eq!(
            Probe::parse("V(out)").unwrap(),
            Probe::NodeVoltage("out".into())
        );
        assert_eq!(
            Probe::parse("v(a,b)").unwrap(),
            Probe::NodeDiff("a".into(), "b".into())
        );
        assert_eq!(
            Probe::parse("I(V1)").unwrap(),
            Probe::BranchCurrent("V1".into())
        );
        assert_eq!(
            Probe::parse("n20").unwrap(),
            Probe::NodeVoltage("n20".into())
        );
        assert!(Probe::parse("V(a").is_err());
        assert!(Probe::parse("X(a)").is_err());
    }
}
