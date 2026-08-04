//! Shared ngspice-oracle plumbing for the benchmark gate.
//!
//! Lives in a `tests/` SUBDIRECTORY on purpose: cargo compiles every top-level
//! `tests/*.rs` as its own test binary, but not files nested a level down, so
//! this can be `#[path]`-included by a gate without becoming a test target that
//! asserts nothing.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// Locate the ngspice binary: `$NGSPICE`, then `PATH`, then per-OS defaults.
///
/// Same search order as the accuracy harness in `tests/ngspice.rs`, so a machine
/// that runs one runs the other.
pub fn find_ngspice() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("NGSPICE") {
        let pb = PathBuf::from(&p);
        if pb.is_file() {
            return Some(pb);
        }
    }
    let exe = if cfg!(windows) {
        "ngspice.exe"
    } else {
        "ngspice"
    };
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

/// The ngspice version token (e.g. `ngspice-45.2`), for the results table.
pub fn ngspice_version(bin: &Path) -> String {
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
            return tail
                .chars()
                .take_while(|c| !c.is_whitespace() && *c != ':')
                .collect();
        }
    }
    "unknown".to_string()
}

/// Run a netlist through `ngspice -b`, returning `(stdout, wall time)`.
///
/// The wall time INCLUDES process start, because that is what a user running
/// ngspice actually waits for and what the published comparison measured. The
/// gate reports the process-start cost separately so the solver-only ratio is
/// visible too, rather than letting ~40 ms of fork/exec quietly inflate a
/// speed claim.
pub fn run_ngspice(bin: &Path, netlist: &str, tag: &str) -> Option<(String, Duration)> {
    let path = std::env::temp_dir().join(format!("hauksbee_gate_{tag}.cir"));
    std::fs::write(&path, netlist).ok()?;
    let t0 = Instant::now();
    let out = Command::new(bin).arg("-b").arg(&path).output().ok()?;
    let elapsed = t0.elapsed();
    let _ = std::fs::remove_file(&path);
    Some((String::from_utf8_lossy(&out.stdout).to_string(), elapsed))
}

/// Best-of-`n` wall time for a netlist through `ngspice -b`.
///
/// The MINIMUM, not the mean. Both sides of this comparison are measuring a
/// floor: the least time the machine needed. A mean folds in scheduler
/// preemption and thermal noise, and on a small circuit whose solver time is the
/// difference of two ~15 ms numbers that noise is the whole signal, which is how
/// a stable ratio turns into one that swings 2x between runs and makes any floor
/// either useless or flaky.
pub fn run_ngspice_best_of(
    bin: &Path,
    netlist: &str,
    tag: &str,
    n: usize,
) -> Option<(String, Duration)> {
    let mut best: Option<(String, Duration)> = None;
    for i in 0..n.max(1) {
        let (out, d) = run_ngspice(bin, netlist, &format!("{tag}{i}"))?;
        if best.as_ref().is_none_or(|(_, bd)| d < *bd) {
            best = Some((out, d));
        }
    }
    best
}

/// Wall time of the cheapest possible `ngspice -b` run: a deck that declares one
/// resistor and stops. Everything above this floor is real solver work, so
/// subtracting it turns an "includes process start" ratio into a solver-only one.
///
/// Measured as the MINIMUM of several runs: process start is a floor, and the
/// minimum is the least noisy estimate of a floor.
pub fn ngspice_startup_cost(bin: &Path) -> Duration {
    const TRIVIAL: &str = "startup probe\nV1 a 0 DC 1\nR1 a 0 1k\n.op\n.end\n";
    let mut best = Duration::from_secs(3600);
    for i in 0..5 {
        if let Some((_, d)) = run_ngspice(bin, TRIVIAL, &format!("startup{i}")) {
            best = best.min(d);
        }
    }
    best
}

/// Parse an ngspice `.print tran` three-column table into `(time, value)` pairs.
///
/// The batch listing wraps the table in headers, dashes and page breaks; only
/// lines that parse as `<index> <time> <value>` are data.
pub fn parse_tran_table(stdout: &str) -> Vec<(f64, f64)> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let mut it = line.split_whitespace();
        let (Some(a), Some(b), Some(c)) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        if it.next().is_some() {
            continue;
        }
        if a.parse::<usize>().is_err() {
            continue;
        }
        let (Ok(t), Ok(v)) = (b.parse::<f64>(), c.parse::<f64>()) else {
            continue;
        };
        out.push((t, v));
    }
    out
}

/// Worst relative error between our waveform and the oracle's, sampled at the
/// oracle's own time points with linear interpolation into ours.
///
/// The denominator is `max(|oracle|, floor)` where the floor is a small fraction
/// of the oracle's full scale, so a waveform crossing zero does not manufacture
/// an infinite relative error out of a perfectly good absolute agreement.
pub fn worst_rel_error(ours: &[(f64, f64)], oracle: &[(f64, f64)]) -> f64 {
    if ours.len() < 2 || oracle.is_empty() {
        return f64::INFINITY;
    }
    let full_scale = oracle
        .iter()
        .map(|(_, v)| v.abs())
        .fold(0.0f64, f64::max)
        .max(1e-12);
    let floor = 0.01 * full_scale;
    let t_last = ours[ours.len() - 1].0;
    let mut worst = 0.0f64;
    for &(t, want) in oracle {
        // Do not extrapolate: a time past our last sample is not a comparison.
        if t > t_last {
            continue;
        }
        let got = interp(ours, t);
        let denom = want.abs().max(floor);
        worst = worst.max((got - want).abs() / denom);
    }
    worst
}

/// Linear interpolation into a time-ordered series.
fn interp(series: &[(f64, f64)], t: f64) -> f64 {
    if t <= series[0].0 {
        return series[0].1;
    }
    // Binary search for the bracketing pair.
    let mut lo = 0usize;
    let mut hi = series.len() - 1;
    while hi - lo > 1 {
        let mid = (lo + hi) / 2;
        if series[mid].0 <= t {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    let (t0, v0) = series[lo];
    let (t1, v1) = series[hi];
    if (t1 - t0).abs() < f64::EPSILON {
        return v1;
    }
    v0 + (v1 - v0) * (t - t0) / (t1 - t0)
}
