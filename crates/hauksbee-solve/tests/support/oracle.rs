//! Shared oracle discovery, correctness cache and uncached benchmark execution.

// The accuracy and timing harnesses use different subsets of this shared module.
#![allow(dead_code)]

use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut s, byte| {
        write!(s, "{byte:02x}").unwrap();
        s
    })
}

pub fn output_path(variable: &str, name: &str) -> PathBuf {
    let path = std::env::var_os(variable).map_or_else(|| build_dir().join(name), PathBuf::from);
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).expect("create oracle output directory");
    }
    path
}

fn build_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/oracle")
}

fn has_diagnostic(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("warning") || lower.contains("error")
}

/// Parse the finite node voltages and source branch currents in a batch `.op` listing.
pub fn parse_op_listing(text: &str) -> Option<HashMap<String, f64>> {
    let mut values = HashMap::new();
    let mut inside = false;
    for line in text.lines() {
        let words: Vec<_> = line.split_whitespace().collect();
        if words == ["Node", "Voltage"] || words == ["Source", "Current"] {
            inside = true;
        } else if inside && line.contains("models") {
            break;
        } else if inside && words.len() == 2 && !words[0].starts_with('-') {
            let value = words[1].parse::<f64>().ok()?;
            if !value.is_finite()
                || values
                    .insert(words[0].to_ascii_lowercase(), value)
                    .is_some()
            {
                return None;
            }
        } else if inside && !words.is_empty() && words.len() != 2 {
            break;
        }
    }
    Some(values)
}

/// Only complete, successful batch output is reusable; diagnostics never earn a cache entry.
fn complete(text: &str) -> bool {
    let rows = text.lines().find_map(|line| {
        line.trim()
            .strip_prefix("No. of Data Rows :")?
            .trim()
            .parse::<usize>()
            .ok()
    });
    rows.is_some_and(|n| {
        n > 0
            && if text.contains("Index") {
                parse_indexed_table(text).is_some_and(|t| t.x.len() == n)
            } else {
                parse_op_listing(text).is_some_and(|values| !values.is_empty())
            }
    }) && text.contains("Total analysis time (seconds)")
        && !has_diagnostic(text)
}

pub struct CachedOracle {
    bin: PathBuf,
    identity: Vec<u8>,
    directory: PathBuf,
}

impl CachedOracle {
    pub fn new(bin: &Path, directory: &Path) -> Self {
        let bin = bin.canonicalize().expect("canonical oracle binary");
        let identity = [
            Sha256::digest(std::fs::read(&bin).expect("read oracle binary")).to_vec(),
            ngspice_version(&bin).into_bytes(),
            include_bytes!("oracle.rs").to_vec(),
        ]
        .concat();
        Self {
            bin,
            identity,
            directory: directory.to_path_buf(),
        }
    }

    fn cache_path(&self, netlist: &str) -> Option<PathBuf> {
        // These overrides may point at mutable external models/code: run them uncached.
        if std::env::vars_os().any(|(name, _)| name.to_string_lossy().starts_with("SPICE")) {
            return None;
        }
        let mut hash = Sha256::new();
        hash.update(&self.identity);
        hash.update(std::fs::read(&self.bin).ok()?);
        hash.update(b"-n -b LC_ALL=C");
        hash_inputs(&mut hash, netlist, &self.directory, 0).then(|| {
            build_dir()
                .join("cache")
                .join(format!("{}.json", hex(&hash.finalize())))
        })
    }

    pub fn run(&self, netlist: &str) -> Option<String> {
        let cached = self.cache_path(netlist);
        if let Some(path) = &cached {
            if let Ok(bytes) = std::fs::read(path) {
                if let Ok((checksum, text)) = serde_json::from_slice::<(String, String)>(&bytes) {
                    if checksum == hex(&Sha256::digest(text.as_bytes())) && complete(&text) {
                        eprintln!("oracle cache hit: {}", path.display());
                        return Some(text);
                    }
                }
            }
        }
        let (text, _) = execute(&self.bin, netlist, &self.directory)?;
        // Changed inputs during execution cannot be associated with the old key.
        if let Some(path) = cached.filter(|path| Some(path) == self.cache_path(netlist).as_ref()) {
            let record = (hex(&Sha256::digest(text.as_bytes())), &text);
            if std::fs::create_dir_all(path.parent()?).is_ok() {
                let _ = std::fs::write(path, serde_json::to_vec(&record).ok()?);
            }
        }
        Some(text)
    }
}

fn hash_inputs(hash: &mut Sha256, text: &str, directory: &Path, depth: usize) -> bool {
    if depth > 20 {
        return false;
    }
    hash.update(directory.to_string_lossy().as_bytes());
    hash.update(text.len().to_le_bytes());
    hash.update(text.as_bytes());
    let mut include = false;
    for raw in text.lines().skip(usize::from(depth == 0)) {
        let line = raw.trim().to_ascii_lowercase();
        let mut words = line.split_whitespace();
        let Some(card) = words.next() else { continue };
        if card.starts_with('*') {
            continue;
        }
        // Cache only self-contained cards and unambiguous includes. Control scripts,
        // section libraries, code models and external/random sources execute uncached.
        if line.contains("file")
            || line.contains("random")
            || line.contains("gauss")
            || line.contains("unif")
            || (include && card.starts_with('+'))
        {
            return false;
        }
        include = matches!(card, ".include" | ".inc");
        if !include {
            if card.starts_with('.')
                && ![
                    ".op", ".tran", ".dc", ".ac", ".print", ".plot", ".model", ".param",
                    ".options", ".temp", ".ic", ".nodeset", ".subckt", ".ends", ".end",
                ]
                .contains(&card)
                || !card.starts_with('.')
                    && !"rclvidqmskbegfhx+".contains(card.chars().next().unwrap())
            {
                return false;
            }
            continue;
        }
        // Use original spelling for case-sensitive paths; whitespace/continuations
        // and alternate include search paths deliberately do not earn cache hits.
        let args: Vec<_> = raw.split_whitespace().skip(1).collect();
        let [file] = args.as_slice() else {
            return false;
        };
        let Ok(path) = directory
            .join(file.trim_matches(['\'', '"']))
            .canonicalize()
        else {
            return false;
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return false;
        };
        if !hash_inputs(hash, &text, path.parent().unwrap(), depth + 1) {
            return false;
        }
    }
    true
}

fn execute(bin: &Path, netlist: &str, directory: &Path) -> Option<(String, Duration)> {
    let scratch = tempfile::tempdir().ok()?;
    let path = scratch.path().join("input.cir");
    std::fs::write(&path, netlist).ok()?;
    let start = Instant::now();
    let output = Command::new(bin)
        .args(["-n", "-b"])
        .arg(path)
        .current_dir(directory)
        .env("LC_ALL", "C")
        .output()
        .ok()?;
    let elapsed = start.elapsed();
    let stdout = String::from_utf8(output.stdout).ok()?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() || !complete(&stdout) || has_diagnostic(&stderr) {
        eprintln!("ngspice failed: {stderr}\n{stdout}");
        return None;
    }
    Some((stdout, elapsed))
}

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
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|dir| dir.join(exe))
        .chain(
            [
                "/opt/homebrew/bin/ngspice",
                "/usr/local/bin/ngspice",
                "/usr/bin/ngspice",
                "/opt/local/bin/ngspice",
                "C:\\Program Files\\ngspice\\bin\\ngspice_con.exe",
                "C:\\Program Files\\ngspice\\bin\\ngspice.exe",
            ]
            .map(PathBuf::from),
        )
        .find(|path| path.is_file())
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

/// Uncached `(stdout, wall time)`, including process start. The gate subtracts a
/// separately measured startup floor rather than claiming fork/exec as speedup.
pub fn run_ngspice(bin: &Path, netlist: &str) -> Option<(String, Duration)> {
    execute(bin, netlist, &std::env::current_dir().ok()?)
}

/// Best-of-`n` actual executions, matching our solver's minimum wall time.
/// A mean folds scheduler/thermal noise into a small startup-subtracted signal.
pub fn run_ngspice_best_of(bin: &Path, netlist: &str, n: usize) -> Option<(String, Duration)> {
    let mut best: Option<(String, Duration)> = None;
    for _ in 0..n.max(1) {
        let (out, d) = run_ngspice(bin, netlist)?;
        if best.as_ref().is_none_or(|(_, bd)| d < *bd) {
            best = Some((out, d));
        }
    }
    best
}

/// Minimum of five trivial `.op` runs: the process-start floor, measured uncached.
pub fn ngspice_startup_cost(bin: &Path) -> Duration {
    const TRIVIAL: &str = "startup probe\nV1 a 0 DC 1\nR1 a 0 1k\n.op\n.end\n";
    run_ngspice_best_of(bin, TRIVIAL, 5)
        .expect("ngspice startup measurement")
        .1
}

/// Parse an ngspice `.print tran` three-column table into `(time, value)` pairs.
///
/// The batch listing wraps the table in headers, dashes and page breaks; only
/// lines that parse as `<index> <time> <value>` are data.
pub fn parse_tran_table(stdout: &str) -> Vec<(f64, f64)> {
    parse_indexed_table(stdout)
        .filter(|table| table.cols.len() == 1)
        .map_or_else(Vec::new, |table| {
            table
                .x
                .into_iter()
                .zip(table.cols.into_iter().next().unwrap())
                .collect()
        })
}

pub struct NgTable {
    pub x: Vec<f64>,
    pub cols: Vec<Vec<f64>>,
}

/// Accept only consecutive, finite, equal-width rows from an indexed batch table.
pub fn parse_indexed_table(text: &str) -> Option<NgTable> {
    let mut table = NgTable {
        x: Vec::new(),
        cols: Vec::new(),
    };
    for line in text.lines() {
        let mut words = line.split_whitespace();
        let Some(index) = words.next().and_then(|s| s.parse::<usize>().ok()) else {
            continue;
        };
        let values: Vec<f64> = words.map(str::parse).collect::<Result<_, _>>().ok()?;
        if index != table.x.len() || values.len() < 2 || values.iter().any(|v| !v.is_finite()) {
            return None;
        }
        if table.cols.is_empty() {
            table.cols.resize_with(values.len() - 1, Vec::new);
        }
        if values.len() != table.cols.len() + 1 {
            return None;
        }
        table.x.push(values[0]);
        for (column, value) in table.cols.iter_mut().zip(&values[1..]) {
            column.push(*value);
        }
    }
    (!table.x.is_empty()).then_some(table)
}

#[cfg(unix)]
#[test]
fn cache_tracks_inputs_and_never_reuses_failed_or_timed_runs() {
    use std::os::unix::fs::PermissionsExt;
    const GOOD: &str = "No. of Data Rows : 2\nIndex time v(out)\n0 0 1\n1 1 2\nTotal analysis time (seconds) = 0\n";
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("oracle");
    let count = dir.path().join("count");
    let install = |body: &str| {
        std::fs::write(
            &bin,
            format!(
                "#!/bin/sh\n[ \"$1\" = --version ] && exit 0\nprintf x >> '{}'\n{body}\n",
                count.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o700)).unwrap();
    };
    let success = format!("printf '%s' '{GOOD}'");
    install(&success);
    let oracle = CachedOracle::new(&bin, dir.path());
    let deck = "test\n.include Model.inc\n.end\n";
    std::fs::write(dir.path().join("Model.inc"), ".inc nested.inc\n").unwrap();
    std::fs::write(dir.path().join("nested.inc"), "R1 a 0 1k\n").unwrap();
    let path = oracle.cache_path(deck).unwrap();
    assert_eq!(oracle.run(deck).as_deref(), Some(GOOD));
    assert_eq!(oracle.run(deck).as_deref(), Some(GOOD));
    assert_eq!(std::fs::read(&count).unwrap().len(), 1);
    std::fs::write(&path, "truncated JSON").unwrap();
    assert_eq!(oracle.run(deck).as_deref(), Some(GOOD));
    assert_eq!(std::fs::read(&count).unwrap().len(), 2);
    std::fs::write(dir.path().join("nested.inc"), "R1 a 0 2k\n").unwrap();
    assert_ne!(oracle.cache_path(deck).unwrap(), path);
    assert_eq!(oracle.run(deck).as_deref(), Some(GOOD));
    assert_eq!(std::fs::read(&count).unwrap().len(), 3);
    assert_ne!(
        oracle.cache_path("changed\nR1 a 0 1k\n.end"),
        oracle.cache_path(deck)
    );
    for card in [
        ".lib Model.inc section",
        ".control",
        ".include missing",
        ".inc\n+ Model.inc",
        "V1 a 0 pwl(file=data)",
    ] {
        assert!(
            oracle.cache_path(&format!("test\n{card}\n.end")).is_none(),
            "{card}"
        );
    }
    run_ngspice_best_of(&bin, deck, 3).unwrap();
    assert_eq!(
        std::fs::read(&count).unwrap().len(),
        6,
        "timings always execute"
    );
    for bad in [
        GOOD.replace("1 1 2", "2 1 2"),
        GOOD.replace("1 1 2", "1 1 NaN"),
        GOOD.replace("1 1 2", "1 1 2 3"),
        GOOD.replace("1 1 2", ""),
        GOOD.replace("Total analysis", "Partial analysis"),
        format!("{GOOD}Warning: failed\n"),
        format!("{GOOD}0 0 1\n"),
    ] {
        assert!(!complete(&bad), "{bad}");
    }
    let op = "No. of Data Rows : 1\nNode Voltage\na 1\nSource Current\nv1#branch -0.001\nResistor models\nTotal analysis time (seconds) = 0\n";
    assert!(complete(op));
    assert!(!complete(&op.replace("a 1", "a NaN")));
    assert!(!complete(&op.replace("a 1", "a malformed")));
    for ending in [
        "exit 7",
        "printf 'Warning: rejected' >&2",
        "printf 'Error: rejected'",
        "printf 'incomplete'",
    ] {
        install(&format!(
            "{}\n{ending}",
            if ending == "printf 'incomplete'" {
                ""
            } else {
                &success
            }
        ));
        let failed_path = oracle.cache_path(deck).unwrap();
        assert!(oracle.run(deck).is_none(), "{ending}");
        assert!(!failed_path.exists(), "failed output cached: {ending}");
    }
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
    let hi = series
        .partition_point(|&(x, _)| x <= t)
        .min(series.len() - 1);
    let (t0, v0) = series[hi - 1];
    let (t1, v1) = series[hi];
    if (t1 - t0).abs() < f64::EPSILON {
        return v1;
    }
    v0 + (v1 - v0) * (t - t0) / (t1 - t0)
}
