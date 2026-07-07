//! The ngspice differential harness (SPICE-compat plan §6).
//!
//! For every `.cir` in `tests/decks/` with a companion `<name>.expect.toml`,
//! this runs ngspice `-b` and the hauksbee solver over the SAME deck, resamples
//! ngspice onto our timebase, and compares each declared probe against its
//! declared PER-QUANTITY tolerance (a diode forward drop wants mV; an RC tail
//! only percent — one global bar cannot serve both). Each deck emits a per-deck
//! pass/fail with its worst-case error and where it occurred, and the whole
//! corpus regenerates `docs/spice-compat/results.md`.
//!
//! ngspice lookup: `$NGSPICE`, then `PATH`, then the known per-OS install
//! locations — so the harness runs on any machine with ngspice, not just the
//! one Mac whose Homebrew path was once hardcoded. If ngspice is not found the
//! harness SKIPS (contributors without it are never blocked); it never fails
//! for want of the oracle.

use hauksbee_ir::{Directives, SpiceLoader};
use hauksbee_solve::{
    run_ac, run_dc, run_op, run_tran, DcInit, Integration, Probe, SimOutput, SolverOptions,
    StepControl,
};
use serde::Deserialize;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

// --- ngspice discovery -------------------------------------------------------

/// Locate the ngspice binary: `$NGSPICE`, then `PATH`, then per-OS defaults.
fn find_ngspice() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("NGSPICE") {
        let pb = PathBuf::from(&p);
        if pb.is_file() {
            return Some(pb);
        }
    }
    let exe = if cfg!(windows) { "ngspice.exe" } else { "ngspice" };
    if let Ok(path) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path) {
            let cand = dir.join(exe);
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    for p in [
        "/opt/homebrew/bin/ngspice",
        "/usr/local/bin/ngspice",
        "/usr/bin/ngspice",
        "/opt/local/bin/ngspice",
        "C:\\Program Files\\ngspice\\bin\\ngspice_con.exe",
        "C:\\Program Files\\ngspice\\bin\\ngspice.exe",
    ] {
        let pb = PathBuf::from(p);
        if pb.is_file() {
            return Some(pb);
        }
    }
    None
}

/// The ngspice version line (e.g. `ngspice-46`), for the results table.
fn ngspice_version(bin: &Path) -> String {
    let out = match Command::new(bin).arg("--version").output() {
        Ok(o) => o,
        Err(_) => return "unknown".to_string(),
    };
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    for line in text.lines() {
        if let Some(pos) = line.find("ngspice-") {
            let tail = &line[pos..];
            let tok: String = tail
                .chars()
                .take_while(|c| !c.is_whitespace() && *c != ':')
                .collect();
            return tok;
        }
    }
    "unknown".to_string()
}

/// Run a netlist string through `ngspice -b` and return its stdout.
fn run_ngspice(bin: &Path, netlist: &str) -> Option<String> {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    netlist.hash(&mut h);
    let path = std::env::temp_dir().join(format!(
        "hauksbee_xcheck_{}_{:016x}.cir",
        std::process::id(),
        h.finish()
    ));
    std::fs::File::create(&path)
        .ok()?
        .write_all(netlist.as_bytes())
        .ok()?;
    let out = Command::new(bin).arg("-b").arg(&path).output().ok()?;
    let _ = std::fs::remove_file(&path);
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

// --- ngspice output parsing --------------------------------------------------

/// A parsed `.print tran` column: (times, values).
struct NgSeries {
    t: Vec<f64>,
    v: Vec<f64>,
}

/// Parse the `Index time value` block ngspice prints for `.print tran`.
fn parse_tran_table(text: &str) -> Option<NgSeries> {
    let mut t = Vec::new();
    let mut v = Vec::new();
    let mut in_table = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("Index") && trimmed.contains("time") {
            in_table = true;
            continue;
        }
        if !in_table || trimmed.starts_with("---") || trimmed.is_empty() {
            continue;
        }
        let cols: Vec<&str> = trimmed.split_whitespace().collect();
        if cols.len() >= 3 {
            if let (Ok(time), Ok(val)) = (cols[1].parse::<f64>(), cols[2].parse::<f64>()) {
                t.push(time);
                v.push(val);
            }
        }
    }
    if t.is_empty() {
        None
    } else {
        Some(NgSeries { t, v })
    }
}

/// A parsed indexed table with any number of value columns: `x` is the sweep
/// axis (column 1 — time / v-sweep / frequency) and `cols[k]` is value column
/// `k` (column `k + 2`). Serves `.print dc` (one value column) and `.print ac`
/// (a `vm` and a `vp` column requested side by side).
struct NgTable {
    x: Vec<f64>,
    cols: Vec<Vec<f64>>,
}

/// Parse the `Index <axis> v1 [v2 ...]` block ngspice prints for `.print dc/ac`.
fn parse_indexed_table(text: &str) -> Option<NgTable> {
    let mut x = Vec::new();
    let mut cols: Vec<Vec<f64>> = Vec::new();
    let mut in_table = false;
    let mut ncol = 0usize;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with("Index") {
            in_table = true;
            continue;
        }
        if !in_table || t.starts_with("---") || t.is_empty() {
            continue;
        }
        let c: Vec<&str> = t.split_whitespace().collect();
        // A data row is `Index xval v1 [v2 ...]`; anything else ends the table.
        if c.len() < 3 {
            if !x.is_empty() {
                break;
            }
            continue;
        }
        let Ok(xv) = c[1].parse::<f64>() else {
            if !x.is_empty() {
                break;
            }
            continue;
        };
        let vals: Vec<f64> = c[2..].iter().filter_map(|s| s.parse::<f64>().ok()).collect();
        if cols.is_empty() {
            ncol = vals.len();
            cols = vec![Vec::new(); ncol];
        }
        if vals.len() != ncol || ncol == 0 {
            continue;
        }
        x.push(xv);
        for (i, v) in vals.iter().enumerate() {
            cols[i].push(*v);
        }
    }
    if x.is_empty() {
        None
    } else {
        Some(NgTable { x, cols })
    }
}

/// Parse the `.op` listing ngspice prints in batch mode: a node-voltage block
/// (`name  value`) and a source-current block (`vname#branch  value`).
fn parse_op_listing(text: &str) -> (HashMap<String, f64>, HashMap<String, f64>) {
    let mut nodes = HashMap::new();
    let mut sources = HashMap::new();
    #[derive(PartialEq)]
    enum State {
        None,
        Nodes,
        Sources,
    }
    let mut state = State::None;
    for line in text.lines() {
        let t = line.trim();
        if t.contains("Node") && t.contains("Voltage") {
            state = State::Nodes;
            continue;
        }
        if t.contains("Source") && t.contains("Current") {
            state = State::Sources;
            continue;
        }
        // A models block (`Resistor models`, `Diode models`, ...) ends the data.
        if t.contains("models") {
            state = State::None;
            continue;
        }
        let cols: Vec<&str> = t.split_whitespace().collect();
        if cols.len() != 2 {
            continue;
        }
        let Ok(val) = cols[1].parse::<f64>() else {
            continue;
        };
        match state {
            State::Nodes => {
                nodes.insert(cols[0].to_ascii_lowercase(), val);
            }
            State::Sources => {
                let key = cols[0]
                    .to_ascii_lowercase()
                    .trim_end_matches("#branch")
                    .to_string();
                sources.insert(key, val);
            }
            State::None => {}
        }
    }
    (nodes, sources)
}

/// Linear interpolation of an ngspice waveform onto an arbitrary time.
fn interp(res: &NgSeries, time: f64) -> f64 {
    if time <= res.t[0] {
        return res.v[0];
    }
    let last = res.t.len() - 1;
    if time >= res.t[last] {
        return res.v[last];
    }
    let (mut lo, mut hi) = (0usize, last);
    while hi - lo > 1 {
        let mid = (lo + hi) / 2;
        if res.t[mid] <= time {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    let span = res.t[hi] - res.t[lo];
    if span == 0.0 {
        return res.v[hi];
    }
    let frac = (time - res.t[lo]) / span;
    res.v[lo] + (res.v[hi] - res.v[lo]) * frac
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
}

/// A per-quantity comparison result for the table.
struct QtyResult {
    probe: String,
    worst_err: f64,
    at: String,
    reltol: f64,
    pass: bool,
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

/// Rewrite a deck for a single-probe ngspice transient run: drop the deck's own
/// `.print`/`.plot` cards and inject exactly one `.print tran <expr>` so the
/// output is a clean three-column `Index time value` table.
fn deck_for_ngspice_tran(orig: &str, probe_expr: &str) -> String {
    let mut out = String::new();
    let mut injected = false;
    for line in orig.lines() {
        let l = line.trim_start().to_ascii_lowercase();
        if l.starts_with(".print") || l.starts_with(".plot") {
            continue;
        }
        if l.starts_with(".end") && !injected {
            out.push_str(&format!(".print tran {probe_expr}\n"));
            injected = true;
        }
        out.push_str(line);
        out.push('\n');
    }
    if !injected {
        out.push_str(&format!(".print tran {probe_expr}\n.end\n"));
    }
    out
}

/// Rewrite a deck for a single-probe ngspice DC-sweep run: drop the deck's
/// `.print`/`.plot` cards and inject one `.print dc <expr>`.
fn deck_for_ngspice_dc(orig: &str, probe_expr: &str) -> String {
    rewrite_with_print(orig, &format!(".print dc {probe_expr}"))
}

/// Rewrite a deck for a single-node ngspice AC run: drop the deck's own
/// `.print`/`.plot` cards and inject `.print ac vm(<node>) vp(<node>)` so the
/// output is a four-column `Index frequency vm vp` table (phase in radians).
fn deck_for_ngspice_ac(orig: &str, node: &str) -> String {
    rewrite_with_print(orig, &format!(".print ac vm({node}) vp({node})"))
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
        if l.starts_with(".end") && !injected {
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

/// Worst relative error of `ours` vs interpolated ngspice, with a full-scale
/// floor so zero-crossings don't blow the ratio up. Returns (worst, at_time).
fn worst_tran_error(
    ours_t: &[f64],
    ours_v: &[f64],
    ng: &NgSeries,
    full_scale: f64,
    skip_initial: f64,
) -> (f64, f64) {
    let floor = if full_scale > 0.0 {
        0.01 * full_scale
    } else {
        1e-9
    };
    let mut worst = 0.0f64;
    let mut at = 0.0f64;
    for (&t, &ov) in ours_t.iter().zip(ours_v) {
        if t < skip_initial {
            continue;
        }
        let nv = interp(ng, t);
        let e = (ov - nv).abs() / nv.abs().max(floor);
        if e > worst {
            worst = e;
            at = t;
        }
    }
    (worst, at)
}

/// Worst relative error of `ours(xs)` vs ngspice interpolated onto `xs`, with an
/// absolute floor so a near-zero magnitude does not blow the ratio up. Used for
/// DC-sweep and AC-magnitude columns. Returns `(worst, at_x)`.
fn worst_rel_error(xs: &[f64], ours: &[f64], ng: &NgSeries, floor: f64) -> (f64, f64) {
    let mut worst = 0.0f64;
    let mut at = xs.first().copied().unwrap_or(0.0);
    for (&x, &ov) in xs.iter().zip(ours) {
        let nv = interp(ng, x);
        let e = (ov - nv).abs() / nv.abs().max(floor);
        if e > worst {
            worst = e;
            at = x;
        }
    }
    (worst, at)
}

/// Worst ABSOLUTE error of `ours(xs)` vs ngspice interpolated onto `xs`. Used for
/// AC phase (degrees), where an absolute tolerance is the honest bar.
fn worst_abs_error(xs: &[f64], ours: &[f64], ng: &NgSeries) -> (f64, f64) {
    let mut worst = 0.0f64;
    let mut at = xs.first().copied().unwrap_or(0.0);
    for (&x, &ov) in xs.iter().zip(ours) {
        let nv = interp(ng, x);
        let e = (ov - nv).abs();
        if e > worst {
            worst = e;
            at = x;
        }
    }
    (worst, at)
}

/// Look up a probe's value in a parsed `.op` listing.
fn op_value(
    probe: &Probe,
    nodes: &HashMap<String, f64>,
    sources: &HashMap<String, f64>,
) -> Option<f64> {
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
        Probe::BranchCurrent(d) => sources.get(&d.to_ascii_lowercase()).copied(),
    }
}

// --- the harness proper ------------------------------------------------------

fn decks_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/decks")
}

/// Run one deck end-to-end. Returns `None` only on an ngspice-side failure the
/// harness cannot attribute (missing output), which the caller treats as skip.
fn run_deck(bin: &Path, cir_path: &Path) -> DeckResult {
    let name = cir_path.file_stem().unwrap().to_string_lossy().to_string();
    let deck = std::fs::read_to_string(cir_path).expect("read deck");
    let expect_path = cir_path.with_extension("expect.toml");
    let expect_text = std::fs::read_to_string(&expect_path)
        .unwrap_or_else(|_| panic!("missing {}", expect_path.display()));
    let expect: Expect = toml::from_str(&expect_text)
        .unwrap_or_else(|e| panic!("parse {}: {e}", expect_path.display()));

    let (circuit, directives) = SpiceLoader::load_with_directives(&deck)
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
            let ng_text = run_ngspice(bin, &deck).expect("ngspice op run");
            let (nodes, sources) = parse_op_listing(&ng_text);
            for (pe, pr) in expect.probe.iter().zip(&probes) {
                let ours_val = ours.column(&pr.label()).unwrap()[0];
                let ng_val = op_value(pr, &nodes, &sources).unwrap_or_else(|| {
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
                });
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
                let ng_deck = deck_for_ngspice_tran(&deck, &pr.label());
                let ng = run_ngspice(bin, &ng_deck)
                    .and_then(|txt| parse_tran_table(&txt))
                    .unwrap_or_else(|| panic!("{name}: no ngspice table for {}", pr.label()));
                let ours_v = ours.column(&pr.label()).unwrap();
                let (worst, at) = worst_tran_error(
                    &ours_t,
                    &ours_v,
                    &ng,
                    expect.full_scale,
                    expect.skip_initial,
                );
                quantities.push(QtyResult {
                    probe: pr.label(),
                    worst_err: worst,
                    at: format!("t={at:.3e}s"),
                    reltol: pe.reltol,
                    pass: worst < pe.reltol,
                });
            }
        }
        "dc" => {
            let dc = directives
                .dc
                .clone()
                .unwrap_or_else(|| panic!("{name}: dc deck has no .dc card"));
            let ours: SimOutput = run_dc(&circuit, &opts, &dc, &probes)
                .unwrap_or_else(|e| panic!("{name}: hauksbee dc failed: {e}"));
            // Column 0 of every row is the swept value (the sweep axis).
            let ours_x: Vec<f64> = ours.rows.iter().map(|r| r[0]).collect();
            for (pe, pr) in expect.probe.iter().zip(&probes) {
                let ng_deck = deck_for_ngspice_dc(&deck, &pr.label());
                let tbl = run_ngspice(bin, &ng_deck)
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
                });
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
                        "{name}: the AC cross-check supports V(node) probes only, got {:?}",
                        other
                    ),
                };
                let ng_deck = deck_for_ngspice_ac(&deck, &node);
                let tbl = run_ngspice(bin, &ng_deck)
                    .and_then(|txt| parse_indexed_table(&txt))
                    .unwrap_or_else(|| panic!("{name}: no ngspice ac table for {}", pr.label()));
                assert!(
                    tbl.cols.len() >= 2,
                    "{name}: ngspice ac table for {} has no vm/vp columns",
                    pr.label()
                );
                // ngspice vp() is in RADIANS; our phase is degrees — convert to compare.
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
                });

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

/// Write the published results table. Regenerated by the corpus test so the
/// numbers can never go stale.
fn write_results_md(results: &[DeckResult], ng_version: &str) {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap()
        .to_path_buf();
    let dir = repo_root.join("docs/spice-compat");
    std::fs::create_dir_all(&dir).expect("mkdir docs/spice-compat");
    let path = dir.join("results.md");

    let mut s = String::new();
    s.push_str("# SPICE compatibility: ngspice cross-check results\n\n");
    s.push_str(
        "Generated by `cargo test -p hauksbee-solve --test ngspice` (do not hand-edit).\n\
         Each deck in `crates/hauksbee-solve/tests/decks/` is run through both ngspice\n\
         `-b` and the hauksbee solver; the worst-case error per probe is compared against\n\
         the per-quantity tolerance declared in that deck's `expect.toml`.\n\n",
    );
    s.push_str(&format!("- Oracle: **ngspice {ng_version}**\n"));
    s.push_str(&format!("- Decks: **{}**\n", results.len()));
    let passed = results.iter().filter(|d| d.passed()).count();
    s.push_str(&format!("- Passing: **{passed}/{}**\n\n", results.len()));

    s.push_str("| Deck | Analysis | Quantity | Worst-case error | Tolerance | Where | Result |\n");
    s.push_str("|------|----------|----------|------------------|-----------|-------|--------|\n");
    for d in results {
        for (i, q) in d.quantities.iter().enumerate() {
            let deck_cell = if i == 0 { d.name.as_str() } else { "" };
            let analysis_cell = if i == 0 { d.analysis.as_str() } else { "" };
            s.push_str(&format!(
                "| {} | {} | `{}` | {:.3e} | {:.1e} | {} | {} |\n",
                deck_cell,
                analysis_cell,
                q.probe,
                q.worst_err,
                q.reltol,
                q.at,
                if q.pass { "PASS" } else { "**FAIL**" },
            ));
        }
    }
    s.push('\n');
    s.push_str("## Deck descriptions\n\n");
    for d in results {
        s.push_str(&format!("- **{}**: {}\n", d.name, d.description));
    }
    s.push('\n');
    std::fs::write(&path, s).expect("write results.md");
    eprintln!("wrote {}", path.display());
}

#[test]
fn ngspice_corpus() {
    let Some(bin) = find_ngspice() else {
        eprintln!("ngspice not found ($NGSPICE / PATH / known locations); skipping corpus.");
        return;
    };
    let version = ngspice_version(&bin);
    eprintln!("ngspice: {} ({version})", bin.display());

    let mut cirs: Vec<PathBuf> = std::fs::read_dir(decks_dir())
        .expect("read decks dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("cir"))
        .collect();
    cirs.sort();
    assert!(!cirs.is_empty(), "no decks found in {}", decks_dir().display());

    let mut results = Vec::new();
    for cir in &cirs {
        let r = run_deck(&bin, cir);
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

    // Always regenerate the published table (with real numbers, failures marked)
    // before asserting, so the doc reflects this exact run.
    write_results_md(&results, &version);

    let failed: Vec<&DeckResult> = results.iter().filter(|d| !d.passed()).collect();
    assert!(
        failed.is_empty(),
        "decks over tolerance: {:?}",
        failed.iter().map(|d| &d.name).collect::<Vec<_>>()
    );
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
