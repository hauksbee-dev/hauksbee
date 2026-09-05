//! Per-net copper geometry + IPC-2221 ampacity probe.
//! Usage: trace_current_probe <board.kicad_pcb> [min_seg_width_mm_filter]
//!
//! Dumps, for every net with copper, whether it is routed as discrete Traces or
//! a Poured plane, its narrowest discrete segment width, and that width's
//! IPC-2221 external-1oz-10C current rating. This is the re-runnable evidence
//! behind the trace-current sweep: it shows, with the data, why the high-current
//! rails on the corpus are Poured (and so out of the discrete-width check's
//! reach) rather than asserting it.

use hauksbee_extract::{ipc2221_ampacity, net_copper_from_root, CopperKind};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut a = std::env::args().skip(1);
    let path = a.next().expect("board .kicad_pcb file");
    let wfilter: Option<f64> = a.next().and_then(|s| s.parse().ok());
    let text = std::fs::read_to_string(&path)?;
    let doc = forge_sexpr::parse(&text)?;
    let root = doc.root().ok_or("no root")?;
    let copper = net_copper_from_root(root);

    println!(
        "{:<28} {:8} {:>6} {:>6} {:>8} {:>10} {:>10}",
        "net", "kind", "#seg", "#zone", "min_mm", "max_mm", "amp@10C"
    );
    for nc in &copper {
        if let Some(wf) = wfilter {
            if nc.min_trace_width_mm.map(|w| w > wf).unwrap_or(true) {
                continue;
            }
        }
        let kind = match nc.kind {
            CopperKind::Traces => "Traces",
            CopperKind::Poured => "Poured",
            CopperKind::None => "None",
        };
        let amp = nc
            .min_trace_width_mm
            .map(|w| ipc2221_ampacity(w, 1.0, 10.0, true))
            .unwrap_or(0.0);
        println!(
            "{:<28} {:8} {:>6} {:>6} {:>8} {:>10} {:>9.2}A",
            truncate(&nc.name, 28),
            kind,
            nc.segment_count,
            nc.zone_count,
            nc.min_trace_width_mm
                .map(|w| format!("{w:.3}"))
                .unwrap_or_else(|| "-".into()),
            nc.max_trace_width_mm
                .map(|w| format!("{w:.3}"))
                .unwrap_or_else(|| "-".into()),
            amp
        );
    }
    Ok(())
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n - 1])
    }
}
