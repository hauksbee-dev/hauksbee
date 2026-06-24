//! Per-net copper width + IPC-2221 ampacity, reconstructed from GERBER files.
//! Usage: gerber_trace_current <gerber_dir> [max_width_mm_filter]
//!
//! Unlike `trace_current_probe` (which reads KiCad `(segment)` primitives), this
//! sources the per-net copper geometry from the reverse-extracted gerber
//! primitives: a drawn track is a finite-width capsule whose width is exact from
//! the manufacturing files, and a pour is reported `Poured` (out of the
//! discrete-width check's reach, exactly as on native CAD). This is the
//! re-runnable evidence behind the gerber trace-current sweep (FAMOUS_SWEEP.md
//! Round 5): the narrowest drawn track on each net and its current rating, with
//! the poured planes honestly skipped.

use std::path::Path;

use hauksbee_extract::gerber::connect::GerberCopperKind;
use hauksbee_extract::gerber::from_gerber_dir;
use hauksbee_extract::ipc2221_ampacity;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut a = std::env::args().skip(1);
    let dir = a.next().expect("gerber dir");
    let wfilter: Option<f64> = a.next().and_then(|s| s.parse().ok());

    let g = from_gerber_dir(Path::new(&dir))?;
    println!("board: {}", g.board.name);
    println!(
        "nets: {}  layers: {}  components: {}",
        g.stats.n_nets, g.stats.n_layers, g.stats.n_components
    );
    println!(
        "{:<10} {:8} {:>7} {:>8} {:>8} {:>8} {:>10}",
        "net", "kind", "#track", "#region", "min_mm", "max_mm", "amp@10C"
    );

    // Sort narrowest-track first so the bottleneck candidates surface at the top.
    let mut rows: Vec<_> = g.stats.net_copper.iter().collect();
    rows.sort_by(|x, y| {
        let kx = x.min_track_width_mm.unwrap_or(f64::INFINITY);
        let ky = y.min_track_width_mm.unwrap_or(f64::INFINITY);
        kx.partial_cmp(&ky).unwrap()
    });

    for nc in rows {
        if let Some(wf) = wfilter {
            if nc.min_track_width_mm.map(|w| w > wf).unwrap_or(true) {
                continue;
            }
        }
        let kind = match nc.kind {
            GerberCopperKind::Traces => "Traces",
            GerberCopperKind::Poured => "Poured",
            GerberCopperKind::None => "None",
        };
        let amp = nc
            .min_track_width_mm
            .map(|w| ipc2221_ampacity(w, 1.0, 10.0, true))
            .unwrap_or(0.0);
        println!(
            "{:<10} {:8} {:>7} {:>8} {:>8} {:>8} {:>9.2}A",
            nc.name,
            kind,
            nc.track_count,
            nc.region_count,
            nc.min_track_width_mm
                .map(|w| format!("{w:.3}"))
                .unwrap_or_else(|| "-".into()),
            nc.max_track_width_mm
                .map(|w| format!("{w:.3}"))
                .unwrap_or_else(|| "-".into()),
            amp
        );
    }
    Ok(())
}
