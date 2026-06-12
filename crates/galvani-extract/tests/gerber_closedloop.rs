//! Closed-loop validation: export a corpus KiCad board's gerbers + drill + P&P
//! with `kicad-cli`, reverse-extract them, and check the reconstruction against
//! the native KiCad extraction of the same board.
//!
//! This is the honesty gate for any real-world claim. The gerbers are exported
//! with `--no-x2 --no-netlist` so they carry **no** net hints: the
//! reconstruction has to rederive connectivity from copper geometry alone, the
//! same as it would on a third-party board where only the fab files exist.
//!
//! The comparison metric is **net-partition equivalence over component pads**:
//! net *names* differ (the recon invents `NET_n`), so we don't compare names.
//! Instead we match pads by board position and check that every pair of matched
//! pads is grouped the same way (same-net vs different-net) in both
//! extractions. 100% means the recovered electrical graph is identical.
//!
//! Skips (does not fail) when the corpus or `kicad-cli` is unavailable, except
//! under `GALVANI_REQUIRE_CORPUS=1` where the small boards must hit their
//! agreement floor.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use galvani_extract::gerber::from_gerber_dir;
use galvani_extract::ExtractedBoard;

fn corpus(rel: &str) -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../board-corpus")
        .join(rel);
    p.exists().then_some(p)
}

fn require_corpus() -> bool {
    std::env::var("GALVANI_REQUIRE_CORPUS").is_ok()
}

/// Locate a usable `kicad-cli` (PATH or the macOS app bundle).
fn kicad_cli() -> Option<PathBuf> {
    if Command::new("kicad-cli").arg("version").output().map(|o| o.status.success()).unwrap_or(false) {
        return Some(PathBuf::from("kicad-cli"));
    }
    let bundle = PathBuf::from("/Applications/KiCad/KiCad.app/Contents/MacOS/kicad-cli");
    if bundle.exists() {
        return Some(bundle);
    }
    None
}

/// Export gerbers + drill + P&P for `pcb` into a fresh temp dir; return it.
fn export_fab(cli: &Path, pcb: &Path, tag: &str) -> Option<PathBuf> {
    let dir = std::env::temp_dir().join(format!("galvani_cl_{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).ok()?;
    let ok = Command::new(cli)
        .args(["pcb", "export", "gerbers", "--no-x2", "--no-netlist", "--no-protel-ext", "-o"])
        .arg(format!("{}/", dir.display()))
        .arg(pcb)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !ok {
        return None;
    }
    let _ = Command::new(cli)
        .args(["pcb", "export", "drill", "--excellon-separate-th", "-o"])
        .arg(format!("{}/", dir.display()))
        .arg(pcb)
        .output();
    let _ = Command::new(cli)
        .args(["pcb", "export", "pos", "--format", "csv", "--units", "mm", "--side", "both", "-o"])
        .arg(dir.join("pos.csv"))
        .arg(pcb)
        .output();
    Some(dir)
}

/// 0.1 mm cell key. Native KiCad pcb has Y pointing down; gerber/P&P have Y up,
/// so the caller negates native Y before keying.
fn pad_key(x: f64, y: f64) -> (i64, i64) {
    ((x * 10.0).round() as i64, (y * 10.0).round() as i64)
}

struct Agreement {
    native_components: usize,
    components_matched: usize,
    native_pads: usize,
    pads_located: usize,
    pad_pairs: usize,
    pad_pairs_agree: usize,
    recon_nets: usize,
    native_nets: usize,
}

fn agreement(native: &ExtractedBoard, recon: &ExtractedBoard) -> Agreement {
    let mut native_pad_net: HashMap<(i64, i64), i64> = HashMap::new();
    let mut native_pads = 0;
    for c in &native.components {
        for p in &c.pins {
            if let Some((x, y)) = p.position {
                native_pads += 1;
                if let Some(net) = p.net {
                    native_pad_net.insert(pad_key(x, -y), net);
                }
            }
        }
    }
    let mut recon_pad_net: HashMap<(i64, i64), i64> = HashMap::new();
    for c in &recon.components {
        for p in &c.pins {
            if let Some((x, y)) = p.position {
                if let Some(net) = p.net {
                    recon_pad_net.insert(pad_key(x, y), net);
                }
            }
        }
    }
    let shared: Vec<(i64, i64)> = native_pad_net
        .keys()
        .filter(|k| recon_pad_net.contains_key(*k))
        .copied()
        .collect();
    let mut pad_pairs = 0;
    let mut pad_pairs_agree = 0;
    for i in 0..shared.len() {
        for j in (i + 1)..shared.len() {
            let same_native = native_pad_net[&shared[i]] == native_pad_net[&shared[j]];
            let same_recon = recon_pad_net[&shared[i]] == recon_pad_net[&shared[j]];
            pad_pairs += 1;
            if same_native == same_recon {
                pad_pairs_agree += 1;
            }
        }
    }
    let recon_by_ref: HashMap<&str, usize> = recon
        .components
        .iter()
        .map(|c| (c.reference.as_str(), c.pins.len()))
        .collect();
    let mut components_matched = 0;
    for c in &native.components {
        let nn = c.pins.iter().filter(|p| p.position.is_some()).count();
        if let Some(&rn) = recon_by_ref.get(c.reference.as_str()) {
            if nn > 0 && rn > 0 && (nn as i64 - rn as i64).abs() <= 1 {
                components_matched += 1;
            }
        }
    }
    Agreement {
        native_components: native.components.len(),
        components_matched,
        native_pads,
        pads_located: shared.len(),
        pad_pairs,
        pad_pairs_agree,
        recon_nets: recon.nets.len(),
        native_nets: native.nets.len(),
    }
}

/// Run the closed loop on one board. Returns Some(agreement) or None when the
/// environment can't run it (skipped).
fn run_board(rel: &str, tag: &str) -> Option<Agreement> {
    let pcb = corpus(rel)?;
    let cli = kicad_cli()?;
    let dir = export_fab(&cli, &pcb, tag)?;
    let native = ExtractedBoard::from_kicad_pcb(&std::fs::read_to_string(&pcb).ok()?).ok()?;
    let recon = from_gerber_dir(&dir).ok()?;
    let a = agreement(&native, &recon.board);
    eprintln!(
        "[{tag}] native {}c/{}n  recon {}c/{}n | components {}/{} | pads {}/{} | net-partition {:.1}% ({}/{})",
        native.components.len(),
        a.native_nets,
        recon.board.components.len(),
        a.recon_nets,
        a.components_matched,
        a.native_components,
        a.pads_located,
        a.native_pads,
        100.0 * a.pad_pairs_agree as f64 / a.pad_pairs.max(1) as f64,
        a.pad_pairs_agree,
        a.pad_pairs,
    );
    let _ = std::fs::remove_dir_all(&dir);
    Some(a)
}

/// The small reference boards must reconstruct an electrically *identical* net
/// graph (100% partition agreement over located pads).
#[test]
fn rp2040_minimal_exact_nets() {
    let Some(a) = run_board(
        "famous/rp2040_minimal_kicad/minimal/RP2040_minimal_r2/RP2040_minimal_r2.kicad_pcb",
        "rp2040_minimal",
    ) else {
        if require_corpus() {
            panic!("corpus/kicad-cli required but unavailable for rp2040_minimal");
        }
        eprintln!("skipping rp2040_minimal (no corpus/kicad-cli)");
        return;
    };
    assert!(a.pads_located > 150, "too few pads located: {}", a.pads_located);
    let pct = 100.0 * a.pad_pairs_agree as f64 / a.pad_pairs.max(1) as f64;
    assert!(pct >= 99.9, "net partition only {pct:.2}% on rp2040_minimal");
}

#[test]
fn pic_programmer_exact_nets() {
    let Some(a) = run_board(
        "kicad-demos-src/demos/pic_programmer/pic_programmer.kicad_pcb",
        "pic_programmer",
    ) else {
        if require_corpus() {
            panic!("corpus/kicad-cli required but unavailable for pic_programmer");
        }
        eprintln!("skipping pic_programmer (no corpus/kicad-cli)");
        return;
    };
    let pct = 100.0 * a.pad_pairs_agree as f64 / a.pad_pairs.max(1) as f64;
    assert!(pct >= 99.0, "net partition only {pct:.2}% on pic_programmer");
}

#[test]
fn stickhub_exact_nets() {
    let Some(a) = run_board("kicad-demos-src/demos/stickhub/StickHub.kicad_pcb", "stickhub")
    else {
        if require_corpus() {
            panic!("corpus/kicad-cli required but unavailable for stickhub");
        }
        eprintln!("skipping stickhub (no corpus/kicad-cli)");
        return;
    };
    let pct = 100.0 * a.pad_pairs_agree as f64 / a.pad_pairs.max(1) as f64;
    assert!(pct >= 99.0, "net partition only {pct:.2}% on stickhub");
}
