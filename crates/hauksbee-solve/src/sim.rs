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
//! `.op`, a [`crate::Waveforms`] for `.tran`).
//!
//! Everything here is a wrapper over code that already exists. It adds no
//! physics; it only routes.

use crate::{dc_operating_point, AcAnalysis, AcSpec, SolverOptions, Sweep, Transient, Workspace};
use hauksbee_ir::{AcDirective, AcSweep, Circuit, DcDirective, Device, NodeId, SourceKind};

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

/// Every non-ground node voltage, in node order; the sane default when the
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

    // Honesty gate: `dc_operating_point` may return Ok with the staged-DC
    // relaxed surrogate (every diode replaced by 1 GΩ) when the true nonlinear
    // DC never converges. That is the right contract for transient SEEDING
    // (the march relaxes it to the true state), but a `.op` REPORT must only
    // present a genuine root of the actual nonlinear circuit. Check the real
    // KCL residual of the adopted point. The bound is a small absolute
    // current, 1e-6 A: every genuinely converged ladder outcome (including
    // the homotopy / gmin-ramp sub-attempts that also set `used_staged_dc`)
    // lands at nA or below; the opt-in ResidualAccept path certifies roots
    // at `residual_accept_tol` = 1e-9 A, while a forced-OFF forward diode
    // leaves an amp-scale imbalance. 1e-6 A keeps three orders of headroom on
    // each side and is far above `opts.abstol` (1e-12 A) so legitimate
    // step-norm-terminated solves are never rejected. Do NOT reject on
    // `used_staged_dc()` alone: most staged outcomes are true roots.
    const OP_KCL_TOL: f64 = 1e-6;
    let res = ws.dc_residual_inf_norm(circuit, opts);
    if !res.is_finite() || res > OP_KCL_TOL {
        let (worst, node) = ws.dc_residual_argmax(circuit, opts);
        let via = if ws.used_staged_dc() {
            " (a staged-DC relaxed surrogate was adopted; it seeds transients \
             but is not a converged operating point)"
        } else {
            ""
        };
        return Err(format!(
            ".op did not converge: KCL residual {worst:.3e} A at unknown #{node} \
             exceeds {OP_KCL_TOL:.0e} A{via}"
        ));
    }

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
                    format!(
                        "element `{d}` carries no branch current (only V-sources and inductors do)"
                    )
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
                    format!(
                        "element `{d}` carries no branch current (only V-sources and inductors do)"
                    )
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

/// The values a single `.dc` source visits, `start` to `stop` inclusive by
/// `step`. The loader guarantees a nonzero step whose sign reaches `stop`.
///
/// The interval count is floored WITH a small tolerance: `(stop-start)/step` is
/// mathematically an integer for a well-formed sweep (e.g. `0..1 step 0.1` → 10),
/// but `step` values like 0.1 are not exactly representable in f64, so the ratio
/// can land at `9.9999999…` and a bare `floor()` would drop the final `stop`
/// point. Adding a relative epsilon before flooring keeps the intended endpoint.
fn dc_sweep_values(start: f64, stop: f64, step: f64) -> Vec<f64> {
    let ratio = (stop - start) / step;
    // Tolerance scaled to the magnitude of the ratio so it stays effective for
    // large sweeps, but never large enough to invent an extra point.
    let eps = 1e-9 * ratio.abs().max(1.0);
    let n = (ratio + eps).floor().max(0.0) as u64;
    (0..=n).map(|i| start + step * i as f64).collect()
}

/// Overwrite an independent source's value in a scratch circuit for one sweep
/// point. The loader guaranteed the swept device is a V/I source.
fn set_source_value(circuit: &mut Circuit, id: hauksbee_ir::DeviceId, value: f64) {
    match &mut circuit.devices[id.0 as usize] {
        Device::Vsource { kind, .. } | Device::Isource { kind, .. } => {
            *kind = SourceKind::Dc(value);
        }
        _ => unreachable!("`.dc` sweep target resolved to a non-source device"),
    }
}

/// Run a `.dc` sweep: loop the operating point, re-stamping the swept source's
/// value at every point. A nested (second) source wraps the inner one, and the
/// inner source is the reported sweep axis (SPICE convention). Blocks for each
/// outer value are concatenated. The first output column is the inner swept
/// value; the remaining columns are the probes.
pub fn run_dc(
    circuit: &Circuit,
    opts: &SolverOptions,
    dc: &DcDirective,
    probes: &[Probe],
) -> Result<SimOutput, String> {
    let inner_vals = dc_sweep_values(dc.inner.start, dc.inner.stop, dc.inner.step);
    let outer_vals = match &dc.outer {
        Some(o) => dc_sweep_values(o.start, o.stop, o.step),
        None => vec![f64::NAN], // one pass, no outer source touched
    };

    let mut columns = Vec::with_capacity(probes.len() + 2);
    columns.push(dc.inner.name.clone());
    columns.extend(probes.iter().map(Probe::label));
    // For a NESTED sweep, carry the outer coordinate as a trailing column so
    // each row records which outer value it belongs to. Without it the outer
    // source was silently dropped and the concatenated blocks read as one
    // non-monotonic sweep (the inner axis repeated per outer point).
    if let Some(outer) = &dc.outer {
        columns.push(outer.name.clone());
    }

    // A scratch circuit we re-stamp in place, so each point solves a fresh OP.
    let mut scratch = circuit.clone();
    let mut rows = Vec::with_capacity(inner_vals.len() * outer_vals.len());
    for &ov in &outer_vals {
        if let Some(outer) = &dc.outer {
            set_source_value(&mut scratch, outer.source, ov);
        }
        for &iv in &inner_vals {
            set_source_value(&mut scratch, dc.inner.source, iv);
            let point = run_op(&scratch, opts, probes)?;
            let mut row = Vec::with_capacity(probes.len() + 2);
            row.push(iv);
            row.extend_from_slice(&point.rows[0]);
            if dc.outer.is_some() {
                row.push(ov);
            }
            rows.push(row);
        }
    }

    Ok(SimOutput {
        columns,
        time: None,
        rows,
    })
}

/// Run a `.ac` sweep against the existing [`AcAnalysis`], reading magnitude
/// (linear) and phase (degrees) at each probe per frequency. The first output
/// column is `frequency`; each probe contributes a magnitude column and a
/// `<probe>:phase_deg` column.
///
/// Refuses (rather than fakes) when the deck carries no AC stimulus: a `.ac`
/// with no `AC`-tagged source would run with a zero drive and hand back an
/// all-zeros answer that looks like a result but is not one.
pub fn run_ac(
    circuit: &Circuit,
    opts: &SolverOptions,
    ac: &AcDirective,
    probes: &[Probe],
) -> Result<SimOutput, String> {
    if circuit.ac_stimulus.is_empty() {
        return Err(
            "`.ac` analysis has no AC stimulus: no source carries an `AC <mag> [phase]` \
             spec, so the small-signal drive is identically zero and the response would be \
             a meaningless all-zeros table. Add `AC 1` to the driving source (e.g. \
             `VIN in 0 AC 1`)."
                .to_string(),
        );
    }

    // AC responds at nodes; a branch-current phasor is not captured here.
    for p in probes {
        if let Probe::BranchCurrent(d) = p {
            return Err(format!(
                "AC output `I({d})` is not supported: the AC analysis reports node-voltage \
                 phasors, not branch currents. Probe a node voltage (e.g. `V(out)`)."
            ));
        }
    }

    let sweep = match ac.sweep {
        AcSweep::Decade => Sweep::Decade,
        AcSweep::Octave => Sweep::Octave,
        AcSweep::Linear => Sweep::Linear,
    };
    let spec = AcSpec {
        fstart: ac.fstart,
        fstop: ac.fstop,
        points: ac.points,
        sweep,
    };
    let resp = AcAnalysis::new(opts.clone()).run(circuit, &spec)?;

    // Resolve each probe's node id(s) once, up front, so a typo fails cleanly.
    enum AcTarget {
        Node(NodeId),
        Diff(NodeId, NodeId),
    }
    let mut targets = Vec::with_capacity(probes.len());
    let mut columns = vec!["frequency".to_string()];
    for p in probes {
        match p {
            Probe::NodeVoltage(a) => targets.push(AcTarget::Node(resolve_node(circuit, a)?)),
            Probe::NodeDiff(a, b) => targets.push(AcTarget::Diff(
                resolve_node(circuit, a)?,
                resolve_node(circuit, b)?,
            )),
            Probe::BranchCurrent(_) => unreachable!("refused above"),
        }
        columns.push(p.label());
        columns.push(format!("{}:phase_deg", p.label()));
    }

    let phasor = |pt: &crate::AcPoint, id: NodeId| -> num_complex::Complex64 {
        if id.is_ground() {
            num_complex::Complex64::new(0.0, 0.0)
        } else {
            pt.node_phasor
                .get(id.0 as usize)
                .copied()
                .unwrap_or_else(|| num_complex::Complex64::new(0.0, 0.0))
        }
    };

    let mut rows = Vec::with_capacity(resp.points.len());
    for pt in &resp.points {
        let mut row = Vec::with_capacity(1 + 2 * probes.len());
        row.push(pt.freq);
        for t in &targets {
            let v = match t {
                AcTarget::Node(id) => phasor(pt, *id),
                AcTarget::Diff(a, b) => phasor(pt, *a) - phasor(pt, *b),
            };
            row.push(v.norm());
            row.push(v.arg().to_degrees());
        }
        rows.push(row);
    }

    Ok(SimOutput {
        columns,
        time: None,
        rows,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use hauksbee_ir::SpiceLoader;

    #[test]
    fn dc_sweep_walks_the_source_and_reads_probes() {
        // VCVS gain 2: out = 2 * in, swept 0..3 V by 1 V.
        let net = "dc\nVin in 0 DC 0\nE1 out 0 in 0 2.0\nRl out 0 1k\n\
                   .dc Vin 0 3 1\n.end\n";
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
        assert_eq!(out.rows.len(), 4); // 0,1,2,3
        for (i, row) in out.rows.iter().enumerate() {
            let vin = i as f64;
            assert!((row[0] - vin).abs() < 1e-9, "sweep value");
            assert!((row[1] - 2.0 * vin).abs() < 1e-6, "gain-2 output");
        }
    }

    #[test]
    fn dc_sweep_includes_endpoint_under_float_drift() {
        // 0..1 by 0.1: the ratio (1-0)/0.1 is mathematically 10 but lands at
        // 9.9999999… in f64, so a bare floor() dropped the final 1.0 point (10
        // points instead of 11). The tolerance keeps the endpoint.
        let vals = dc_sweep_values(0.0, 1.0, 0.1);
        assert_eq!(vals.len(), 11, "must include both endpoints: {vals:?}");
        assert!((vals.first().copied().unwrap()).abs() < 1e-12);
        assert!(
            (vals.last().copied().unwrap() - 1.0).abs() < 1e-9,
            "stop endpoint present"
        );
        // A non-integer number of steps must NOT gain a spurious extra point:
        // 0..0.95 by 0.1 covers 0.0..0.9 (10 points), not 1.05.
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
        // inner 0,1,2 (3 pts) x outer 0,1 (2 pts) = 6 rows; the inner value is
        // the reported sweep axis and repeats per outer block.
        assert_eq!(out.rows.len(), 6);
        let axis: Vec<f64> = out.rows.iter().map(|r| r[0]).collect();
        assert_eq!(axis, vec![0.0, 1.0, 2.0, 0.0, 1.0, 2.0]);
        // R13: the outer coordinate is carried as a trailing column, so each row
        // records which outer block it belongs to (was silently dropped).
        assert_eq!(out.columns, vec!["Vin", "V(in)", "Vg"]);
        let outer_col = out.columns.iter().position(|c| c == "Vg").unwrap();
        let outer: Vec<f64> = out.rows.iter().map(|r| r[outer_col]).collect();
        assert_eq!(outer, vec![0.0, 0.0, 0.0, 1.0, 1.0, 1.0]);
    }

    #[test]
    fn ac_refuses_a_deck_with_no_ac_stimulus() {
        // A `.ac` deck whose sources carry no `AC` spec would run with a zero
        // drive, refuse rather than hand back all zeros.
        let net = "ac\nVin in 0 DC 1\nR1 in out 1k\nC1 out 0 159.155n\n\
                   .ac dec 5 10 1e6\n.end\n";
        let (c, d) = SpiceLoader::load_with_directives(net).unwrap();
        let probes = [Probe::NodeVoltage("out".into())];
        let err = run_ac(
            &c,
            &SolverOptions::default(),
            d.ac.as_ref().unwrap(),
            &probes,
        )
        .unwrap_err();
        assert!(err.contains("no AC stimulus"), "{err}");
    }

    #[test]
    fn ac_lowpass_hits_the_corner() {
        // RC low-pass with fc = 1 kHz: at the corner |H| ~ 0.707 and phase ~ -45.
        // A 2-point linear sweep 1 kHz .. 2 kHz: the first point is exactly fc.
        let net = "ac\nVin in 0 AC 1\nR1 in out 1k\nC1 out 0 159.155n\n\
                   .ac lin 2 1000 2000\n.end\n";
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
        assert!((row[0] - 1000.0).abs() < 1e-6, "first freq is the corner");
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
