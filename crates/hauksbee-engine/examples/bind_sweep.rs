//! "Any PCB" sweep: run every board in the corpus through extract -> bind
//! and tabulate what hauksbee makes of boards it has never seen.
//!
//!   cargo run --release -p hauksbee-engine --example bind_sweep [corpus_dir]

use hauksbee_engine::bind_board;
use hauksbee_extract::ExtractedBoard;
use hauksbee_models::{Confidence, ModelLibrary};
use std::path::{Path, PathBuf};

fn main() {
    let corpus = std::env::args().nth(1).map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../board-corpus")
    });
    let mut boards: Vec<PathBuf> = Vec::new();
    collect(&corpus, &mut boards);
    boards.sort();

    let lib = ModelLibrary::builtin();
    println!(
        "{:<42} {:>6} {:>6} {:>7} {:>6} {:>5} {:>5} {:>7}",
        "board", "comps", "nets", "resolv%", "analog", "digi", "mcu", "ms"
    );
    let mut ok = 0usize;
    let mut failed = 0usize;
    for path in &boards {
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .chars()
            .take(40)
            .collect::<String>();
        let Ok(text) = std::fs::read_to_string(path) else {
            println!("{name:<42} unreadable");
            failed += 1;
            continue;
        };
        let t0 = std::time::Instant::now();
        let board = match ExtractedBoard::from_auto(&text) {
            Ok(b) => b,
            Err(e) => {
                println!("{name:<42} extract failed: {e}");
                failed += 1;
                continue;
            }
        };
        let bound = bind_board(&board, &lib);
        let dt = t0.elapsed().as_millis();

        let mut resolved = 0usize;
        let mut considered = 0usize;
        let mut analog = 0usize;
        let mut digi = 0usize;
        for row in &bound.report.rows {
            let kind = bound
                .component_kinds
                .get(&row.reference)
                .map(String::as_str)
                .unwrap_or("");
            if kind == "ignore" {
                continue;
            }
            considered += 1;
            if !matches!(row.confidence, Confidence::Unresolved) {
                resolved += 1;
            }
            match kind {
                "passive" | "diode" | "bjtnpn" | "bjtpnp" | "nmos" | "pmos"
                | "analogswitch" | "opamp" | "comparator" | "vreg" => analog += 1,
                "digital" | "shiftregister" | "dac" | "adc" => digi += 1,
                _ => {}
            }
        }
        let pct = if considered > 0 {
            100.0 * resolved as f64 / considered as f64
        } else {
            0.0
        };
        println!(
            "{:<42} {:>6} {:>6} {:>6.1}% {:>6} {:>5} {:>5} {:>7}",
            name,
            board.components.len(),
            board.nets.len(),
            pct,
            analog,
            digi,
            bound.mcus.len(),
            dt
        );
        ok += 1;
    }
    println!("\n{ok} boards bound, {failed} failed");
    if failed > 0 {
        std::process::exit(1);
    }
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // Skip the upstream-corrupt demo.
            if path.to_str().is_some_and(|p| p.contains("royalblue54L_feather")) {
                continue;
            }
            collect(&path, out);
        } else if matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("kicad_pcb" | "brd")
        ) {
            out.push(path);
        }
    }
}
