//! Reverse-extract a gerber job directory and print a reconstruction report.
//! Usage: cargo run --example gerber_report -- <gerber_dir>

use std::path::Path;
use std::time::Instant;

use hauksbee_extract::gerber::from_gerber_dir;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 2 {
        eprintln!("usage: gerber_report <gerber_dir>");
        std::process::exit(2);
    }
    let t = Instant::now();
    let g = match from_gerber_dir(Path::new(&a[1])) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("extract failed: {e}");
            std::process::exit(1);
        }
    };
    let dt = t.elapsed();
    let s = &g.stats;
    println!("board: {}", g.board.name);
    println!("copper layers:       {}", s.n_layers);
    println!("plated holes:        {}", s.n_holes);
    println!("nets reconstructed:  {}", s.n_nets);
    println!("components placed:    {}", s.n_components);
    println!(
        "flashes:             {} total, {} assigned to components, {} unassigned (vias/test)",
        s.total_flashes, s.assigned_flashes, s.unassigned_flashes
    );
    println!("  of which slots:    {}", s.n_slots);
    println!(
        "  on the outline:    {} (castellations / plated edge slots)",
        s.n_castellations
    );
    println!("spans refused:       {}", s.refused_span_holes);
    println!("GND net detected:    {}", s.gnd_detected);
    println!("extraction time:     {dt:?}");
    for n in &s.notes {
        println!("note: {n}");
    }

    // Bind-rate proxy: fraction of placed components that got >=1 pad with a net.
    let bound = g
        .board
        .components
        .iter()
        .filter(|c| c.pins.iter().any(|p| p.net.is_some()))
        .count();
    println!(
        "components with >=1 netted pad: {}/{} ({:.0}%)",
        bound,
        g.board.components.len(),
        100.0 * bound as f64 / g.board.components.len().max(1) as f64
    );
    let total_pads: usize = g.board.components.iter().map(|c| c.pins.len()).sum();
    let netted_pads: usize = g
        .board
        .components
        .iter()
        .flat_map(|c| &c.pins)
        .filter(|p| p.net.is_some())
        .count();
    println!("component pads: {total_pads} ({netted_pads} on a net)");

    // Largest nets (sanity: GND / power should top the list).
    let mut by_net: std::collections::HashMap<i64, usize> = std::collections::HashMap::new();
    for c in &g.board.components {
        for p in &c.pins {
            if let Some(n) = p.net {
                *by_net.entry(n).or_default() += 1;
            }
        }
    }
    let mut v: Vec<_> = by_net.into_iter().collect();
    v.sort_by_key(|(_, c)| std::cmp::Reverse(*c));
    println!("top nets by pad count:");
    for (id, count) in v.iter().take(6) {
        let name = g.board.net(*id).map(|n| n.name.as_str()).unwrap_or("?");
        println!("  {name:<10} {count} pads");
    }
}
