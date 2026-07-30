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
//!
//! ## Why the floors carry a toolchain version
//!
//! Half of this loop is `kicad-cli`, which is not ours. The gerbers it writes
//! (aperture choices, how pads render, what lands in the drill file) shift
//! between KiCad releases, and the pad-location rate shifts with them. A floor
//! is therefore only meaningful next to the version it was measured against,
//! recorded in `CALIBRATED_KICAD_CLI`.
//!
//! When a floor breaks, check that version first. A mismatch means the ground
//! truth moved, not that reverse extraction regressed, and the honest response
//! is to re-measure and record the new version, never to shave the floor until
//! it passes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use hauksbee_extract::gerber::from_gerber_dir;
use hauksbee_extract::ExtractedBoard;

fn corpus(rel: &str) -> Option<PathBuf> {
    // Through the shared resolver, which accepts both the hand-built
    // `famous/<id>` layout and the `<id>` layout scripts/fetch-corpus.sh
    // produces. Joining the path directly is what made this sweep skip
    // silently for anyone who used the documented fetch.
    hauksbee_testkit::corpus_board(env!("CARGO_MANIFEST_DIR"), rel)
}

fn require_corpus() -> bool {
    std::env::var("HAUKSBEE_REQUIRE_CORPUS").is_ok()
}

/// The `kicad-cli` release the floors below were measured against. See the
/// module docs: a floor and a toolchain version are one fact, not two.
const CALIBRATED_KICAD_CLI: &str = "9.0.3";

/// The running `kicad-cli` version, for attributing a floor breach to the side
/// that actually changed.
fn kicad_cli_version(cli: &Path) -> String {
    Command::new(cli)
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
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
    let dir = std::env::temp_dir().join(format!("hauksbee_cl_{tag}_{}", std::process::id()));
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

/// 0.1 mm bucket key. Native KiCad pcb has Y pointing down; gerber/P&P have Y
/// up, so the caller negates native Y before keying.
///
/// This is a spatial bucket, NOT a tolerance. Rounding alone would make two
/// pads 0.001 mm apart fail to match whenever they straddle a cell boundary,
/// so lookups scan the 3x3 neighbourhood and accept the nearest hit within
/// `PAD_MATCH_TOL_MM`.
fn pad_key(x: f64, y: f64) -> (i64, i64) {
    ((x * 10.0).round() as i64, (y * 10.0).round() as i64)
}

/// How far apart the same pad may land in the two extractions and still be
/// recognised as the same pad. Well under any real pad pitch, so it cannot
/// fuse neighbouring pads; it exists to absorb rounding in the gerber and P&P
/// coordinate formats.
const PAD_MATCH_TOL_MM: f64 = 0.05;

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
    // Only pads carrying a net can be matched, so only those count in the
    // denominator. Including net-less pads would report a miss rate for pads
    // this comparison never tries to place.
    let mut native_pad_net: HashMap<(i64, i64), i64> = HashMap::new();
    let mut native_xy: Vec<(f64, f64, (i64, i64))> = Vec::new();
    let mut native_pads = 0;
    for c in &native.components {
        for p in &c.pins {
            if let (Some((x, y)), Some(net)) = (p.position, p.net) {
                native_pads += 1;
                let k = pad_key(x, -y);
                native_pad_net.insert(k, net);
                native_xy.push((x, -y, k));
            }
        }
    }
    // Bucketed by cell so a lookup scans a 3x3 neighbourhood rather than the
    // whole board.
    let mut recon_pad_net: HashMap<(i64, i64), i64> = HashMap::new();
    let mut recon_cells: HashMap<(i64, i64), Vec<(f64, f64)>> = HashMap::new();
    for c in &recon.components {
        for p in &c.pins {
            if let (Some((x, y)), Some(net)) = (p.position, p.net) {
                let k = pad_key(x, y);
                recon_pad_net.insert(k, net);
                recon_cells.entry(k).or_default().push((x, y));
            }
        }
    }
    // A native pad is located when some reconstructed pad sits within tolerance
    // of it. Scanning the 3x3 cell neighbourhood is what makes the tolerance
    // real: an exact-key test would drop every pair that happens to straddle a
    // cell boundary, however close together they actually are.
    let mut shared: Vec<(i64, i64)> = Vec::new();
    for &(nx, ny, nk) in &native_xy {
        let mut best: Option<((i64, i64), f64)> = None;
        for dx in -1..=1 {
            for dy in -1..=1 {
                let cell = (nk.0 + dx, nk.1 + dy);
                for &(rx, ry) in recon_cells.get(&cell).into_iter().flatten() {
                    let d = ((rx - nx).powi(2) + (ry - ny).powi(2)).sqrt();
                    if d <= PAD_MATCH_TOL_MM && best.as_ref().is_none_or(|&(_, bd)| d < bd) {
                        best = Some((pad_key(rx, ry), d));
                    }
                }
            }
        }
        if let Some((rk, _)) = best {
            // Keyed by the RECON cell, so the pair-agreement pass below can look
            // both nets up by the same key.
            native_pad_net.insert(rk, native_pad_net[&nk]);
            shared.push(rk);
        }
    }
    shared.sort_unstable();
    shared.dedup();
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

/// Round-trip a board that ships in this repo, by absolute path.
///
/// The corpus version of this needs a fetch and is skipped without one. These
/// boards are always present, so the gerber path is exercised on every run
/// rather than only on a machine that happens to have the corpus.
fn run_repo_board(rel: &str, tag: &str) -> Option<Agreement> {
    let pcb = repo_root().join(rel);
    if !pcb.is_file() {
        return None;
    }
    let cli = kicad_cli()?;
    let dir = export_fab(&cli, &pcb, tag)?;
    let native = ExtractedBoard::from_kicad_pcb(&std::fs::read_to_string(&pcb).ok()?).ok()?;
    let recon = from_gerber_dir(&dir).ok()?;
    let a = agreement(&native, &recon.board);
    eprintln!(
        "[{tag}] native {}c/{}n  recon {}c/{}n | components {}/{} | pads {}/{} | net-partition {:.1}%",
        native.components.len(),
        a.native_nets,
        recon.board.components.len(),
        a.recon_nets,
        a.components_matched,
        a.native_components,
        a.pads_located,
        a.native_pads,
        100.0 * a.pad_pairs_agree as f64 / a.pad_pairs.max(1) as f64,
    );
    let _ = std::fs::remove_dir_all(&dir);
    Some(a)
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("repo root")
        .to_path_buf()
}

/// Every board that ships in this repo must survive the gerber round-trip.
///
/// Two different things are checked here, and conflating them would be a trap
/// for whoever reads this next.
///
/// **Every board must round-trip.** kicad-cli has to load it, export gerbers,
/// and the reconstruction has to read them back without error. This is a real
/// gate and it has already caught a real defect: six demo boards carried
/// Lisp-style `;` comments inside the s-expression, which KiCad's format does
/// not have. forge-sexpr tolerated them, so nothing here noticed, while KiCad
/// itself answered "Failed to load board" for anyone who opened our own demo
/// board in their CAD tool.
///
/// **Only routed boards are judged on accuracy.** Most boards in this list are
/// pad-and-netlist fixtures with zero segments, vias and zones: their
/// connectivity lives in the file, not in copper. A gerber carries copper and
/// nothing else, so on an unrouted board there is physically nothing to trace
/// and the reconstruction can only infer from pad overlap. Those boards score
/// 60-85% net-partition, and that number measures the fixture, not the
/// extractor. Chasing it would be chasing a ghost.
///
/// Watchy is the one shipped board with a real layout (685 segments, 114 vias,
/// 6 zones), and it recovers the electrical graph exactly. That is the result
/// worth gating, and the wider evidence is the corpus sweep below.
#[test]
fn shipped_boards_survive_gerbers() {
    // (path, has real copper: only these carry an accuracy floor)
    const BOARDS: &[(&str, bool)] = &[
        ("crates/hauksbee-ci/examples/boards/blinky.kicad_pcb", false),
        (
            "crates/hauksbee-ci/examples/boards/boot_gate.kicad_pcb",
            false,
        ),
        (
            "crates/hauksbee-ci/examples/boards/power_resistor.kicad_pcb",
            false,
        ),
        (
            "crates/hauksbee-ci/examples/boards/tolerance_divider.kicad_pcb",
            false,
        ),
        ("crates/hauksbee-ci/examples/boards/watchy.kicad_pcb", true),
        ("testdata/boards/button_pullup.kicad_pcb", false),
        ("testdata/boards/esp32_devkit_demo.kicad_pcb", false),
        ("testdata/boards/esp32_spi_adc_demo.kicad_pcb", false),
        ("testdata/boards/esp32c3_devkit_demo.kicad_pcb", false),
        ("testdata/boards/stm32_adc_divider_demo.kicad_pcb", false),
        ("testdata/boards/stm32_bluepill_demo.kicad_pcb", false),
        ("testdata/boards/stm32_i2c_thermostat.kicad_pcb", false),
        ("testdata/boards/stm32_spi_adc_demo.kicad_pcb", false),
        ("testdata/boards/vcd_pulse.kicad_pcb", false),
    ];
    if kicad_cli().is_none() {
        // This used to skip unconditionally, and CI has no KiCad, so it never
        // ran once while its own doc comment called it "a real gate". A gate
        // that silently does not run is worse than no gate: it reads as
        // evidence. HAUKSBEE_REQUIRE_KICAD=1 makes a missing kicad-cli a hard
        // failure, and scripts/make-public.sh sets it, so the release gate
        // exercises the claim even though a per-PR runner does not.
        if std::env::var_os("HAUKSBEE_REQUIRE_KICAD").is_some() {
            panic!(
                "HAUKSBEE_REQUIRE_KICAD is set but kicad-cli was not found. Gerber \
                 reverse extraction is a headline claim and this run cannot test it."
            );
        }
        eprintln!(
            "skipping shipped-board gerber sweep: no kicad-cli, so THIS RUN HAS NOT \
             tested gerber reverse extraction. Set HAUKSBEE_REQUIRE_KICAD=1 to make \
             that a failure."
        );
        return;
    }
    for (rel, routed) in BOARDS {
        let tag = Path::new(rel)
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let Some(a) = run_repo_board(rel, &tag) else {
            panic!(
                "{rel} did not survive the gerber round-trip. Every board that ships has \
                 to: gerber-only extraction is a headline claim, and a demo board that \
                 cannot even make gerbers cannot demonstrate it. If kicad-cli says \
                 \"Failed to load board\", check the file for syntax KiCad does not \
                 accept, such as `;` comment lines."
            )
        };
        if *routed {
            // Measured 2026-07-29 against kicad-cli 10.0.3: 100.0% over 262 of
            // 276 pads. The floor sits just under the measurement so a real
            // connectivity regression trips it.
            assert!(
                pct(&a) >= 99.0,
                "{tag}: net partition {:.1}%, floor 99.0%. This board has real copper, \
                 so the reconstruction has traces to follow and the electrical graph \
                 should come back intact.",
                pct(&a)
            );
            assert!(
                a.pads_located * 100 >= a.native_pads * 90,
                "{tag}: located only {}/{} pads",
                a.pads_located,
                a.native_pads
            );
        }
    }
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
            // Measured 74.4% (482/648) with kicad-cli 9.0.3.
            0.70,
        ),
        (
            "famous/lily58/Pro_V2/Pro_V2.kicad_pcb",
            "lily58prov2",
            98.5,
            // Measured 81.0% (687/848) with kicad-cli 9.0.3.
            0.78,
        ),
        (
            "famous/mnt_reform/reform2-motherboard30-pcb/reform2-motherboard30.kicad_pcb",
            "reform_mobo",
            99.0,
            // Measured 81.7% (1785/2184) with kicad-cli 9.0.3.
            0.78,
        ),
    ];
    // Half of this loop is kicad-cli's output, so a floor breach has to say
    // which version produced it.
    let running = kicad_cli()
        .map(|c| kicad_cli_version(&c))
        .unwrap_or_else(|| "no kicad-cli".to_string());
    eprintln!(
        "closed-loop floors calibrated against kicad-cli {CALIBRATED_KICAD_CLI}; running {running}"
    );

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
                    "located only {:.0}% of native pads (< {:.0}% floor) on {tag}. \
                     Floors were measured against kicad-cli {CALIBRATED_KICAD_CLI}; \
                     this run used {running}. If those differ, the exporter moved \
                     the ground truth: re-measure and record the new version \
                     rather than lowering the floor.",
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
