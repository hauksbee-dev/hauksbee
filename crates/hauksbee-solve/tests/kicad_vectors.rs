//! Accuracy vectors authored by the KiCad project: real schematics exported
//! with `kicad-cli sch export netlist --format spice` (testdata/
//! spice-vectors/). The same netlist runs through ngspice and hauksbee;
//! waveforms must agree. These exercise KiCad's node naming
//! (`/rect_out`, `Net-_D1-A_`), `.include` model cards, and vendor-style
//! Gummel-Poon parameter sets none of our hand-written tests use.

use hauksbee_ir::SpiceLoader;
use hauksbee_solve::{SolverOptions, StepControl, Transient};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

const NGSPICE: &str = "/opt/homebrew/bin/ngspice";

fn vectors_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/spice-vectors")
}

/// Inline `.include` files and strip `.control` blocks so a vector becomes a
/// self-contained netlist both engines accept.
fn preprocess(path: &Path) -> Option<String> {
    let src = std::fs::read_to_string(path).ok()?;
    let mut out = String::new();
    let mut in_control = false;
    for line in src.lines() {
        let lower = line.trim().to_ascii_lowercase();
        if lower.starts_with(".control") {
            in_control = true;
            continue;
        }
        if lower.starts_with(".endc") {
            in_control = false;
            continue;
        }
        if in_control {
            continue;
        }
        if let Some(rest) = line.trim().strip_prefix(".include") {
            let inc_path = rest.trim().trim_matches('"');
            let Ok(inc) = std::fs::read_to_string(inc_path) else {
                eprintln!("include missing: {inc_path}");
                return None;
            };
            out.push_str(&inc);
            out.push('\n');
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    Some(out)
}

/// Swap the analysis line for our own and append print/options for ngspice.
fn with_analysis(net: &str, tran: &str, probe: &str) -> String {
    let mut out = String::new();
    for line in net.lines() {
        let lower = line.trim().to_ascii_lowercase();
        if lower.starts_with(".tran") || lower.starts_with(".ac") || lower.starts_with(".end") {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(tran);
    out.push('\n');
    out.push_str(&format!(".print tran v({probe})\n"));
    out.push_str(".options reltol=1e-4\n.end\n");
    out
}

struct NgResult {
    t: Vec<f64>,
    v: Vec<f64>,
}

fn run_ngspice(netlist: &str) -> Option<NgResult> {
    if !std::path::Path::new(NGSPICE).exists() {
        return None;
    }
    let path = std::env::temp_dir().join(format!(
        "hauksbee_kicadvec_{}_{:x}.cir",
        std::process::id(),
        netlist.len()
    ));
    {
        let mut f = std::fs::File::create(&path).ok()?;
        f.write_all(netlist.as_bytes()).ok()?;
    }
    let out = Command::new(NGSPICE).arg("-b").arg(&path).output().ok()?;
    let _ = std::fs::remove_file(&path);
    let text = String::from_utf8_lossy(&out.stdout);
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
        eprintln!(
            "ngspice produced no table; stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        None
    } else {
        Some(NgResult { t, v })
    }
}

fn interp(res: &NgResult, time: f64) -> f64 {
    if time <= res.t[0] {
        return res.v[0];
    }
    let last = res.t.len() - 1;
    if time >= res.t[last] {
        return res.v[last];
    }
    let mut lo = 0usize;
    let mut hi = last;
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
    res.v[lo] + (res.v[hi] - res.v[lo]) * ((time - res.t[lo]) / span)
}

/// Run one vector through both engines; return (max rel error, full scale).
fn cross_check(file: &str, tran: &str, probe: &str, tstop: f64) -> Option<f64> {
    let path = vectors_dir().join(file);
    if !path.exists() {
        eprintln!("vector missing: {}; skipping", path.display());
        return None;
    }
    let net = preprocess(&path)?;
    let net = with_analysis(&net, tran, probe);

    let ng = match run_ngspice(&net) {
        Some(r) => r,
        None => {
            eprintln!("ngspice unavailable/failed for {file}; skipping");
            return None;
        }
    };

    let circuit = SpiceLoader::load(&net).unwrap_or_else(|e| panic!("{file}: loader failed: {e}"));
    let opts = SolverOptions {
        step: StepControl::Fixed { dt: tstop / 4000.0 },
        ..SolverOptions::default()
    };
    let result = Transient::new(opts)
        .run(&circuit, tstop)
        .unwrap_or_else(|e| panic!("{file}: solve failed: {e}"));
    let ours = result
        .node(&circuit, probe)
        .unwrap_or_else(|| panic!("{file}: probe {probe} missing from results"));

    let full_scale = ng.v.iter().fold(0.0f64, |m, &x| m.max(x.abs())).max(1e-9);
    let floor = 0.01 * full_scale;
    let skip = tstop * 0.05; // both engines settle initial transients
    let mut worst = 0.0f64;
    for (&t, &ov) in result.time.iter().zip(ours) {
        if t < skip {
            continue;
        }
        let nv = interp(&ng, t);
        worst = worst.max((ov - nv).abs() / nv.abs().max(floor));
    }
    eprintln!("{file}: max rel error vs ngspice = {worst:.3e} (full scale {full_scale:.2})");
    Some(worst)
}

#[test]
fn kicad_rectifier_vector() {
    if let Some(err) = cross_check("rectifier.cir", ".tran 1u 10m", "/rect_out", 10e-3) {
        assert!(err < 0.01, "rectifier error {err:.3e} >= 1%");
    }
}

#[test]
fn kicad_amplifier_ac_vector_as_transient() {
    // The KiCad demo is an AC analysis; we run the same 3x 2N2222 amplifier
    // in transient with its 1kHz source, which exercises the full
    // Gummel-Poon parameter set from the vendor model card.
    if let Some(err) = cross_check("amplifier-ac.cir", ".tran 2u 4m", "VOUT", 4e-3) {
        assert!(err < 0.02, "amplifier error {err:.3e} >= 2%");
    }
}
