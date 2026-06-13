//! Cross-checks against ngspice. The same netlist string is (a) run through
//! `/opt/homebrew/bin/ngspice -b` and (b) parsed by our [`SpiceLoader`] and
//! simulated by our engine; the two output waveforms are compared.
//!
//! ngspice defaults differ from ours in a couple of places, so the netlists set
//! `.options reltol=...` to match and use `uic` where we seed initial
//! conditions. Tolerance is max relative voltage error < 1% over the run after
//! resampling ngspice onto our timebase (documented per test).
//!
//! If ngspice is not installed the tests skip rather than fail.

use hauksbee_ir::{SpiceError, SpiceLoader};
use hauksbee_solve::{Integration, SolverOptions, StepControl, Transient};
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::process::Command;

const NGSPICE: &str = "/opt/homebrew/bin/ngspice";

/// A parsed ngspice `.print tran` column: (times, values).
struct NgResult {
    t: Vec<f64>,
    v: Vec<f64>,
}

/// Run a netlist through ngspice batch mode and parse the printed transient
/// table. Returns `None` if ngspice is unavailable.
fn run_ngspice(netlist: &str) -> Option<NgResult> {
    if !std::path::Path::new(NGSPICE).exists() {
        return None;
    }
    // Unique per netlist so tests running in parallel never share a file.
    let mut h = std::collections::hash_map::DefaultHasher::new();
    netlist.hash(&mut h);
    let dir = std::env::temp_dir();
    let path = dir.join(format!(
        "hauksbee_xcheck_{}_{:016x}.cir",
        std::process::id(),
        h.finish()
    ));
    {
        let mut f = std::fs::File::create(&path).ok()?;
        f.write_all(netlist.as_bytes()).ok()?;
    }
    let out = Command::new(NGSPICE)
        .arg("-b")
        .arg(&path)
        .output()
        .ok()?;
    let _ = std::fs::remove_file(&path);
    let text = String::from_utf8_lossy(&out.stdout);
    parse_print_table(&text)
}

/// Parse the `Index time value` block ngspice prints for `.print tran`.
fn parse_print_table(text: &str) -> Option<NgResult> {
    let mut t = Vec::new();
    let mut v = Vec::new();
    let mut in_table = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("Index") && trimmed.contains("time") {
            in_table = true;
            continue;
        }
        if !in_table {
            continue;
        }
        if trimmed.starts_with("---") || trimmed.is_empty() {
            continue;
        }
        // Rows look like: `12\t2.960000e-06\t1.477811e-02`.
        let cols: Vec<&str> = trimmed.split_whitespace().collect();
        if cols.len() >= 3 {
            if let (Ok(time), Ok(val)) = (cols[1].parse::<f64>(), cols[2].parse::<f64>()) {
                // New blocks repeat the header for multi-page prints; the index
                // resets, but times are monotonic across pages so just append.
                t.push(time);
                v.push(val);
            }
        } else if !cols.is_empty() && cols[0].chars().next().is_some_and(|c| c.is_ascii_digit()) {
            // Already handled above; ignore stray lines.
        }
    }
    if t.is_empty() {
        None
    } else {
        Some(NgResult { t, v })
    }
}

/// Linear interpolation of an ngspice waveform onto an arbitrary time.
fn interp(res: &NgResult, time: f64) -> f64 {
    if time <= res.t[0] {
        return res.v[0];
    }
    let last = res.t.len() - 1;
    if time >= res.t[last] {
        return res.v[last];
    }
    // Binary search for the bracketing interval.
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
    let frac = (time - res.t[lo]) / span;
    res.v[lo] + (res.v[hi] - res.v[lo]) * frac
}

/// Max relative error of our `out` waveform vs interpolated ngspice, using a
/// full-scale floor so transient zero-crossings don't blow up the ratio.
fn compare(
    ours_t: &[f64],
    ours_v: &[f64],
    ng: &NgResult,
    full_scale: f64,
    skip_initial: f64,
) -> f64 {
    let floor = 0.01 * full_scale;
    let mut worst = 0.0f64;
    for (&t, &ov) in ours_t.iter().zip(ours_v) {
        if t < skip_initial {
            continue;
        }
        let nv = interp(ng, t);
        let e = (ov - nv).abs() / (nv.abs().max(floor));
        worst = worst.max(e);
    }
    worst
}

fn load(net: &str) -> Result<hauksbee_ir::Circuit, SpiceError> {
    SpiceLoader::load(net)
}

#[test]
fn half_wave_rectifier_with_smoothing() {
    // Sine source -> diode -> RC smoothing. Classic peak-detector ripple.
    let net = r#"halfwave
V1 in 0 SIN(0 5 1k 0 0 0)
D1 in out DMOD
R1 out 0 10k
C1 out 0 10u
.model DMOD D(IS=1e-14 N=1.0 RS=0.1)
.tran 5u 5m uic
.print tran v(out)
.options reltol=1e-4
.end
"#;

    let ng = match run_ngspice(net) {
        Some(r) => r,
        None => {
            eprintln!("ngspice not available; skipping half_wave_rectifier");
            return;
        }
    };

    let circuit = load(net).expect("load netlist");
    let opts = SolverOptions {
        integration: Integration::Trapezoidal,
        step: StepControl::Adaptive {
            dt_initial: 1e-7,
            dt_min: 1e-12,
            dt_max: 5e-6,
        },
        reltol: 1e-4,
        ..SolverOptions::default()
    };
    let wf = Transient::new(opts).run(&circuit, 5e-3).unwrap();
    let out = wf.node(&circuit, "out").unwrap();

    // Skip the first quarter period while the cap charges from the IC.
    let err = compare(&wf.time, out, &ng, 5.0, 1e-3);
    eprintln!("RECTIFIER err = {err:.4}"); assert!(err < 0.01, "rectifier max rel err {err:.4} (>1%)");
}

#[test]
fn common_emitter_amplifier_transient() {
    // NPN common-emitter, DC-biased (no uic, so ngspice computes the real
    // operating point too). A small sine on the base swings the collector; we
    // compare the collector waveform. Direct base drive avoids coupling-cap
    // settling that would make `uic` runs diverge for trivial reasons.
    let net = r#"ce amp
VCC vcc 0 DC 12
VB base 0 SIN(1.9 0.02 5k 0 0 0)
RC vcc coll 4.7k
RE emit 0 1k
Q1 coll base emit QMOD
.model QMOD NPN(IS=1e-15 BF=200 VAF=80 NF=1.0)
.tran 2u 1m
.print tran v(coll)
.options reltol=1e-4
.end
"#;

    let ng = match run_ngspice(net) {
        Some(r) => r,
        None => {
            eprintln!("ngspice not available; skipping common_emitter");
            return;
        }
    };

    let circuit = load(net).expect("load netlist");
    let opts = SolverOptions {
        integration: Integration::Trapezoidal,
        step: StepControl::Adaptive {
            dt_initial: 1e-7,
            dt_min: 1e-12,
            dt_max: 2e-6,
        },
        reltol: 1e-4,
        ..SolverOptions::default()
    };
    let wf = Transient::new(opts).run(&circuit, 1e-3).unwrap();
    let coll = wf.node(&circuit, "coll").unwrap();

    // Both sides bias from the DC operating point; compare the whole run.
    let err = compare(&wf.time, coll, &ng, 12.0, 0.0);
    eprintln!("CE_AMP err = {err:.4}");
    assert!(err < 0.02, "CE amp max rel err {err:.4} (>2%)");
}

#[test]
fn rc_ladder_20_stages() {
    // 20-stage RC ladder driven by a step. Tests accuracy of a deep linear
    // chain (the sparse solver's bread and butter).
    let mut net = String::from("rc ladder\nV1 n0 0 PULSE(0 1 0 1n 1n 1 1)\n");
    let stages = 20;
    for i in 0..stages {
        net.push_str(&format!("R{} n{} n{} 1k\n", i + 1, i, i + 1));
        net.push_str(&format!("C{} n{} 0 10n\n", i + 1, i + 1));
    }
    net.push_str(".tran 1u 2m uic\n.print tran v(n20)\n.options reltol=1e-4\n.end\n");

    let ng = match run_ngspice(&net) {
        Some(r) => r,
        None => {
            eprintln!("ngspice not available; skipping rc_ladder");
            return;
        }
    };

    let circuit = load(&net).expect("load netlist");
    let opts = SolverOptions {
        integration: Integration::Trapezoidal,
        step: StepControl::Adaptive {
            dt_initial: 1e-7,
            dt_min: 1e-12,
            dt_max: 5e-6,
        },
        reltol: 1e-4,
        ..SolverOptions::default()
    };
    let wf = Transient::new(opts).run(&circuit, 2e-3).unwrap();
    let last = wf.node(&circuit, "n20").unwrap();

    let err = compare(&wf.time, last, &ng, 1.0, 5e-5);
    eprintln!("RC_LADDER err = {err:.4}"); assert!(err < 0.01, "RC ladder max rel err {err:.4} (>1%)");
}
