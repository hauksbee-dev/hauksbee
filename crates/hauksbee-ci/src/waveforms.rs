//! `hauksbee-ci run --waveforms DIR`: the sampled time series behind each
//! assertion, so a reader can plot what happened instead of reading only the
//! window's min and max.
//!
//! The runner already keeps per-frame `(t_s, volts)` series for hwtrace and
//! edge-timed nets (`RunOutcome::net_series`) and per-scenario rail windows
//! (`RunOutcome::rail_windows`); with `Spec::record_waveforms` set it also
//! records every net an assertion names. This module only writes them out, in
//! the `time_s,<net>` CSV shape `hauksbee run --probe-csv` uses.
//!
//! Layout under `DIR/<spec-stem>/`: `seed<N>/net_<net>.csv` (the whole run)
//! and `seed<N>/a<index>_<net>.csv` (the assertion's own window, with the
//! scenario scope for a scoped `rail_window`), plus `index.json` mapping each
//! file to its net, assertion and window. Samples are the settled per-frame
//! values: an intra-frame extreme folded into a window's min/max has no
//! timestamp and is not in the series. A series longer than [`MAX_POINTS`] is
//! min/max-bucket decimated, which keeps every bucket's extremes.

use std::path::Path;

use crate::runner::RunOutcome;
use crate::spec::Spec;

/// Upper bound on points per file after decimation.
pub const MAX_POINTS: usize = 20_000;

/// Keep each bucket's lowest and highest sample, in time order, so a dip or
/// spike the assertion judged is still visible after decimation.
#[must_use]
pub fn decimate(series: &[(f64, f64)]) -> Vec<(f64, f64)> {
    if series.len() <= MAX_POINTS {
        return series.to_vec();
    }
    let per = series.len().div_ceil(MAX_POINTS / 2);
    let mut out = Vec::with_capacity(MAX_POINTS);
    for bucket in series.chunks(per) {
        let lo = bucket.iter().min_by(|a, b| a.1.total_cmp(&b.1));
        let hi = bucket.iter().max_by(|a, b| a.1.total_cmp(&b.1));
        if let (Some(&lo), Some(&hi)) = (lo, hi) {
            out.extend(if lo.0 <= hi.0 { [lo, hi] } else { [hi, lo] });
        }
    }
    out.dedup_by(|a, b| a.0 == b.0);
    out
}

fn file_stem(net: &str) -> String {
    net.chars()
        .map(|c| match c {
            '+' => 'P',
            c if c.is_ascii_alphanumeric() || c == '-' || c == '.' => c,
            _ => '_',
        })
        .collect()
}

fn write_csv(path: &Path, net: &str, series: &[(f64, f64)]) -> std::io::Result<usize> {
    let rows = decimate(series);
    let mut csv = format!(
        "time_s,{}\n",
        hauksbee_engine::commands::sim::csv_escape(net)
    );
    for (t, v) in &rows {
        csv.push_str(&format!("{t:.9},{v}\n"));
    }
    std::fs::write(path, csv)?;
    Ok(rows.len())
}

/// Write every seed's series for `spec` under `dir/<spec-stem>/`.
pub fn export(
    dir: &Path,
    spec_path: &Path,
    spec: &Spec,
    outcomes: &[RunOutcome],
) -> std::io::Result<()> {
    let stem = spec_path
        .file_stem()
        .map_or_else(|| "spec".into(), |s| s.to_string_lossy());
    let root = dir.join(stem.as_ref());
    let mut index = Vec::new();
    for out in outcomes {
        let seed_dir = root.join(format!("seed{}", out.seed));
        std::fs::create_dir_all(&seed_dir)?;
        let mut nets: Vec<_> = out.net_series.iter().collect();
        nets.sort_by(|a, b| a.0.cmp(b.0));
        for (net, series) in nets {
            let file = format!("seed{}/net_{}.csv", out.seed, file_stem(net));
            let points = write_csv(&root.join(&file), net, series)?;
            index.push(
                serde_json::json!({"seed": out.seed, "net": net, "file": file,
                "points": points, "samples": series.len()}),
            );
        }
        for (i, a) in spec.asserts.iter().enumerate() {
            let Some(net) = a.net.as_deref() else {
                continue;
            };
            let series: Vec<(f64, f64)> = if a.kind == "rail_window" {
                let scope = (a.scenario.clone().unwrap_or_default(), net.to_string());
                out.rail_windows
                    .get(&scope)
                    .map(|w| w.samples.clone())
                    .unwrap_or_default()
            } else if let Some((ws, we)) = crate::assertions::analog_eval_window(a, out) {
                let all = out.net_series.get(net).map_or(&[][..], Vec::as_slice);
                all.iter()
                    .copied()
                    .filter(|&(t, _)| t >= ws && t <= we)
                    .collect()
            } else {
                continue;
            };
            let file = format!("seed{}/a{i}_{}.csv", out.seed, file_stem(net));
            let points = write_csv(&root.join(&file), net, &series)?;
            let window = series.first().zip(series.last()).map(|(a, b)| [a.0, b.0]);
            index.push(
                serde_json::json!({"seed": out.seed, "net": net, "file": file,
                "points": points, "samples": series.len(), "assert": i, "kind": a.kind,
                "name": a.name, "scenario": a.scenario, "window_s": window}),
            );
        }
    }
    let doc = serde_json::json!({"spec": spec.name, "files": index});
    std::fs::write(root.join("index.json"), format!("{doc:#}\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimation_bounds_points_and_keeps_extremes() {
        let n = MAX_POINTS * 5;
        let mut s: Vec<(f64, f64)> = (0..n).map(|i| (i as f64 * 1e-6, 1.0)).collect();
        s[12_345].1 = -3.0;
        s[77_777].1 = 9.0;
        let d = decimate(&s);
        assert!(d.len() <= MAX_POINTS, "{} points", d.len());
        assert!(d.windows(2).all(|w| w[0].0 < w[1].0), "time stays ordered");
        assert!(
            d.contains(&s[12_345]) && d.contains(&s[77_777]),
            "the dip and the spike survive"
        );
        assert_eq!(
            decimate(&s[..10]),
            s[..10].to_vec(),
            "short series pass through"
        );
    }

    #[test]
    fn file_stems_are_filesystem_safe() {
        assert_eq!(file_stem("+3V3"), "P3V3");
        assert_eq!(file_stem("/SHEET/NET A"), "_SHEET_NET_A");
    }
}
