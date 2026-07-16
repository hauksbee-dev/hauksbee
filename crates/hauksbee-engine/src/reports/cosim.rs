//! The headless co-sim path's leaf machinery: driving the scheduler for a fixed
//! wall of simulated time (`run_headless`), the structured co-sim summary
//! (`build_cosim_json`), and the `--probe` net-name validation helpers. `cmd_run`
//! orchestrates these; the boot advisory it layers on top lives in
//! `crate::checks::boot`.

use std::path::Path;

use crate::engine::HauksbeeEngine;
use crate::result::{CosimFailedWindow, CosimJson, NetActivity};

/// Truncate to at most `max` chars (byte-for-byte the binary's helper).
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

/// Build the machine-readable co-sim summary (Track B) from a finished run.
/// Returns `None` when no MCU core ran (no co-sim happened, so there is nothing
/// to summarise). Reads the scheduler's per-net stats for the total toggle count
/// and the top-N most-active nets, and the MCU binding identities for the
/// requested part / backend / substitution flag.
pub fn build_cosim_json(engine: &HauksbeeEngine, uart_seen: bool) -> Option<CosimJson> {
    let sched = engine.scheduler();
    let identities = sched.mcu_identities();
    // No live MCU => no co-sim ran (e.g. a renode/qemu board with no firmware).
    let (mcu_ref, backend, requested_part) = identities.into_iter().next()?;
    // A substitution is recorded against this reference iff its requested part
    // was collapsed onto a less-specific modelled core.
    let substituted = sched
        .substitutions()
        .iter()
        .any(|s| s.reference == mcu_ref);

    let total_toggles: u64 = sched.stats.values().map(|s| s.toggles).sum();

    // Top-N nets by activity (toggles, then voltage range), mirroring the text
    // table's ordering so JSON and text agree on "most active".
    let mut rows: Vec<_> = sched.stats.iter().collect();
    rows.sort_by(|a, b| {
        b.1.toggles
            .cmp(&a.1.toggles)
            .then(
                (b.1.max_v - b.1.min_v)
                    .partial_cmp(&(a.1.max_v - a.1.min_v))
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
            // Total-order tiebreak on net name: without it two nets with equal
            // toggles and equal voltage range order by HashMap iteration, so the
            // JSON activity summary was nondeterministic across runs.
            .then_with(|| a.0.cmp(b.0))
    });
    let activity_summary: Vec<NetActivity> = rows
        .iter()
        .take(10)
        .filter(|(_, st)| st.toggles > 0)
        .map(|(name, st)| NetActivity {
            net: (*name).clone(),
            toggles: st.toggles,
            v_min: if st.min_v.is_finite() { st.min_v } else { 0.0 },
            v_max: if st.max_v.is_finite() { st.max_v } else { 0.0 },
        })
        .collect();

    // Analog-fidelity honesty (05 §3b): a run that held stale voltages over one
    // or more non-convergent chunks is not a faithful analog result. Surface the
    // exact windows so a consumer sees which span cannot be trusted rather than
    // reading the quiet-held voltages as real.
    let analog_valid = sched.analog_valid();
    let failed_windows: Vec<CosimFailedWindow> = sched
        .failed_windows()
        .iter()
        .map(|&(start_s, end_s)| CosimFailedWindow { start_s, end_s })
        .collect();

    Some(CosimJson {
        mcu_ref,
        backend,
        requested_part,
        substituted,
        total_toggles,
        uart_seen,
        activity_summary,
        analog_valid,
        failed_windows,
        spi_framing: sched
            .spi_framing_modes()
            .into_iter()
            .map(|(bus, mode)| crate::result::CosimSpiFraming {
                bus,
                mode: mode.as_str().to_string(),
            })
            .collect(),
    })
}

/// Trim, drop empties, and de-duplicate probe net names while preserving the
/// order the user gave (which becomes the CSV column order).
pub fn dedup_probes(raw: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for n in raw {
        let n = n.trim();
        if !n.is_empty() && !out.iter().any(|e| e == n) {
            out.push(n.to_string());
        }
    }
    out
}

/// Validate `--probe` preconditions before a headless run: a `--probe-csv` sink
/// is required, and every probed net must exist on the board. An unknown net
/// fails loudly with near-matches, the same did-you-mean style the spec loader
/// uses for a bad net name.
pub fn validate_probes(
    probes: &[String],
    csv: Option<&Path>,
    known: &[String],
) -> anyhow::Result<()> {
    if probes.is_empty() {
        return Ok(());
    }
    if csv.is_none() {
        anyhow::bail!("--probe needs --probe-csv <path> to write the waveforms to");
    }
    let known_set: std::collections::HashSet<&str> = known.iter().map(String::as_str).collect();
    for net in probes {
        if !known_set.contains(net.as_str()) {
            let near = nearest_nets(net, known, 5);
            let hint = if near.is_empty() {
                String::new()
            } else {
                format!(" - did you mean: {}?", near.join(", "))
            };
            anyhow::bail!("--probe: net '{net}' not found on the board{hint}");
        }
    }
    Ok(())
}

/// Up to `limit` known net names closest to `target` by edit distance, favouring
/// substring matches. A compact twin of the spec loader's net suggester, kept
/// local because the engine binary cannot depend on the CI crate.
pub fn nearest_nets(target: &str, known: &[String], limit: usize) -> Vec<String> {
    let t = target.to_ascii_lowercase();
    let mut scored: Vec<(usize, &String)> = known
        .iter()
        .map(|name| {
            let n = name.to_ascii_lowercase();
            let contains = n.contains(&t) || t.contains(&n);
            let dist = levenshtein(&t, &n);
            let score = if contains { dist.saturating_sub(3) } else { dist };
            (score, name)
        })
        .collect();
    scored.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(b.1)));
    let cutoff = (target.len() / 2).max(3);
    scored
        .into_iter()
        .filter(|(score, _)| *score <= cutoff)
        .take(limit)
        .map(|(_, name)| name.clone())
        .collect()
}

/// Classic Levenshtein edit distance.
pub fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (n, m) = (a.len(), b.len());
    if n == 0 {
        return m;
    }
    if m == 0 {
        return n;
    }
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut cur = vec![0usize; m + 1];
    for i in 1..=n {
        cur[0] = i;
        for j in 1..=m {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[m]
}

pub fn run_headless(
    engine: &mut HauksbeeEngine,
    seconds: f64,
    uart_seen: &mut bool,
    quiet: bool,
    strict: bool,
    probes: &[String],
    probe_csv: Option<&Path>,
) -> anyhow::Result<Vec<crate::FaultEvent>> {
    use crate::{FaultEvent, FaultKind};
    use hauksbee_server::engine::Engine;
    // External emulator backends (Renode/QEMU) advance over a socket: a fine 1 ms
    // chunk means thousands of round-trips and a co-sim that looks frozen for
    // minutes. Use a coarse 10 ms chunk for them and print progress so a slow
    // STM32 run is legible. In-process AVR stays at 1 kHz (fast, more resolution).
    let external = engine
        .scheduler()
        .mcu_identities()
        .iter()
        .any(|(_, backend, _)| backend.starts_with("renode:") || backend.starts_with("qemu:"));
    let frame_dt = if external { 10.0 / 1000.0 } else { 1.0 / 1000.0 };
    if external {
        engine.scheduler_mut().chunk_s = frame_dt;
        eprintln!(
            "co-sim: {seconds:.2}s on an external emulator (slow — roughly wall-clock \
             per simulated second; this is normal for Renode/QEMU). Progress:"
        );
    } else {
        eprintln!("co-sim: {seconds:.2}s headless...");
    }
    let mut t = 0.0;
    let mut next_progress = seconds / 5.0; // ~5 progress lines over the run
    let mut last_uart: Vec<u8> = Vec::new();
    let mut faults: Vec<FaultEvent> = Vec::new();
    // Probe recording: one (time_s, [volts per probed net]) row per chunk. The
    // net order follows the order the user gave, which becomes the CSV columns.
    let mut probe_rows: Vec<(f64, Vec<f64>)> = Vec::new();
    while t < seconds {
        // Refuse rather than fake (05 §3b): under --strict, stop as soon as the
        // analog solve has been stuck for a whole streak of chunks. Continuing
        // would burn wall time producing more held-voltage frames the strict gate
        // is about to reject anyway, so break and let the caller exit 3. Non-strict
        // runs complete so the failed windows and analog_valid:false are reported.
        if strict && engine.scheduler().analog_abort_tripped() {
            break;
        }
        if external && t >= next_progress {
            eprintln!("  ... {t:.2} / {seconds:.2}s simulated");
            next_progress += (seconds / 5.0).max(frame_dt);
        }
        let frame = engine.step(frame_dt);
        if !probes.is_empty() {
            // A probed net absent from the frame reads 0 V (e.g. a net collapsed
            // onto ground); validation already rejected genuinely unknown names.
            let volts = probes
                .iter()
                .map(|net| frame.net_voltages.get(net).copied().unwrap_or(0.0))
                .collect();
            probe_rows.push((frame.t, volts));
        }
        for bytes in frame.uart.values() {
            last_uart.extend_from_slice(bytes);
        }
        for f in frame.faults {
            faults.push(FaultEvent {
                component: f.component,
                kind: FaultKind::from_str(&f.kind),
                value: f.value,
                limit: f.limit,
                t: f.t,
                destroyed: f.destroyed,
            });
        }
        t += frame_dt;
    }

    *uart_seen = !last_uart.is_empty();
    // The activity table + UART dump are human-facing. Under `--json` (quiet) the
    // SAME data is emitted structurally via CosimJson.activity_summary, so printing
    // it here would corrupt stdout for a machine consumer. Suppress when quiet.
    if !quiet {
        let sched = engine.scheduler();
        println!(
            "\nsimulated {:.3}s over {} nets",
            sched.sim_time,
            sched.stats.len()
        );
        // Sort nets by activity (toggle count then range).
        let mut rows: Vec<_> = sched.stats.iter().collect();
        rows.sort_by(|a, b| {
            b.1.toggles
                .cmp(&a.1.toggles)
                .then(
                    (b.1.max_v - b.1.min_v)
                        .partial_cmp(&(a.1.max_v - a.1.min_v))
                        .unwrap_or(std::cmp::Ordering::Equal),
                )
                // Total-order tiebreak on net name so the text table agrees with
                // the JSON activity summary (line 54); without it equal-toggle,
                // equal-range nets order by HashMap iteration and disagree.
                .then_with(|| a.0.cmp(b.0))
        });
        println!("\nmost active nets:");
        println!(
            "┌────────────────────────────┬──────────┬──────────┬──────────┐\n\
             │ Net                        │ min (V)  │ max (V)  │ toggles  │\n\
             ├────────────────────────────┼──────────┼──────────┼──────────┤"
        );
        for (name, st) in rows.iter().take(15) {
            let min_v = if st.min_v.is_finite() { st.min_v } else { 0.0 };
            let max_v = if st.max_v.is_finite() { st.max_v } else { 0.0 };
            println!(
                "│ {:<26} │ {:>8.3} │ {:>8.3} │ {:>8} │",
                truncate(name, 26),
                min_v,
                max_v,
                st.toggles
            );
        }
        println!("└────────────────────────────┴──────────┴──────────┴──────────┘");

        // Analog-fidelity line (05 §3b): if any chunk failed to converge, say so
        // and where, so the default text mode never presents held-stale voltages
        // as a quiet, healthy run.
        let failed = sched.failed_chunk_count();
        if failed > 0 {
            println!(
                "\nanalog_valid: false ({failed} chunk(s) failed to converge); \
                 those windows held stale voltages:"
            );
            for &(start_s, end_s) in sched.failed_windows() {
                println!("  [{:.6}s .. {:.6}s)", start_s, end_s);
            }
        }

        if !last_uart.is_empty() {
            let s = String::from_utf8_lossy(&last_uart);
            println!(
                "\nUART output ({} bytes):\n{}",
                last_uart.len(),
                s.trim_end()
            );
        }
    }

    // De-duplicate faults by (component, kind), keeping the worst value, so a
    // fault that trips every chunk is reported once. Mirrors check_board_text.
    faults.sort_by(|a, b| {
        a.component
            .cmp(&b.component)
            .then(a.kind.as_str().cmp(b.kind.as_str()))
            .then(
                b.value
                    .partial_cmp(&a.value)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
    });
    faults.dedup_by(|a, b| a.component == b.component && a.kind.as_str() == b.kind.as_str());

    // Write the probe CSV: header `time_s` then one column per probed net, one
    // row per chunk. Done after the run so a slow board still streams its summary.
    if let Some(path) = probe_csv {
        let mut csv = String::from("time_s");
        for net in probes {
            csv.push(',');
            csv.push_str(net);
        }
        csv.push('\n');
        for (t, volts) in &probe_rows {
            csv.push_str(&format!("{t:.6}"));
            for v in volts {
                csv.push_str(&format!(",{v:.6}"));
            }
            csv.push('\n');
        }
        std::fs::write(path, csv)
            .map_err(|e| anyhow::anyhow!("writing probe CSV to {}: {e}", path.display()))?;
        if !quiet {
            eprintln!("wrote {} probe row(s) to {}", probe_rows.len(), path.display());
        }
    }

    Ok(faults)
}
