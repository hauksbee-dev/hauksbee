//! A firmware co-sim that could not run is not judged, never a board fault.
//!
//! Drives the same analysis `/api/analyze-with-firmware` runs.

use hauksbee_engine::frontdoor::{analyze, analyze_with_firmware, WebReport};

fn read(rel: &str) -> Vec<u8> {
    let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").join(rel);
    std::fs::read(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

fn dump(label: &str, r: &WebReport) {
    eprintln!(
        "[{label}] verdict={} partly={} serious={} total={}\n  headline={}\n  reason={:?}\n  refusal={:?}",
        r.verdict,
        r.partly_judged,
        r.serious,
        r.total,
        r.headline,
        r.verdict_reason,
        r.refusal.as_ref().map(|x| x.claim.clone()),
    );
    if let Some(c) = &r.cosim {
        for f in &c.findings {
            eprintln!("  cosim {}: {}", f.level, f.what);
        }
    }
    if let Some(doc) = &r.check_report {
        for f in doc["findings"].as_array().into_iter().flatten() {
            eprintln!("  machine {} {} {}", f["check"], f["severity"], f["message"]);
        }
    }
}

#[test]
fn cosim_not_judged_repro() {
    for (board, fw) in [
        (
            "testdata/boards/esp32_devkit_demo.kicad_pcb",
            "testdata/firmware/esp32_blinky/flash.bin",
        ),
        (
            "frontend/public/samples/watchy.kicad_pcb",
            "testdata/firmware/watchy_display_init/watchy_display_init.elf",
        ),
    ] {
        let board_bytes = read(board);
        let name = board.rsplit('/').next().unwrap();
        let fw_name = fw.rsplit('/').next().unwrap();
        dump(&format!("{name} static"), &analyze(name, &board_bytes));
        dump(
            &format!("{name} + {fw_name}"),
            &analyze_with_firmware(name, &board_bytes, fw_name, &read(fw)),
        );
    }
    panic!("diagnostic dump");
}
