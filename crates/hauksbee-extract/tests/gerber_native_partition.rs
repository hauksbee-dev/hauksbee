//! Differential Gerber connectivity gate against the native layout exported
//! beside the films.
//!
//! The native file is not copied into this repository. The corpus resolver
//! makes the gate mandatory when `HAUKSBEE_REQUIRE_CORPUS=1` and visibly skips
//! it on a checkout that has no separately fetched, license-reviewed corpus.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use hauksbee_extract::gerber::from_gerber_dir;
use hauksbee_extract::ExtractedBoard;

struct TempJob(PathBuf);

impl TempJob {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "hauksbee_gerber_native_{tag}_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create differential Gerber job");
        Self(path)
    }
}

impl Drop for TempJob {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn csv_cell(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn is_bottom(layer: &str) -> bool {
    matches!(
        layer.trim().to_ascii_lowercase().as_str(),
        "bottom" | "back" | "b.cu" | "16"
    )
}

fn pad_cell(x: f64, y: f64) -> (i64, i64) {
    // The Gerber binder itself deduplicates coincident flashes on this 0.05 mm
    // grid. Using the same physical cell makes the comparison insensitive to
    // harmless exporter decimal rounding without moving a pad across an
    // ordinary fabrication clearance.
    ((x * 20.0).round() as i64, (y * 20.0).round() as i64)
}

fn insert_unambiguous(
    map: &mut BTreeMap<(i64, i64), i64>,
    ambiguous: &mut BTreeSet<(i64, i64)>,
    key: (i64, i64),
    net: i64,
) {
    if ambiguous.contains(&key) {
        return;
    }
    match map.get(&key) {
        Some(existing) if *existing != net => {
            map.remove(&key);
            ambiguous.insert(key);
        }
        _ => {
            map.insert(key, net);
        }
    }
}

fn copy_copper_films(source: &Path, destination: &Path) {
    for entry in std::fs::read_dir(source).expect("read corpus production directory") {
        let entry = entry.expect("read production entry");
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("");
        if matches!(extension.to_ascii_lowercase().as_str(), "gtl" | "gbl") {
            std::fs::copy(&path, destination.join(entry.file_name()))
                .expect("copy corpus copper film");
        }
    }
}

fn native_placement_csv(board: &ExtractedBoard) -> String {
    let mut csv = String::from("Ref,Val,Package,PosX,PosY,Rot,Side\n");
    for component in &board.components {
        let Some((x, y, rotation)) = component.position else {
            continue;
        };
        let package = if component.footprint.is_empty() {
            &component.lib_id
        } else {
            &component.footprint
        };
        csv.push_str(&format!(
            "{},{},{},{x:.6},{y:.6},{rotation:.6},{}\n",
            csv_cell(&component.reference),
            csv_cell(&component.value),
            csv_cell(package),
            if is_bottom(&component.layer) {
                "bottom"
            } else {
                "top"
            }
        ));
    }
    csv
}

#[test]
fn sparkfun_panel_gerbers_never_merge_native_layout_nets() {
    let Some(production) = hauksbee_testkit::corpus_or_skip(
        env!("CARGO_MANIFEST_DIR"),
        "sparkfun_thingplus_rp2040/Hardware/Production",
        "SparkFun RP2040 native-layout/Gerber partition oracle",
    ) else {
        return;
    };
    let native_path = production.join("RP2040_Thing_Plus-Panel.brd");
    let native_text = std::fs::read_to_string(&native_path).expect("read native Eagle panel");
    let native = ExtractedBoard::from_eagle_brd(&native_text).expect("parse native Eagle panel");

    // The published manufacturing folder has no pick-and-place file. Generate
    // one from the native layout solely to expose the reconstructed Gerber net
    // at each physical pad centre; the net comparison below never trusts the
    // generated reference, value, or package as connectivity authority.
    let job = TempJob::new("sparkfun_rp2040");
    copy_copper_films(&production, &job.0);
    std::fs::write(
        job.0.join("native-oracle-pos.csv"),
        native_placement_csv(&native),
    )
    .expect("write native placement probe");
    let reconstructed = from_gerber_dir(&job.0).expect("reverse-extract production films");

    let mut native_at = BTreeMap::new();
    let mut native_ambiguous = BTreeSet::new();
    for component in &native.components {
        for pin in &component.pins {
            if let (Some(net), Some((x, y))) = (pin.net, pin.position) {
                insert_unambiguous(&mut native_at, &mut native_ambiguous, pad_cell(x, y), net);
            }
        }
    }

    let mut reconstructed_at = BTreeMap::new();
    let mut reconstructed_ambiguous = BTreeSet::new();
    for component in &reconstructed.board.components {
        for pin in &component.pins {
            if let (Some(net), Some((x, y))) = (pin.net, pin.position) {
                insert_unambiguous(
                    &mut reconstructed_at,
                    &mut reconstructed_ambiguous,
                    pad_cell(x, y),
                    net,
                );
            }
        }
    }

    let mut native_by_reconstructed: BTreeMap<i64, BTreeSet<i64>> = BTreeMap::new();
    let mut shared_pads = 0usize;
    for (cell, reconstructed_net) in &reconstructed_at {
        let Some(native_net) = native_at.get(cell) else {
            continue;
        };
        shared_pads += 1;
        native_by_reconstructed
            .entry(*reconstructed_net)
            .or_default()
            .insert(*native_net);
    }
    let witnessed_nets = native_by_reconstructed
        .values()
        .filter(|native_nets| !native_nets.is_empty())
        .count();
    let false_merges: Vec<_> = native_by_reconstructed
        .iter()
        .filter(|(_, native_nets)| native_nets.len() > 1)
        .map(|(reconstructed_net, native_nets)| (*reconstructed_net, native_nets.clone()))
        .collect();

    eprintln!(
        "native Gerber oracle: {shared_pads} shared pads across {witnessed_nets} reconstructed nets; {} false merges",
        false_merges.len()
    );

    assert!(
        shared_pads >= 500,
        "the oracle must compare a substantial real pad set, got {shared_pads}"
    );
    assert!(
        witnessed_nets >= 100,
        "the oracle must exercise a substantial reconstructed partition, got {witnessed_nets} nets"
    );
    assert!(
        false_merges.is_empty(),
        "Gerber reconstruction merged native-layout nets at shared pad centres: {false_merges:?}"
    );
}
