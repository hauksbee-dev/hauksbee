//! Bug hunt — Channel 1 (codegen anomalies) dump.
//!
//! Runs forge-codegen's repeat-block decompiler over the real InputSystem
//! layout and prints, for every multi-instance cluster, the per-instance
//! anomalies (value/lib_id mismatch, missing, extra). A single instance whose
//! VALUE deviates from an otherwise-uniform cluster is a candidate hardware
//! bug (e.g. one synapse with a wrong resistor). Most will be benign role
//! variation; we print them all and let the human/verification step classify.
//!
//! Run: `cargo run -p galvani-engine --example bug_hunt --release`

use forge_codegen::{decompile_analysis, Anomaly};
use forge_model::Pcb;
use std::collections::BTreeMap;
use std::path::Path;

const TARSKI: &str = "/Users/hauksbee-user/Tarski/Tarski-Repos/Tarski-Schematics/Neuron/InputSystem/InputSystem.kicad_pcb";

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| TARSKI.to_string());
    if !Path::new(&path).exists() {
        eprintln!("board not found: {path}");
        std::process::exit(1);
    }
    eprintln!("reading {path}");
    let text = std::fs::read_to_string(&path).expect("read board");
    let pcb = Pcb::parse(&text).expect("parse board");

    let t0 = std::time::Instant::now();
    let (nl, analysis) = decompile_analysis(&pcb);
    eprintln!(
        "decompiled {} components in {:.2}s: {} clusters, {} singletons, coverage {:.1}%",
        nl.comps.len(),
        t0.elapsed().as_secs_f64(),
        analysis.clusters.len(),
        analysis.singletons.len(),
        analysis.cluster_coverage() * 100.0,
    );

    println!("\n===== CLUSTER SUMMARY =====");
    for c in &analysis.clusters {
        println!(
            "{:>4}x  {:>3} comp/inst  {:>5} anomalies   {}",
            c.size(),
            c.template.len(),
            c.anomaly_count(),
            c.name,
        );
    }

    // For each cluster, classify anomalies. We care most about ValueMismatch on
    // a cluster where the mismatch is RARE (one instance differs from many) —
    // that is the "single wrong part in an otherwise uniform array" signal.
    println!("\n===== ANOMALY DETAIL (rare value/lib deviations first) =====");
    for c in &analysis.clusters {
        if c.anomaly_count() == 0 {
            continue;
        }
        // Tally value-mismatches per (slot, found-value) so we can spot which
        // deviations are rare vs which are just "this slot varies by position".
        let mut value_found_counts: BTreeMap<(usize, String), usize> = BTreeMap::new();
        let mut slot_total: BTreeMap<usize, usize> = BTreeMap::new();
        for inst in &c.instances {
            for a in &inst.anomalies {
                if let Anomaly::ValueMismatch { slot, found, .. } = a {
                    *value_found_counts
                        .entry((*slot, found.clone()))
                        .or_default() += 1;
                }
            }
            for (slot, comp) in inst.comps_by_slot.iter().enumerate() {
                if comp.is_some() {
                    *slot_total.entry(slot).or_default() += 1;
                }
            }
        }

        println!("\n--- {} (x{}) ---", c.name, c.size());
        // Print template for reference.
        for tr in &c.template {
            if !tr.value.is_empty() {
                println!(
                    "    template slot {:>2}: {:<40} = {}",
                    tr.slot, tr.lib_id, tr.value
                );
            }
        }

        // Now per-instance anomalies, annotating value mismatches with rarity.
        for inst in &c.instances {
            if inst.anomalies.is_empty() {
                continue;
            }
            // Identify the instance by its first present component reference.
            let label = inst
                .comps_by_slot
                .iter()
                .flatten()
                .chain(inst.extra_comps.iter())
                .next()
                .map(|&ci| nl.comps[ci].reference.clone())
                .unwrap_or_else(|| "?".into());
            for a in &inst.anomalies {
                match a {
                    Anomaly::ValueMismatch {
                        slot,
                        reference,
                        expected,
                        found,
                    } => {
                        let n_this = value_found_counts
                            .get(&(*slot, found.clone()))
                            .copied()
                            .unwrap_or(0);
                        let n_slot = slot_total.get(slot).copied().unwrap_or(0);
                        let rarity = if n_slot > 0 {
                            format!("{n_this}/{n_slot} insts have this value")
                        } else {
                            "?".into()
                        };
                        let flag = if n_this * 10 <= n_slot { " <== RARE" } else { "" };
                        println!(
                            "  [{label}] {reference} slot{slot} VALUE expected={expected:?} found={found:?}  ({rarity}){flag}"
                        );
                    }
                    Anomaly::LibIdMismatch {
                        reference,
                        expected,
                        found,
                        ..
                    } => {
                        println!(
                            "  [{label}] {reference} LIB expected={expected:?} found={found:?}"
                        );
                    }
                    Anomaly::MissingComponent {
                        slot,
                        expected_lib_id,
                        expected_value,
                    } => {
                        println!(
                            "  [{label}] MISSING slot{slot} {expected_lib_id} {expected_value:?}"
                        );
                    }
                    Anomaly::ExtraComponent {
                        reference,
                        lib_id,
                        value,
                    } => {
                        println!("  [{label}] EXTRA {reference} {lib_id} {value:?}");
                    }
                }
            }
        }
    }
}
