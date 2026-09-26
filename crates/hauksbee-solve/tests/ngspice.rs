//! The ngspice differential harness (SPICE-compat plan §6).
//!
//! For every `.cir` in `tests/decks/` with a companion `<name>.expect.toml`,
//! this runs ngspice `-b` and the hauksbee solver over the SAME deck, resamples
//! ngspice onto our timebase, and compares each declared probe against its
//! declared PER-QUANTITY tolerance (a diode forward drop wants mV; an RC tail
//! only percent, one global bar cannot serve both). Each deck emits a per-deck
//! pass/fail with its worst-case error and where it occurred. Results go under
//! `target/oracle`; set `HAUKSBEE_NGSPICE_REPORT` to explicitly refresh a document.
//! Correctness-oracle output is content-addressed, not recomputed unchanged.
//!
//! ngspice lookup: `$NGSPICE`, then `PATH`, then known per-OS locations.
//! Missing ngspice skips locally; `HAUKSBEE_REQUIRE_NGSPICE=1` makes it fatal.

use hauksbee_ir::{Directives, SpiceLoader};
use hauksbee_solve::{
    run_ac, run_dc, run_op, run_tran, DcInit, Integration, Probe, SimOutput, SolverOptions,
    StepControl,
};
use serde::Deserialize;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
#[path = "support/oracle.rs"]
mod oracle;
use oracle::{find_ngspice, ngspice_version, parse_indexed_table, parse_op_listing, CachedOracle};

// --- ngspice output parsing --------------------------------------------------

/// A parsed `.print tran` column: (times, values).
struct NgSeries {
    t: Vec<f64>,
    v: Vec<f64>,
}

/// Parse the `Index time value` block ngspice prints for `.print tran`.
fn parse_tran_table(text: &str) -> Option<NgSeries> {
    let (t, v): (Vec<_>, Vec<_>) = oracle::parse_tran_table(text).into_iter().unzip();
    (!t.is_empty()).then_some(NgSeries { t, v })
}

/// Linear interpolation, clamped at the sampled endpoints.
fn interp(res: &NgSeries, time: f64) -> f64 {
    let hi = res.t.partition_point(|&t| t <= time);
    if hi == 0 || hi == res.t.len() {
        return res.v[hi.saturating_sub(1)];
    }
    let lo = hi - 1;
    let span = res.t[hi] - res.t[lo];
    if span == 0.0 {
        return res.v[hi];
    }
    res.v[lo] + (res.v[hi] - res.v[lo]) * (time - res.t[lo]) / span
}

// --- deck / expectation model ------------------------------------------------

#[derive(Deserialize)]
struct Expect {
    analysis: String,
    description: String,
    #[serde(default)]
    full_scale: f64,
    #[serde(default)]
    skip_initial: f64,
    probe: Vec<ProbeExpect>,
}

#[derive(Deserialize)]
struct ProbeExpect {
    expr: String,
    /// Magnitude relative tolerance (also the DC / transient tolerance).
    reltol: f64,
    #[serde(default)]
    abstol: Option<f64>,
    /// AC phase tolerance in degrees (absolute). Required for `.ac` probes.
    #[serde(default)]
    phase_abstol: Option<f64>,
    /// CLOSED-FORM expectations for this probe, checked in addition to the
    /// oracle comparison. See [`AnalyticPoint`].
    #[serde(default)]
    analytic: Vec<AnalyticPoint>,
    /// Why this probe's bound is allowed to sit further than [`MAX_HEADROOM`]
    /// above the error actually measured. Absent means it is not: see
    /// [`headroom_verdict`].
    #[serde(default)]
    loose_reason: Option<String>,
}

/// One closed-form expectation: at sweep point `at` (seconds for `tran`, volts for
/// `dc`, hertz for `ac`), the probe must equal `value`.
///
/// Analytic expectations outrank a second implementation: the synapse oracle's
/// switch-straddling timestep caused an 87.5 mV error; refining its grid improved
/// it 10x. These are model-equation checks, not physical measurements.
#[derive(Deserialize)]
struct AnalyticPoint {
    /// Sweep coordinate: seconds (`tran`), the swept source value (`dc`), hertz
    /// (`ac`), ignored for `op`.
    #[serde(default)]
    at: f64,
    /// The closed-form value.
    value: f64,
    /// Relative tolerance on `value`.
    reltol: f64,
    /// Absolute floor, for a `value` near zero.
    #[serde(default)]
    abstol: Option<f64>,
    /// The derivation, so a reader can check the number rather than trust it.
    /// Required: an unexplained analytic constant is indistinguishable from a
    /// value copied out of a previous run.
    note: String,
}

/// Maximum tolerance/error ratio: generous to adaptive-grid and platform changes,
/// but not the historical 250,000x headroom that hid regressions. Probes needing
/// more must justify their bound with `loose_reason`.
const MAX_HEADROOM: f64 = 50.0;

/// Do not tighten below 1e-6 relative just because solvers agree to rounding noise
/// (e.g. exact divider V(in), or 7.5e-10 error for `param_divider`).
const HEADROOM_FLOOR: f64 = 1e-6;

/// Is this probe's tolerance tight enough to be a gate? `None` when it is (or is
/// waived), `Some(message)` naming the problem when it is not.
fn headroom_verdict(pe: &ProbeExpect, worst: f64) -> Option<String> {
    if pe.loose_reason.is_some() {
        return None;
    }
    let bar = (worst * MAX_HEADROOM).max(HEADROOM_FLOOR);
    if pe.reltol <= bar {
        return None;
    }
    Some(format!(
        "tolerance {:.1e} is {:.0}x the measured error {:.3e}, which cannot catch a \
         regression; tighten it to <= {:.1e} or state `loose_reason = \"...\"`",
        pe.reltol,
        if worst > 0.0 {
            pe.reltol / worst
        } else {
            f64::INFINITY
        },
        worst,
        bar,
    ))
}

/// A per-quantity comparison result for the table.
struct QtyResult {
    probe: String,
    worst_err: f64,
    at: String,
    reltol: f64,
    pass: bool,
    /// Set when the bound is too loose to gate anything (see [`headroom_verdict`]).
    too_loose: Option<String>,
}

struct DeckResult {
    name: String,
    analysis: String,
    description: String,
    quantities: Vec<QtyResult>,
}

impl DeckResult {
    fn passed(&self) -> bool {
        self.quantities.iter().all(|q| q.pass)
    }
}

/// Build solver options from the deck's directives, exactly as the CLI does:
/// the deck's tolerances, `uic` -> power-on start, and an adaptive step bounded
/// by the requested `.tran` step.
fn opts_from_directives(directives: &Directives) -> SolverOptions {
    let mut opts = SolverOptions::default();
    if let Some(r) = directives.reltol {
        opts.reltol = r;
    }
    if let Some(a) = directives.abstol {
        opts.abstol = a;
    }
    if let Some(v) = directives.vntol {
        opts.vntol = v;
    }
    if let Some(td) = directives.tran {
        let dt_max = td.tmax.unwrap_or(td.tstep).max(1e-15);
        opts.integration = Integration::Trapezoidal;
        opts.step = StepControl::Adaptive {
            dt_initial: (td.tstep / 100.0).max(1e-15),
            dt_min: 1e-15,
            dt_max,
        };
    }
    if directives.use_initial_conditions {
        opts.dc_init = DcInit::FromZero;
    }
    opts
}

/// Shared body of the ngspice deck rewriters: strip `.print`/`.plot`, then splice
/// `print_card` in just before `.end`.
fn rewrite_with_print(orig: &str, print_card: &str) -> String {
    let mut out = String::new();
    let mut injected = false;
    for line in orig.lines() {
        let l = line.trim_start().to_ascii_lowercase();
        if l.starts_with(".print") || l.starts_with(".plot") {
            continue;
        }
        if l.split_whitespace().next() == Some(".end") && !injected {
            out.push_str(print_card);
            out.push('\n');
            injected = true;
        }
        out.push_str(line);
        out.push('\n');
    }
    if !injected {
        out.push_str(print_card);
        out.push_str("\n.end\n");
    }
    out
}

/// Worst relative error of `ours` vs interpolated ngspice, with a floor so
/// zero-crossings don't blow the ratio up: the probe's own `abstol` when the
/// expect file states one (the op-branch convention, extended to tran for
/// probes whose off-state is an exponential tail toward zero, a switch
/// drain settling to millivolts turns sub-mV agreement into O(1) "relative"
/// noise without it), else 1% of `full_scale`. Returns (worst, `at_time`).
fn worst_tran_error(
    ours_t: &[f64],
    ours_v: &[f64],
    ng: &NgSeries,
    full_scale: f64,
    skip_initial: f64,
    abstol: Option<f64>,
) -> (f64, f64) {
    let floor = abstol.unwrap_or(if full_scale > 0.0 {
        0.01 * full_scale
    } else {
        1e-9
    });
    worst_error(ours_t, ours_v, ng, skip_initial, |ours, want| {
        (ours - want).abs() / want.abs().max(floor)
    })
}

/// Worst relative error of `ours(xs)` vs ngspice interpolated onto `xs`, with an
/// absolute floor so a near-zero magnitude does not blow the ratio up. Used for
/// DC-sweep and AC-magnitude columns. Returns `(worst, at_x)`.
fn worst_rel_error(xs: &[f64], ours: &[f64], ng: &NgSeries, floor: f64) -> (f64, f64) {
    worst_error(xs, ours, ng, f64::NEG_INFINITY, |ours, want| {
        (ours - want).abs() / want.abs().max(floor)
    })
}

/// Worst ABSOLUTE error of `ours(xs)` vs ngspice interpolated onto `xs`. Used for
/// AC phase (degrees), where an absolute tolerance is the honest bar.
fn worst_abs_error(xs: &[f64], ours: &[f64], ng: &NgSeries) -> (f64, f64) {
    worst_error(xs, ours, ng, f64::NEG_INFINITY, |ours, want| {
        (ours - want).abs()
    })
}

fn worst_error(
    xs: &[f64],
    ours: &[f64],
    ng: &NgSeries,
    skip: f64,
    error: impl Fn(f64, f64) -> f64,
) -> (f64, f64) {
    assert_eq!(xs.len(), ours.len(), "comparison axis/value lengths");
    xs.iter()
        .zip(ours)
        .filter(|(x, _)| **x >= skip)
        .map(|(&x, &value)| {
            let err = error(value, interp(ng, x));
            (if err.is_finite() { err } else { f64::INFINITY }, x)
        })
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .unwrap_or((f64::INFINITY, 0.0))
}

/// Check every closed-form expectation on one probe, appending a result row each.
///
/// `xs`/`vs` are OUR series on our own sweep axis; the analytic value is compared
/// at `at` by interpolating our series there, the same way the oracle comparison
/// resamples. For `op` there is no axis and the single sample is used.
fn check_analytic(
    out: &mut Vec<QtyResult>,
    label: &str,
    pe: &ProbeExpect,
    xs: &[f64],
    vs: &[f64],
    axis: &str,
) {
    for (k, a) in pe.analytic.iter().enumerate() {
        assert!(
            !a.note.trim().is_empty(),
            "{label}: analytic point {k} needs a `note` deriving its value"
        );
        let ours = if xs.len() <= 1 {
            vs[0]
        } else {
            interp(
                &NgSeries {
                    t: xs.to_vec(),
                    v: vs.to_vec(),
                },
                a.at,
            )
        };
        let floor = a.abstol.unwrap_or(1e-12);
        let err = (ours - a.value).abs() / a.value.abs().max(floor);
        out.push(QtyResult {
            probe: format!("{label} vs closed form"),
            worst_err: err,
            at: format!("{axis}={:.3e}", a.at),
            reltol: a.reltol,
            pass: err < a.reltol,
            // Closed-form bounds are justified by model accuracy, not oracle drift.
            too_loose: None,
        });
    }
}

/// Look up a probe's value in a parsed `.op` listing.
fn op_value(probe: &Probe, nodes: &HashMap<String, f64>) -> Option<f64> {
    let node = |n: &str| -> Option<f64> {
        if n == "0" || n.eq_ignore_ascii_case("gnd") {
            Some(0.0)
        } else {
            nodes.get(&n.to_ascii_lowercase()).copied()
        }
    };
    match probe {
        Probe::NodeVoltage(a) => node(a),
        Probe::NodeDiff(a, b) => Some(node(a)? - node(b)?),
        Probe::BranchCurrent(d) => nodes
            .get(&format!("{}#branch", d.to_ascii_lowercase()))
            .copied(),
    }
}

// --- the harness proper ------------------------------------------------------

fn decks_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/decks")
}

/// Run one deck end-to-end; missing, invalid or failed oracle output is fatal.
#[allow(clippy::too_many_lines)] // one deck: load, per-analysis oracle compare, record
fn run_deck(oracle: &CachedOracle, cir_path: &Path) -> DeckResult {
    let name = cir_path.file_stem().unwrap().to_string_lossy().to_string();
    let deck = std::fs::read_to_string(cir_path).expect("read deck");
    // A deck using a card ngspice has no element for (the `A` op-amp) ships a
    // companion `<name>.ngspice.cir`: same model form, same node names.
    let ng_src = std::fs::read_to_string(cir_path.with_extension("ngspice.cir"))
        .unwrap_or_else(|_| deck.clone());
    let expect_path = cir_path.with_extension("expect.toml");
    let expect_text = std::fs::read_to_string(&expect_path)
        .unwrap_or_else(|_| panic!("missing {}", expect_path.display()));
    let expect: Expect = toml::from_str(&expect_text)
        .unwrap_or_else(|e| panic!("parse {}: {e}", expect_path.display()));

    let (circuit, directives) = SpiceLoader::load_file_with_directives(cir_path)
        .unwrap_or_else(|e| panic!("load {name}: {e}"));
    let opts = opts_from_directives(&directives);

    let probes: Vec<Probe> = expect
        .probe
        .iter()
        .map(|p| Probe::parse(&p.expr).expect("valid probe expr"))
        .collect();

    let mut quantities = Vec::new();

    match expect.analysis.as_str() {
        "op" => {
            let ours: SimOutput = run_op(&circuit, &opts, &probes)
                .unwrap_or_else(|e| panic!("{name}: hauksbee op failed: {e}"));
            let ng_text = oracle.run(&ng_src).expect("ngspice op run");
            let nodes = parse_op_listing(&ng_text).expect("finite ngspice op listing");
            for (pe, pr) in expect.probe.iter().zip(&probes) {
                let ours_val = ours.column(&pr.label()).unwrap()[0];
                let ng_val = op_value(pr, &nodes).unwrap_or_else(|| {
                    panic!("{name}: ngspice op listing has no value for {}", pr.label())
                });
                let floor = pe.abstol.unwrap_or(1e-9);
                let err = (ours_val - ng_val).abs() / ng_val.abs().max(floor);
                quantities.push(QtyResult {
                    probe: pr.label(),
                    worst_err: err,
                    at: "op".to_string(),
                    reltol: pe.reltol,
                    pass: err < pe.reltol,
                    too_loose: headroom_verdict(pe, err),
                });
                check_analytic(&mut quantities, &pr.label(), pe, &[0.0], &[ours_val], "op");
            }
        }
        "tran" => {
            let td = directives
                .tran
                .unwrap_or_else(|| panic!("{name}: tran deck has no .tran card"));
            let ours: SimOutput = run_tran(&circuit, &opts, td.tstop, &probes)
                .unwrap_or_else(|e| panic!("{name}: hauksbee tran failed: {e}"));
            let ours_t = ours.time.clone().unwrap();
            for (pe, pr) in expect.probe.iter().zip(&probes) {
                let ng_deck = rewrite_with_print(&ng_src, &format!(".print tran {}", pr.label()));
                let ng = oracle
                    .run(&ng_deck)
                    .and_then(|txt| parse_tran_table(&txt))
                    .unwrap_or_else(|| panic!("{name}: no ngspice table for {}", pr.label()));
                let ours_v = ours.column(&pr.label()).unwrap();
                let (worst, at) = worst_tran_error(
                    &ours_t,
                    &ours_v,
                    &ng,
                    expect.full_scale,
                    expect.skip_initial,
                    pe.abstol,
                );
                quantities.push(QtyResult {
                    probe: pr.label(),
                    worst_err: worst,
                    at: format!("t={at:.3e}s"),
                    reltol: pe.reltol,
                    pass: worst < pe.reltol,
                    too_loose: headroom_verdict(pe, worst),
                });
                check_analytic(&mut quantities, &pr.label(), pe, &ours_t, &ours_v, "t");
            }
        }
        "dc" => {
            let dc = directives
                .dc
                .unwrap_or_else(|| panic!("{name}: dc deck has no .dc card"));
            let ours: SimOutput = run_dc(&circuit, &opts, &dc, &probes)
                .unwrap_or_else(|e| panic!("{name}: hauksbee dc failed: {e}"));
            // Column 0 of every row is the swept value (the sweep axis).
            let ours_x: Vec<f64> = ours.rows.iter().map(|r| r[0]).collect();
            for (pe, pr) in expect.probe.iter().zip(&probes) {
                let ng_deck = rewrite_with_print(&ng_src, &format!(".print dc {}", pr.label()));
                let tbl = oracle
                    .run(&ng_deck)
                    .and_then(|txt| parse_indexed_table(&txt))
                    .unwrap_or_else(|| panic!("{name}: no ngspice dc table for {}", pr.label()));
                let ng = NgSeries {
                    t: tbl.x,
                    v: tbl.cols[0].clone(),
                };
                let ours_v = ours.column(&pr.label()).unwrap();
                let floor = if expect.full_scale > 0.0 {
                    0.01 * expect.full_scale
                } else {
                    pe.abstol.unwrap_or(1e-9)
                };
                let (worst, at) = worst_rel_error(&ours_x, &ours_v, &ng, floor);
                quantities.push(QtyResult {
                    probe: pr.label(),
                    worst_err: worst,
                    at: format!("sweep={at:.3e}"),
                    reltol: pe.reltol,
                    pass: worst < pe.reltol,
                    too_loose: headroom_verdict(pe, worst),
                });
                check_analytic(&mut quantities, &pr.label(), pe, &ours_x, &ours_v, "sweep");
            }
        }
        "ac" => {
            let ac = directives
                .ac
                .unwrap_or_else(|| panic!("{name}: ac deck has no .ac card"));
            let ours: SimOutput = run_ac(&circuit, &opts, &ac, &probes)
                .unwrap_or_else(|e| panic!("{name}: hauksbee ac failed: {e}"));
            let ours_f: Vec<f64> = ours.rows.iter().map(|r| r[0]).collect();
            for (pe, pr) in expect.probe.iter().zip(&probes) {
                let node = match pr {
                    Probe::NodeVoltage(a) => a.clone(),
                    other => panic!(
                        "{name}: the AC cross-check supports V(node) probes only, got {other:?}"
                    ),
                };
                let ng_deck =
                    rewrite_with_print(&ng_src, &format!(".print ac vm({node}) vp({node})"));
                let tbl = oracle
                    .run(&ng_deck)
                    .and_then(|txt| parse_indexed_table(&txt))
                    .unwrap_or_else(|| panic!("{name}: no ngspice ac table for {}", pr.label()));
                assert!(
                    tbl.cols.len() >= 2,
                    "{name}: ngspice ac table for {} has no vm/vp columns",
                    pr.label()
                );
                // ngspice vp() is in RADIANS; our phase is degrees, convert to compare.
                let ng_mag = NgSeries {
                    t: tbl.x.clone(),
                    v: tbl.cols[0].clone(),
                };
                let ng_phase_deg = NgSeries {
                    t: tbl.x.clone(),
                    v: tbl.cols[1].iter().map(|r| r.to_degrees()).collect(),
                };

                // Magnitude: relative tolerance with an absolute floor.
                let ours_mag = ours.column(&pr.label()).unwrap();
                let floor = pe.abstol.unwrap_or(1e-6);
                let (wm, atm) = worst_rel_error(&ours_f, &ours_mag, &ng_mag, floor);
                quantities.push(QtyResult {
                    probe: format!("{} mag", pr.label()),
                    worst_err: wm,
                    at: format!("f={atm:.3e}Hz"),
                    reltol: pe.reltol,
                    pass: wm < pe.reltol,
                    too_loose: headroom_verdict(pe, wm),
                });
                check_analytic(
                    &mut quantities,
                    &format!("{} mag", pr.label()),
                    pe,
                    &ours_f,
                    &ours_mag,
                    "f",
                );

                // Phase: absolute tolerance in degrees.
                let ph_tol = pe.phase_abstol.unwrap_or_else(|| {
                    panic!("{name}: ac probe {} needs `phase_abstol`", pr.label())
                });
                let ours_ph = ours.column(&format!("{}:phase_deg", pr.label())).unwrap();
                let (wp, atp) = worst_abs_error(&ours_f, &ours_ph, &ng_phase_deg);
                quantities.push(QtyResult {
                    probe: format!("{} phase(deg)", pr.label()),
                    worst_err: wp,
                    at: format!("f={atp:.3e}Hz"),
                    reltol: ph_tol,
                    pass: wp < ph_tol,
                    // Phase is an ABSOLUTE bound in degrees, so the relative
                    // headroom floor does not apply; the bar is a tenth of a
                    // degree, below which a phase claim is not meaningful anyway.
                    too_loose: if pe.loose_reason.is_none() && ph_tol > (wp * MAX_HEADROOM).max(0.1)
                    {
                        Some(format!(
                            "phase tolerance {ph_tol:.1e} deg is far above the measured \
                             {wp:.3e} deg; tighten it or state `loose_reason`"
                        ))
                    } else {
                        None
                    },
                });
            }
        }
        other => panic!("{name}: unknown analysis `{other}` in expect.toml"),
    }

    DeckResult {
        name,
        analysis: expect.analysis,
        description: expect.description,
        quantities,
    }
}

/// Write current measurements without modifying tracked documentation by default.
fn write_results_md(results: &[DeckResult], ng_version: &str) {
    let path = oracle::output_path("HAUKSBEE_NGSPICE_REPORT", "spice-compat-results.md");

    let mut s = String::new();
    s.push_str("# SPICE compatibility: ngspice cross-check results\n\n");
    s.push_str(
        // Keep the generated report and explicit documentation refresh identical.
        "Generated by `cargo nextest run -p hauksbee-solve --test it -E 'test(ngspice_corpus)'` (do not hand-edit).\n\
         Set `HAUKSBEE_NGSPICE_REPORT` to refresh this document explicitly.\n\
         The test runs each deck in `crates/hauksbee-solve/tests/decks/` through both\n\
         ngspice `-b` and the hauksbee solver, then compares the worst-case error per\n\
         probe against the per-quantity tolerance declared in that deck's\n\
         `expect.toml`.\n\n\
         This differential corpus runs wherever ngspice is installed, including the\n\
         release-quality job, and skips otherwise, so the numbers below are from\n\
         whichever machine last regenerated the file. The oracle version is that run's\n\
         ngspice, not a pinned requirement, and a different ngspice may move the\n\
         worst-case columns.\n\
         `docs/spice-compat/compatibility.md` lists the supported cards that have no\n\
         deck here at all.\n\n",
    );
    let _ = writeln!(s, "- Oracle: **{ng_version}**");
    let _ = writeln!(s, "- Decks: **{}**", results.len());
    let passed = results.iter().filter(|d| d.passed()).count();
    let _ = writeln!(s, "- Passing: **{passed}/{}**\n", results.len());

    s.push_str(
        "| Deck | Analysis | Quantity | Worst-case error | Tolerance | Headroom | Where | \
         Result |\n",
    );
    s.push_str(
        "|------|----------|----------|------------------|-----------|----------|-------|\
         --------|\n",
    );
    for d in results {
        for (i, q) in d.quantities.iter().enumerate() {
            let deck_cell = if i == 0 { d.name.as_str() } else { "" };
            let analysis_cell = if i == 0 { d.analysis.as_str() } else { "" };
            // Publish the tolerance/error ratio so readers can judge sensitivity.
            let headroom = if q.worst_err > 0.0 {
                format!("{:.0}x", q.reltol / q.worst_err)
            } else {
                "exact".to_string()
            };
            let _ = writeln!(
                s,
                "| {} | {} | `{}` | {:.3e} | {:.1e} | {} | {} | {} |",
                deck_cell,
                analysis_cell,
                q.probe,
                q.worst_err,
                q.reltol,
                headroom,
                q.at,
                if q.pass { "PASS" } else { "**FAIL**" },
            );
        }
    }
    s.push('\n');
    s.push_str("## Deck descriptions\n\n");
    for d in results {
        let _ = writeln!(s, "- **{}**: {}", d.name, d.description);
    }
    s.push('\n');
    std::fs::write(&path, s).expect("write results.md");
    eprintln!("wrote {}", path.display());
}

#[test]
fn mos_on_resistance_bound_rejects_lost_series_resistance() {
    let deck = include_str!("decks/mos_rds_on.cir");
    let expect: Expect = toml::from_str(include_str!("decks/mos_rds_on.expect.toml")).unwrap();
    let point = &expect.probe[0].analytic[0];
    for (series, passes) in [("RD=0.05 RS=0.05", true), ("RD=0 RS=0", false)] {
        let (circuit, directives) =
            SpiceLoader::load_with_directives(&deck.replace("RD=0.05 RS=0.05", series)).unwrap();
        let output = run_dc(
            &circuit,
            &opts_from_directives(&directives),
            directives.dc.as_ref().unwrap(),
            &[Probe::parse("V(d)").unwrap()],
        )
        .unwrap();
        let value = *output.column("V(d)").unwrap().last().unwrap();
        let error = (value - point.value).abs() / point.value;
        assert_eq!(error < point.reltol, passes, "{series}: error={error}");
        assert_eq!(
            error < expect.probe[0].reltol,
            passes,
            "oracle bound: {series}"
        );
    }
    for analysis in ["tran", "dc", "ac"] {
        let card = format!(".print {analysis} V(out)");
        let rewritten = rewrite_with_print("title\n.subckt x a b\n.ends x\n.end\n", &card);
        assert!(rewritten.ends_with(&format!(".ends x\n{card}\n.end\n")));
    }
}

/// The deck op-amp converges COLD at DC with no homotopy rung firing (the
/// corpus entry checks the values it converges to). The same amplifier as a `B`
/// source does not: its finite-difference tangent is exactly 0 inside the clamp
/// and the loop two-cycles between the rails. The answer alone would still pass
/// if a change rescued it with staged DC, so require the drained ladder silent.
#[test]
fn deck_opamp_converges_cold_with_no_fallback_strategy() {
    let (circuit, directives) =
        SpiceLoader::load_with_directives(include_str!("decks/opamp_deck_gain11.cir")).unwrap();
    let probes = ["V(outb)", "V(outd)"].map(|p| Probe::parse(p).unwrap());
    drop(hauksbee_solve::take_strategy_activations());
    run_op(&circuit, &opts_from_directives(&directives), &probes).unwrap();
    let fired = hauksbee_solve::take_strategy_activations();
    assert!(fired.is_empty(), "cold DC needed the ladder: {fired:?}");
}

#[test]
fn ngspice_corpus() {
    let Some(bin) = find_ngspice() else {
        // A CI runner that REQUIRES the differential corpus (rather than
        // tolerating the skip on contributor machines without ngspice) sets
        // HAUKSBEE_REQUIRE_NGSPICE=1 so a missing oracle is a hard failure,
        // never a silently-green gate with zero coverage.
        assert!(
            std::env::var_os("HAUKSBEE_REQUIRE_NGSPICE").is_none(),
                "HAUKSBEE_REQUIRE_NGSPICE is set but ngspice was not found                  ($NGSPICE / PATH / known locations); the SPICE differential                  gate cannot run"
        );
        eprintln!("ngspice not found ($NGSPICE / PATH / known locations); skipping corpus.");
        return;
    };
    let version = ngspice_version(&bin);
    eprintln!("ngspice: {} ({version})", bin.display());

    let mut cirs: Vec<PathBuf> = std::fs::read_dir(decks_dir())
        .expect("read decks dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("cir"))
        .filter(|p| !p.to_string_lossy().ends_with(".ngspice.cir"))
        .collect();
    cirs.sort();
    assert!(
        !cirs.is_empty(),
        "no decks found in {}",
        decks_dir().display()
    );

    let oracle = CachedOracle::new(&bin, &decks_dir());
    let mut results = Vec::new();
    for cir in &cirs {
        let r = run_deck(&oracle, cir);
        for q in &r.quantities {
            eprintln!(
                "[{}] {} {}  worst={:.3e}  tol={:.1e}  {}  ({})",
                if q.pass { "PASS" } else { "FAIL" },
                r.name,
                q.probe,
                q.worst_err,
                q.reltol,
                if q.pass { "ok" } else { "OVER TOLERANCE" },
                q.at,
            );
        }
        results.push(r);
    }

    // Preserve measurements, including failures, before asserting.
    write_results_md(&results, &version);

    let failed: Vec<&DeckResult> = results.iter().filter(|d| !d.passed()).collect();
    assert!(
        failed.is_empty(),
        "decks over tolerance: {:?}",
        failed.iter().map(|d| &d.name).collect::<Vec<_>>()
    );

    // A bound far above the real error is a hole in the gate, so it fails the gate.
    let mut loose = Vec::new();
    for d in &results {
        for q in &d.quantities {
            if let Some(msg) = &q.too_loose {
                loose.push(format!("{} {}: {msg}", d.name, q.probe));
            }
        }
    }
    assert!(
        loose.is_empty(),
        "tolerances too loose to gate anything ({} probes):\n  {}",
        loose.len(),
        loose.join("\n  ")
    );

    // Only the generated report may name the measured oracle's version.
    let mut stale = Vec::new();
    let ver_pat = regex_lite_ngspice_version();
    for cir in &cirs {
        let ep = cir.with_extension("expect.toml");
        let text = std::fs::read_to_string(&ep).expect("read expect");
        for (i, line) in text.lines().enumerate() {
            if ver_pat(line) {
                stale.push(format!(
                    "{}:{}: {}",
                    ep.file_name().unwrap().to_string_lossy(),
                    i + 1,
                    line.trim()
                ));
            }
        }
    }
    assert!(
        stale.is_empty(),
        "expectation prose names a specific ngspice version; the generated table \
         (docs/spice-compat/results.md) is the only place that may, because it \
         records the binary that actually ran:\n  {}",
        stale.join("\n  ")
    );
}

/// True when a line names a specific ngspice version (`ngspice-46`, `ngspice 45.2`).
/// Hand-rolled rather than pulling in a regex crate for one predicate.
fn regex_lite_ngspice_version() -> impl Fn(&str) -> bool {
    |line: &str| {
        let lower = line.to_ascii_lowercase();
        let mut rest = lower.as_str();
        while let Some(pos) = rest.find("ngspice") {
            rest = &rest[pos + "ngspice".len()..];
            let tail = rest.trim_start_matches(['-', ' ', '_']);
            if tail.starts_with(|c: char| c.is_ascii_digit()) {
                return true;
            }
        }
        false
    }
}

// A tiny unit check that the ngspice lookup honors $NGSPICE without needing the
// binary present, so the generalization itself is covered even on a bare CI box.
#[test]
fn ngspice_lookup_respects_env() {
    // A path that does not exist must NOT be returned (we probe is_file()).
    std::env::set_var("NGSPICE", "/no/such/ngspice/binary");
    // Either PATH/known-location finds a real one, or nothing is found; in both
    // cases the bogus env path is never what we get back.
    if let Some(p) = find_ngspice() {
        assert_ne!(p, PathBuf::from("/no/such/ngspice/binary"));
    }
    std::env::remove_var("NGSPICE");
}
