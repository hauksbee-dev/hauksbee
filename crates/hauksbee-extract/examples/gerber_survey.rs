//! Dev survey: reverse-extract an already-exported gerber dir and compare to a
//! native KiCad board. Prints the agreement line. Usage:
//!   cargo run --example gerber_survey -- <native.kicad_pcb> <gerber_dir>

use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use hauksbee_extract::gerber::from_gerber_dir;
use hauksbee_extract::ExtractedBoard;

fn pad_key(x: f64, y: f64) -> (i64, i64) {
    ((x * 10.0).round() as i64, (y * 10.0).round() as i64)
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let native = ExtractedBoard::from_kicad_pcb(&std::fs::read_to_string(&a[1]).unwrap()).unwrap();
    let t = Instant::now();
    let recon = from_gerber_dir(Path::new(&a[2])).unwrap();
    let dt = t.elapsed();

    let mut nn: HashMap<(i64, i64), i64> = HashMap::new();
    let mut npads = 0;
    for c in &native.components {
        for p in &c.pins {
            if let Some((x, y)) = p.position {
                npads += 1;
                if let Some(net) = p.net {
                    nn.insert(pad_key(x, -y), net);
                }
            }
        }
    }
    let mut rn: HashMap<(i64, i64), i64> = HashMap::new();
    for c in &recon.board.components {
        for p in &c.pins {
            if let (Some((x, y)), Some(net)) = (p.position, p.net) {
                rn.insert(pad_key(x, y), net);
            }
        }
    }
    let shared: Vec<(i64, i64)> = nn.keys().filter(|k| rn.contains_key(*k)).copied().collect();
    let (mut pairs, mut agree) = (0u64, 0u64);
    for i in 0..shared.len() {
        for j in (i + 1)..shared.len() {
            let sn = nn[&shared[i]] == nn[&shared[j]];
            let sr = rn[&shared[i]] == rn[&shared[j]];
            pairs += 1;
            if sn == sr {
                agree += 1;
            }
        }
    }
    let recon_by_ref: HashMap<&str, usize> = recon
        .board
        .components
        .iter()
        .map(|c| (c.reference.as_str(), c.pins.len()))
        .collect();
    let mut cmatch = 0;
    for c in &native.components {
        let n = c.pins.iter().filter(|p| p.position.is_some()).count();
        if let Some(&r) = recon_by_ref.get(c.reference.as_str()) {
            if n > 0 && r > 0 && (n as i64 - r as i64).abs() <= 1 {
                cmatch += 1;
            }
        }
    }
    println!(
        "{:<22} native {:>4}c/{:>4}n  recon {:>4}c/{:>4}n | comp {:>3}/{:<3} | pads {:>4}/{:<4} | nets {:>5.1}% | {:?}",
        Path::new(&a[2]).file_name().unwrap().to_string_lossy(),
        native.components.len(),
        native.nets.len(),
        recon.board.components.len(),
        recon.board.nets.len(),
        cmatch,
        native.components.len(),
        shared.len(),
        npads,
        100.0 * agree as f64 / pairs.max(1) as f64,
        dt,
    );
}
