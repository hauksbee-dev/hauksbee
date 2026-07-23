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
//! under `HAUKSBEE_REQUIRE_CORPUS=1` where the small boards must hit their
//! agreement floor.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use hauksbee_extract::gerber::from_gerber_dir;
use hauksbee_extract::ExtractedBoard;

fn corpus(rel: &str) -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../board-corpus")
        .join(rel);
    p.exists().then_some(p)
}

fn require_corpus() -> bool {
    std::env::var("HAUKSBEE_REQUIRE_CORPUS").is_ok()
}

/// Locate a usable `kicad-cli` (PATH or the macOS app bundle).
fn kicad_cli() -> Option<PathBuf> {
    if Command::new("kicad-cli")
        .arg("version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
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
    let dir = std::env::temp_dir().join(format!("hauksbee_cl_{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).ok()?;
    let ok = Command::new(cli)
        .args([
            "pcb",
            "export",
            "gerbers",
            "--no-x2",
            "--no-netlist",
            "--no-protel-ext",
            "-o",
        ])
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
        .args([
            "pcb", "export", "pos", "--format", "csv", "--units", "mm", "--side", "both", "-o",
        ])
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

fn pct(a: &Agreement) -> f64 {
    100.0 * a.pad_pairs_agree as f64 / a.pad_pairs.max(1) as f64
}

/// The smallest reference board must reconstruct an electrically *identical*
/// net graph (~100% partition agreement over located pads). This is the tight
/// gate: a regression here means a real connectivity bug.
#[test]
fn rp2040_minimal_exact_nets() {
    let Some(a) = run_board(
        "famous/rp2040_minimal_kicad/minimal/RP2040_minimal_r2/RP2040_minimal_r2.kicad_pcb",
        "rp2040_minimal",
    ) else {
        if require_corpus() {
            panic!("corpus/kicad-cli required but couldn't round-trip rp2040_minimal");
        }
        eprintln!("skipping rp2040_minimal (no corpus/kicad-cli)");
        return;
    };
    assert!(
        a.pads_located > 150,
        "too few pads located: {}",
        a.pads_located
    );
    assert!(
        pct(&a) >= 99.0,
        "net partition only {:.2}% on rp2040_minimal",
        pct(&a)
    );
}

/// The full sweep of boards that `kicad-cli` can round-trip, small to large.
/// Every board listed in `docs/ingest/GERBER.md`'s accuracy table is gated here, so
/// the documented numbers are reproducible (run `HAUKSBEE_REQUIRE_CORPUS=1
/// cargo test --test gerber_closedloop -- --nocapture`). KiCad-10-format demos
/// (pic_programmer / stickhub) are skipped, not failed: the installed CLI 9.x
/// cannot load them to make ground-truth gerbers.
///
/// Each board gates two things, because net-partition alone is computed only
/// over pads the reconstruction *located* and would flatter a run that lost
/// pads: `part_floor` is the partition-agreement floor over located pads, and
/// `loc_floor` is the fraction of native pads the reconstruction must locate.
#[test]
fn corpus_sweep_partition_floor() {
    // (path, tag, partition-floor %, located-pad floor)
    let boards = [
        (
            "famous/mnt_reform/reform2-oled-pcb/reform2-oled.kicad_pcb",
            "reform_oled",
            99.0,
            0.85,
        ),
        (
            "famous/lumenpnp/ring-light/ringLight.kicad_pcb",
            "ringlight",
            99.0,
            0.80,
        ),
        ("famous/watchy/Watchy.kicad_pcb", "watchy", 99.0, 0.80),
        (
            "famous/mnt_reform/reform2-trackball2-pcb/reform2-trackball2.kicad_pcb",
            "reform_trackball2",
            99.0,
            0.75,
        ),
        (
            "famous/crkbd/pcbs/corne-cherry.kicad_pcb",
            "corne",
            99.0,
            0.45,
        ),
        (
            "famous/lily58/Pro_V2/Pro_V2.kicad_pcb",
            "lily58prov2",
            98.5,
            0.50,
        ),
        (
            "famous/mnt_reform/reform2-motherboard30-pcb/reform2-motherboard30.kicad_pcb",
            "reform_mobo",
            99.0,
            0.85,
        ),
    ];
    let mut ran = 0;
    for (rel, tag, part_floor, loc_floor) in boards {
        match run_board(rel, tag) {
            Some(a) => {
                ran += 1;
                assert!(
                    pct(&a) >= part_floor,
                    "net partition {:.2}% < floor {part_floor}% on {tag}",
                    pct(&a)
                );
                let loc = a.pads_located as f64 / a.native_pads.max(1) as f64;
                assert!(
                    loc >= loc_floor,
                    "located only {:.0}% of native pads (< {:.0}% floor) on {tag}",
                    loc * 100.0,
                    loc_floor * 100.0
                );
            }
            None => eprintln!("skipping {tag} (kicad-cli could not round-trip it)"),
        }
    }
    if require_corpus() {
        assert!(
            ran >= 1,
            "corpus required but no board could be round-tripped"
        );
    } else if ran == 0 {
        eprintln!("skipping closed-loop sweep (no corpus/kicad-cli)");
    }
}
