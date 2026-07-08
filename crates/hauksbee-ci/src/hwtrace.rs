//! Hardware-trace comparison: the T6 oracle tier (validation plan §T6).
//! Long-form how-and-why: docs/how-and-why/hauksbee-ci/hwtrace.md.
//!
//! A *hardware trace* is a captured waveform from a physical board — an
//! oscilloscope CSV export or a logic-analyzer VCD — checked into
//! `testdata/hwtraces/<board>/<scenario>/` beside a `trace.toml` that states
//! where it came from (instrument, probe point, **provenance**) and which
//! *features* of it the simulation must reproduce. A `[[assert]]` block with
//! `kind = "hwtrace"` in a CI spec points at the `trace.toml`; the runner
//! records the referenced nets' simulated waveforms and this module compares
//! feature-by-feature.
//!
//! **Feature-based, never pointwise.** Real hardware carries component
//! tolerances, probe loading, supply drift, and timing jitter that a correct
//! simulation legitimately will not match sample-for-sample. What an EE
//! actually checks on a scope is a small set of derived quantities — the
//! settled level, the peak, the period and duty of an oscillation, the pulse
//! width, how many edges fired. Those are the vocabulary here, each compared
//! within its own stated tolerance (hardware traces carry their own error
//! bars; the trace file states them). A pointwise diff would fail every
//! honest capture and pass only a capture faked from the sim itself.
//!
//! **The provenance rule.** Every trace MUST declare
//! `provenance = "real" | "synthetic"`. `real` means the data came off an
//! instrument probing a physical board. `synthetic` means it was constructed
//! (from datasheet-typical behavior, another simulator, or by hand) to prove
//! the pipeline — useful as scaffolding, but it validates the *harness*, not
//! the simulator. A synthetic trace passed off as hardware is exactly the
//! fake this repo refuses; the field is mandatory so the label can never be
//! omitted silently.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::SpecError;
use crate::spec::Spec;

// ── trace.toml model ──────────────────────────────────────────────────────────

/// A parsed, validated `trace.toml`: one capture session on one board, with
/// one or more probed channels and the feature assertions per channel.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Trace {
    /// The capture metadata block (`[trace]`).
    pub trace: TraceMeta,
    /// The probed channels (`[[channel]]`), each with its data file and features.
    #[serde(rename = "channel")]
    pub channels: Vec<Channel>,

    /// Directory the trace was loaded from (for resolving data files). Not part
    /// of the TOML; filled in by [`Trace::load`].
    #[serde(skip)]
    pub base_dir: PathBuf,
}

/// The `[trace]` metadata: where this capture came from and under what
/// conditions. `provenance` is deliberately non-optional (see module doc).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TraceMeta {
    /// The board this was captured from (human reference, e.g. a board file
    /// path or a hardware revision string).
    pub board: String,
    /// What scenario the board was in (firmware state, stimulus, supply).
    pub scenario: String,
    /// `"real"` (captured from a physical board) or `"synthetic"`
    /// (constructed; validates the harness, not the simulator). REQUIRED.
    pub provenance: String,
    /// The instrument (e.g. "Rigol DS1054Z, 10x passive probe") — or, for a
    /// synthetic trace, how it was constructed. REQUIRED so a trace can never
    /// silently omit its source.
    pub instrument: String,
    /// Capture date (free-form, e.g. "2026-07-08").
    #[serde(default)]
    pub date: Option<String>,
    /// Anything else worth recording: supply voltage, temperature, trigger
    /// setup, known quirks of the specimen board.
    #[serde(default)]
    pub notes: Option<String>,
}

/// One probed channel: a net, its captured data file, and the features the
/// simulation must reproduce within tolerance.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Channel {
    /// The net name this probe was on (must match the board's net name; the
    /// simulated waveform is sampled from the same net).
    pub net: String,
    /// The data file beside the trace.toml: `.csv` (scope export: time,
    /// voltage columns; preamble lines are skipped) or `.vcd` (logic-analyzer
    /// export; digital 0/1, so only timing features are allowed on it).
    pub file: String,
    /// Where the probe physically was (e.g. "U1 pin 19, 10x passive").
    #[serde(default)]
    pub probe: Option<String>,
    /// For VCD files with more than one signal: the `$var` reference name of
    /// the signal to read. Optional when the file has exactly one signal.
    #[serde(default)]
    pub signal: Option<String>,
    /// The feature assertions on this channel.
    #[serde(rename = "feature")]
    pub features: Vec<Feature>,
}

/// One feature assertion: extract the same quantity from the captured and the
/// simulated waveform, compare within the stated tolerance. Each kind is
/// something an EE would read off a scope's measure menu — nothing fancier.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Feature {
    /// `level | min | max | period | duty | pulse_width | edge_count`.
    pub kind: String,
    /// Ignore samples before this time (ms) — lets the boot transient pass.
    #[serde(default)]
    pub after_ms: f64,
    /// Edge threshold in volts for the timing features. Default: 50% of each
    /// waveform's own swing (the scope-measure-menu convention), which also
    /// makes a digital 0/1 VCD and an analog capture comparable.
    #[serde(default)]
    pub threshold: Option<f64>,
    /// Absolute tolerance in the feature's own unit (V for level/min/max,
    /// ms for period/pulse_width, fraction for duty, count for edge_count).
    #[serde(default)]
    pub abstol: Option<f64>,
    /// Relative tolerance as a fraction of the captured value.
    #[serde(default)]
    pub reltol: Option<f64>,
}

/// The feature kinds and their units, for validation and reporting.
const FEATURE_KINDS: &[(&str, &str)] = &[
    ("level", "V"),         // time-weighted mean (the settled/average level)
    ("min", "V"),           // minimum voltage
    ("max", "V"),           // maximum voltage (peak)
    ("period", "ms"),       // mean rising-to-rising edge spacing
    ("duty", "frac"),       // high-time fraction over complete periods
    ("pulse_width", "ms"),  // mean high-pulse (rising-to-falling) duration
    ("edge_count", "edges"), // total threshold crossings (both directions)
];

/// Timing features are meaningful on a digital (VCD) channel; the voltage
/// features are not — a logic analyzer records bits, not volts, and comparing
/// its 0/1 levels against simulated volts would be a fake agreement.
const VCD_ALLOWED: &[&str] = &["period", "duty", "pulse_width", "edge_count"];

impl Feature {
    fn unit(&self) -> &'static str {
        FEATURE_KINDS
            .iter()
            .find(|(k, _)| *k == self.kind)
            .map(|(_, u)| *u)
            .unwrap_or("?")
    }
}

impl Trace {
    /// Load and validate a `trace.toml`. Fails loud with a named reason on any
    /// structural problem — a bad trace must never silently skip.
    pub fn load(path: &Path) -> Result<Self, SpecError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| SpecError::Io(format!("reading trace {}: {e}", path.display())))?;
        let mut trace: Trace = toml::from_str(&text).map_err(|e| SpecError::Toml {
            file: path.display().to_string(),
            message: e.message().to_string(),
        })?;
        trace.base_dir = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        trace.validate(path)?;
        Ok(trace)
    }

    fn validate(&self, path: &Path) -> Result<(), SpecError> {
        let ctx = path.display();
        match self.trace.provenance.as_str() {
            "real" | "synthetic" => {}
            other => {
                return Err(SpecError::Invalid(format!(
                    "{ctx}: provenance must be \"real\" or \"synthetic\", got \"{other}\". \
                     A trace that is not a capture from a physical board must say so."
                )))
            }
        }
        if self.channels.is_empty() {
            return Err(SpecError::Invalid(format!(
                "{ctx}: a trace with no [[channel]] blocks compares nothing"
            )));
        }
        for ch in &self.channels {
            if ch.features.is_empty() {
                return Err(SpecError::Invalid(format!(
                    "{ctx}: channel '{}' has no [[channel.feature]] blocks — with no \
                     features the channel passes vacuously",
                    ch.net
                )));
            }
            let is_vcd = ch.file.to_ascii_lowercase().ends_with(".vcd");
            for f in &ch.features {
                if !FEATURE_KINDS.iter().any(|(k, _)| *k == f.kind) {
                    let known: Vec<&str> = FEATURE_KINDS.iter().map(|(k, _)| *k).collect();
                    return Err(SpecError::Invalid(format!(
                        "{ctx}: channel '{}': unknown feature kind '{}' (expected one of {})",
                        ch.net,
                        f.kind,
                        known.join("|")
                    )));
                }
                if f.abstol.is_none() && f.reltol.is_none() {
                    return Err(SpecError::Invalid(format!(
                        "{ctx}: channel '{}' feature '{}' needs an `abstol` and/or `reltol` — \
                         a hardware trace carries its own error bars and must state them",
                        ch.net, f.kind
                    )));
                }
                if is_vcd && !VCD_ALLOWED.contains(&f.kind.as_str()) {
                    return Err(SpecError::Invalid(format!(
                        "{ctx}: channel '{}': feature '{}' is a voltage feature, but '{}' is a \
                         VCD (logic-analyzer) file which records bits, not volts. Only timing \
                         features ({}) are honest on a digital capture",
                        ch.net,
                        f.kind,
                        ch.file,
                        VCD_ALLOWED.join("|")
                    )));
                }
            }
        }
        Ok(())
    }

    /// Load one channel's captured waveform as a `(t_seconds, value)` series.
    pub fn load_channel(&self, ch: &Channel) -> Result<Vec<(f64, f64)>, SpecError> {
        let path = if Path::new(&ch.file).is_absolute() {
            PathBuf::from(&ch.file)
        } else {
            self.base_dir.join(&ch.file)
        };
        if ch.file.to_ascii_lowercase().ends_with(".vcd") {
            load_vcd(&path, ch.signal.as_deref())
        } else {
            load_csv(&path)
        }
    }
}

/// Every net referenced by the spec's `hwtrace` assertions, with each trace
/// loaded and validated up front (fail loud before the sim spends minutes).
/// The runner records these nets' simulated waveforms during the run.
pub fn assert_nets(spec: &Spec) -> Result<HashSet<String>, SpecError> {
    let mut nets = HashSet::new();
    for a in &spec.asserts {
        if a.kind != "hwtrace" {
            continue;
        }
        let trace = Trace::load(&trace_path(spec, a)?)?;
        for ch in &trace.channels {
            nets.insert(ch.net.clone());
        }
    }
    Ok(nets)
}

/// Resolve an hwtrace assertion's `trace` path against the spec's directory.
pub fn trace_path(spec: &Spec, a: &crate::spec::Assertion) -> Result<PathBuf, SpecError> {
    let rel = a.trace.as_deref().ok_or_else(|| {
        SpecError::Invalid("hwtrace assertion needs a `trace` path".to_string())
    })?;
    Ok(if Path::new(rel).is_absolute() {
        PathBuf::from(rel)
    } else {
        spec.base_dir.join(rel)
    })
}

// ── capture-file loaders ──────────────────────────────────────────────────────

/// Load a scope CSV export: any line whose first two comma/semicolon/tab/space
/// separated fields parse as floats is a `(time_s, volts)` sample; everything
/// else (instrument preamble, column headers, units rows) is skipped. This is
/// deliberately permissive because every scope vendor writes a different
/// header, but it fails loud when fewer than 8 samples survive — a file that
/// is all preamble is a wrong file, not an empty waveform.
fn load_csv(path: &Path) -> Result<Vec<(f64, f64)>, SpecError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| SpecError::Io(format!("reading capture {}: {e}", path.display())))?;
    let mut out = Vec::new();
    for line in text.lines() {
        let mut fields = line.split([',', ';', '\t', ' ']).filter(|s| !s.is_empty());
        let (Some(a), Some(b)) = (fields.next(), fields.next()) else {
            continue;
        };
        if let (Ok(t), Ok(v)) = (a.trim().parse::<f64>(), b.trim().parse::<f64>()) {
            out.push((t, v));
        }
    }
    if out.len() < 8 {
        return Err(SpecError::Invalid(format!(
            "{}: only {} numeric (time, value) rows found — not a waveform capture",
            path.display(),
            out.len()
        )));
    }
    if out.windows(2).any(|w| w[1].0 < w[0].0) {
        return Err(SpecError::Invalid(format!(
            "{}: time column is not monotonically non-decreasing",
            path.display()
        )));
    }
    Ok(out)
}

/// Load a logic-analyzer VCD: minimal parser for the subset every LA export
/// uses (`$timescale`, `$var`, `#time`, scalar `0<id>`/`1<id>` changes).
/// Returns a `(t_seconds, value)` event series with values 0.0/1.0 — the
/// digital levels, which is why only timing features are allowed on VCD
/// channels. `signal` picks the `$var` by reference name; with exactly one
/// signal in the file it may be omitted.
fn load_vcd(path: &Path, signal: Option<&str>) -> Result<Vec<(f64, f64)>, SpecError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| SpecError::Io(format!("reading capture {}: {e}", path.display())))?;
    let ctx = path.display();

    // Header: timescale and var declarations.
    let mut timescale_s: Option<f64> = None;
    let mut vars: Vec<(String, String)> = Vec::new(); // (id, name)
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with("$timescale") {
            // "$timescale 1us $end" or the value on the following token.
            let tok: String = l
                .trim_start_matches("$timescale")
                .replace("$end", "")
                .split_whitespace()
                .collect();
            timescale_s = parse_timescale(&tok);
        } else if l.starts_with("$var") {
            // $var wire 1 <id> <name> [...] $end
            let parts: Vec<&str> = l.split_whitespace().collect();
            if parts.len() >= 5 {
                vars.push((parts[3].to_string(), parts[4].to_string()));
            }
        } else if l.starts_with("$enddefinitions") {
            break;
        }
    }
    let ts = timescale_s.ok_or_else(|| {
        SpecError::Invalid(format!("{ctx}: VCD has no parseable $timescale"))
    })?;
    let id = match signal {
        Some(name) => {
            vars.iter()
                .find(|(_, n)| n == name)
                .map(|(i, _)| i.clone())
                .ok_or_else(|| {
                    let known: Vec<&String> = vars.iter().map(|(_, n)| n).collect();
                    SpecError::Invalid(format!(
                        "{ctx}: VCD has no signal named '{name}' (have: {known:?})"
                    ))
                })?
        }
        None => {
            if vars.len() != 1 {
                let known: Vec<&String> = vars.iter().map(|(_, n)| n).collect();
                return Err(SpecError::Invalid(format!(
                    "{ctx}: VCD has {} signals ({known:?}); the channel needs a `signal` \
                     field to pick one",
                    vars.len()
                )));
            }
            vars[0].0.clone()
        }
    };

    // Body: #time markers and scalar value changes for our id.
    let mut out: Vec<(f64, f64)> = Vec::new();
    let mut t = 0.0_f64;
    let mut last_marker = 0.0_f64;
    let mut in_body = false;
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with("$enddefinitions") {
            in_body = true;
            continue;
        }
        if !in_body || l.is_empty() || l.starts_with('$') {
            continue;
        }
        if let Some(stamp) = l.strip_prefix('#') {
            if let Ok(ticks) = stamp.parse::<f64>() {
                t = ticks * ts;
                last_marker = last_marker.max(t);
            }
            continue;
        }
        // Scalar change: '0<id>' / '1<id>' (x/z are skipped — undefined levels
        // carry no honest edge information).
        if l.len() > 1 {
            let (val, rest) = l.split_at(1);
            if rest == id {
                match val {
                    "0" => out.push((t, 0.0)),
                    "1" => out.push((t, 1.0)),
                    _ => {}
                }
            }
        }
    }
    if out.len() < 2 {
        return Err(SpecError::Invalid(format!(
            "{ctx}: VCD signal has {} value changes — not enough to extract any feature",
            out.len()
        )));
    }
    // The capture observed the signal (holding its last value) up to the final
    // time marker; record that endpoint so the observation window — which the
    // edge_count comparison depends on — is the capture's, not the last edge's.
    if let Some(&(t_last, v_last)) = out.last() {
        if last_marker > t_last {
            out.push((last_marker, v_last));
        }
    }
    Ok(out)
}

/// Parse a VCD timescale token ("1us", "10ns", "1ms") into seconds-per-tick.
fn parse_timescale(tok: &str) -> Option<f64> {
    let digits: String = tok.chars().take_while(|c| c.is_ascii_digit()).collect();
    let unit: String = tok.chars().skip_while(|c| c.is_ascii_digit()).collect();
    let n: f64 = digits.parse().ok()?;
    let mult = match unit.trim() {
        "s" => 1.0,
        "ms" => 1e-3,
        "us" => 1e-6,
        "ns" => 1e-9,
        "ps" => 1e-12,
        "fs" => 1e-15,
        _ => return None,
    };
    Some(n * mult)
}

// ── feature extraction ────────────────────────────────────────────────────────

/// One detected edge: time and direction.
#[derive(Debug, Clone, Copy)]
struct Edge {
    t: f64,
    rising: bool,
}

/// Detect threshold crossings with hysteresis (±5% of swing around the
/// threshold), the way a scope's measure menu does. Without hysteresis, noise
/// riding on a real capture near the threshold would manufacture edges and
/// fake a period/duty disagreement — or worse, fake an agreement.
fn detect_edges(series: &[(f64, f64)], threshold: Option<f64>) -> Vec<Edge> {
    let (min, max) = extremes(series);
    let swing = max - min;
    if swing <= 1e-9 {
        return Vec::new(); // a flat line has no edges
    }
    let thr = threshold.unwrap_or(min + 0.5 * swing);
    let hi = thr + 0.05 * swing;
    let lo = thr - 0.05 * swing;

    let mut edges = Vec::new();
    let mut state_high = series[0].1 > thr;
    for &(t, v) in series {
        if !state_high && v >= hi {
            state_high = true;
            edges.push(Edge { t, rising: true });
        } else if state_high && v <= lo {
            state_high = false;
            edges.push(Edge { t, rising: false });
        }
    }
    edges
}

fn extremes(series: &[(f64, f64)]) -> (f64, f64) {
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    for &(_, v) in series {
        min = min.min(v);
        max = max.max(v);
    }
    (min, max)
}

/// Extract one feature's value from a waveform. Returns a named reason when
/// the waveform cannot honestly yield the feature (too few edges, empty
/// window) — the comparison surfaces that reason as a failure, never a skip.
pub fn extract(series: &[(f64, f64)], f: &Feature) -> Result<f64, String> {
    let start_s = f.after_ms / 1000.0;
    let win: Vec<(f64, f64)> = series
        .iter()
        .copied()
        .filter(|&(t, _)| t + 1e-12 >= start_s)
        .collect();
    if win.is_empty() {
        return Err(format!("no samples after {} ms", f.after_ms));
    }

    match f.kind.as_str() {
        "min" => Ok(extremes(&win).0),
        "max" => Ok(extremes(&win).1),
        "level" => {
            // Time-weighted (trapezoidal) mean, so a burst of dense samples
            // cannot bias the level the way a plain average would.
            if win.len() == 1 {
                return Ok(win[0].1);
            }
            let mut area = 0.0;
            for w in win.windows(2) {
                area += 0.5 * (w[0].1 + w[1].1) * (w[1].0 - w[0].0);
            }
            let span = win[win.len() - 1].0 - win[0].0;
            if span <= 0.0 {
                return Err("window has zero time span".to_string());
            }
            Ok(area / span)
        }
        "period" => {
            let rising: Vec<f64> = detect_edges(&win, f.threshold)
                .iter()
                .filter(|e| e.rising)
                .map(|e| e.t)
                .collect();
            if rising.len() < 2 {
                return Err(format!(
                    "only {} rising edge(s) in the window — need >= 2 for a period",
                    rising.len()
                ));
            }
            let span = rising[rising.len() - 1] - rising[0];
            Ok(span / (rising.len() - 1) as f64 * 1000.0) // ms
        }
        "pulse_width" => {
            let edges = detect_edges(&win, f.threshold);
            let mut widths = Vec::new();
            for w in edges.windows(2) {
                if w[0].rising && !w[1].rising {
                    widths.push(w[1].t - w[0].t);
                }
            }
            if widths.is_empty() {
                return Err("no complete high pulse (rising→falling) in the window".to_string());
            }
            Ok(widths.iter().sum::<f64>() / widths.len() as f64 * 1000.0) // ms
        }
        "duty" => {
            // High time over complete periods (rising edge to next rising edge),
            // so a truncated final half-cycle cannot skew the ratio.
            let edges = detect_edges(&win, f.threshold);
            let mut high = 0.0;
            let mut total = 0.0;
            let mut i = 0;
            while i + 1 < edges.len() {
                if !edges[i].rising {
                    i += 1;
                    continue;
                }
                // Find the next rising edge (end of this period).
                let Some(next_rise) = edges[i + 1..].iter().position(|e| e.rising) else {
                    break;
                };
                let end = edges[i + 1 + next_rise];
                let fall = edges[i + 1..i + 1 + next_rise + 1]
                    .iter()
                    .find(|e| !e.rising);
                if let Some(fall) = fall {
                    high += fall.t - edges[i].t;
                    total += end.t - edges[i].t;
                }
                i += 1 + next_rise;
            }
            if total <= 0.0 {
                return Err("no complete period in the window — cannot compute duty".to_string());
            }
            Ok(high / total)
        }
        "edge_count" => Ok(detect_edges(&win, f.threshold).len() as f64),
        other => Err(format!("unknown feature kind '{other}'")),
    }
}

// ── comparison ────────────────────────────────────────────────────────────────

/// One feature's captured-vs-simulated comparison, with both values and the
/// tolerance band that judged them — the report line an EE can argue with.
#[derive(Debug, Clone)]
pub struct FeatureResult {
    /// The net (probe point).
    pub net: String,
    /// The feature kind.
    pub kind: String,
    /// The feature's unit ("V", "ms", "frac", "edges").
    pub unit: &'static str,
    pub pass: bool,
    /// The full one-line verdict: sim vs captured, delta, tolerance.
    pub detail: String,
}

/// Compare one feature across the captured and simulated waveforms. The
/// tolerance band is `max(abstol, reltol * |captured|)` of whichever are
/// stated — the captured value is the oracle, so the relative band is
/// anchored on it.
pub fn compare(
    net: &str,
    f: &Feature,
    captured: &[(f64, f64)],
    simulated: &[(f64, f64)],
) -> FeatureResult {
    let unit = f.unit();
    let fail = |detail: String| FeatureResult {
        net: net.to_string(),
        kind: f.kind.clone(),
        unit,
        pass: false,
        detail,
    };

    // edge_count is only comparable over matching observation windows: five
    // edges in 100 ms and five edges in 1000 ms are different waveforms. This
    // is the trigger-alignment trap made explicit rather than absorbed.
    if f.kind == "edge_count" {
        let span = |s: &[(f64, f64)]| s.last().map(|l| l.0 - s[0].0).unwrap_or(0.0);
        let (cs, ss) = (span(captured), span(simulated));
        if cs > 0.0 && (cs - ss).abs() / cs > 0.10 {
            return fail(format!(
                "{net} edge_count: window mismatch — capture spans {:.1} ms but the sim \
                 spans {:.1} ms; edge counts over different windows are not comparable \
                 (match `duration_ms` to the capture, or trim the capture)",
                cs * 1e3,
                ss * 1e3
            ));
        }
    }

    let cap = match extract(captured, f) {
        Ok(v) => v,
        Err(e) => return fail(format!("{net} {}: captured trace: {e}", f.kind)),
    };
    let sim = match extract(simulated, f) {
        Ok(v) => v,
        Err(e) => return fail(format!("{net} {}: simulated waveform: {e}", f.kind)),
    };

    let mut band = 0.0_f64;
    if let Some(a) = f.abstol {
        band = band.max(a);
    }
    if let Some(r) = f.reltol {
        band = band.max(r * cap.abs());
    }
    let delta = sim - cap;
    let pass = delta.abs() <= band + 1e-12;
    let verdict = if pass { "within" } else { "EXCEEDS" };
    FeatureResult {
        net: net.to_string(),
        kind: f.kind.clone(),
        unit,
        pass,
        detail: format!(
            "{net} {}: sim {:.4} {unit} vs captured {:.4} {unit} (Δ {:+.4} {unit} {verdict} ±{:.4} {unit})",
            f.kind, sim, cap, delta, band
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A clean square wave: `period_s` period, `duty` high fraction, levels
    /// lo/hi, sampled at `dt`, starting low at t=0.
    fn square(period_s: f64, duty: f64, lo: f64, hi: f64, total_s: f64, dt: f64) -> Vec<(f64, f64)> {
        let mut out = Vec::new();
        let mut t = 0.0;
        while t <= total_s {
            let phase = (t / period_s).fract();
            // Low first, then high: rising edge at (1-duty) into the period.
            let v = if phase >= 1.0 - duty { hi } else { lo };
            out.push((t, v));
            t += dt;
        }
        out
    }

    fn feat(kind: &str) -> Feature {
        Feature {
            kind: kind.to_string(),
            after_ms: 0.0,
            threshold: None,
            abstol: Some(1.0),
            reltol: None,
        }
    }

    #[test]
    fn extracts_square_wave_features() {
        // 200 ms period, 50% duty, 0..5 V, 1 s at 0.5 ms — the blinky shape.
        let s = square(0.2, 0.5, 0.0, 5.0, 1.0, 0.0005);
        let period = extract(&s, &feat("period")).unwrap();
        assert!((period - 200.0).abs() < 2.0, "period {period}");
        let duty = extract(&s, &feat("duty")).unwrap();
        assert!((duty - 0.5).abs() < 0.02, "duty {duty}");
        let pw = extract(&s, &feat("pulse_width")).unwrap();
        assert!((pw - 100.0).abs() < 2.0, "pulse_width {pw}");
        let max = extract(&s, &feat("max")).unwrap();
        assert!((max - 5.0).abs() < 1e-9);
        let min = extract(&s, &feat("min")).unwrap();
        assert!(min.abs() < 1e-9);
        let level = extract(&s, &feat("level")).unwrap();
        assert!((level - 2.5).abs() < 0.1, "level {level}");
        // 1 s / 100 ms half-period ≈ 10 edges (9..10 depending on truncation).
        let edges = extract(&s, &feat("edge_count")).unwrap();
        assert!((9.0..=11.0).contains(&edges), "edges {edges}");
    }

    #[test]
    fn noise_near_threshold_does_not_manufacture_edges() {
        // A 2.5 V flat line with ±0.1 V "noise" around it: without hysteresis
        // scaled to the swing this would rack up hundreds of fake edges; with
        // it, the swing IS the noise so crossings are genuine — but a real
        // square wave with small noise must still count only the real edges.
        let mut s = square(0.2, 0.5, 0.0, 5.0, 1.0, 0.0005);
        for (i, p) in s.iter_mut().enumerate() {
            // Deterministic pseudo-noise, ±60 mV.
            p.1 += 0.06 * ((i as f64 * 0.7).sin());
        }
        let edges = extract(&s, &feat("edge_count")).unwrap();
        assert!((9.0..=11.0).contains(&edges), "noisy edges {edges}");
    }

    #[test]
    fn too_few_edges_is_a_named_refusal_not_a_zero() {
        let s: Vec<(f64, f64)> = (0..100).map(|i| (i as f64 * 1e-3, 5.0)).collect();
        let err = extract(&s, &feat("period")).unwrap_err();
        assert!(err.contains("rising edge"), "got: {err}");
    }

    #[test]
    fn mismatch_names_feature_and_both_values() {
        let cap = square(0.3, 0.5, 0.0, 5.0, 1.2, 0.0005); // 300 ms period
        let sim = square(0.2, 0.5, 0.0, 5.0, 1.2, 0.0005); // 200 ms period
        let f = Feature {
            kind: "period".into(),
            after_ms: 0.0,
            threshold: None,
            abstol: None,
            reltol: Some(0.05),
        };
        let r = compare("D13", &f, &cap, &sim);
        assert!(!r.pass);
        assert!(r.detail.contains("period"), "{}", r.detail);
        assert!(r.detail.contains("200"), "sim value missing: {}", r.detail);
        assert!(r.detail.contains("300"), "captured value missing: {}", r.detail);
        assert!(r.detail.contains("EXCEEDS"), "{}", r.detail);
    }

    #[test]
    fn edge_count_refuses_window_mismatch() {
        let cap = square(0.2, 0.5, 0.0, 5.0, 0.4, 0.0005); // 400 ms capture
        let sim = square(0.2, 0.5, 0.0, 5.0, 1.0, 0.0005); // 1 s sim
        let r = compare("D13", &feat("edge_count"), &cap, &sim);
        assert!(!r.pass);
        assert!(r.detail.contains("window mismatch"), "{}", r.detail);
    }
}
