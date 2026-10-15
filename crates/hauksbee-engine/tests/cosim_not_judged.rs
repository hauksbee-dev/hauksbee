//! A firmware co-sim that could not run is not judged, never a board fault.
//!
//! Drives the same analysis `/api/analyze-with-firmware` runs. Both sample
//! boards select an ESP32 that only an external emulator runs, which the quick
//! web report cannot host, so the firmware is attached but never executed.

use hauksbee_engine::frontdoor::{analyze, analyze_with_firmware, WebReport};

fn read(rel: &str) -> Vec<u8> {
    let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel);
    std::fs::read(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

fn analyze_pair(board: &str, fw: &str) -> (WebReport, WebReport) {
    let bytes = read(board);
    let name = board.rsplit('/').next().expect("board name");
    let fw_name = fw.rsplit('/').next().expect("firmware name");
    (
        analyze(name, &bytes),
        analyze_with_firmware(name, &bytes, fw_name, &read(fw)),
    )
}

/// The run says what it did not judge, why, and what to do next.
fn assert_named_not_judged(report: &WebReport) {
    let refusal = report
        .refusal
        .as_ref()
        .expect("a co-sim that did not run is refused, with its reason");
    assert!(!refusal.missing_prerequisite.trim().is_empty());
    assert!(!refusal.next_action.trim().is_empty());
    let cosim = report.cosim.as_ref().expect("co-sim section");
    assert!(!cosim.ran);
    assert!(
        cosim
            .findings
            .iter()
            .all(|f| f.level != "serious" && f.level != "warning"),
        "a run that did not execute observed nothing to fault"
    );
    let doc = report.check_report.as_ref().expect("machine document");
    assert!(
        doc["findings"]
            .as_array()
            .into_iter()
            .flatten()
            .all(|f| f["check"] != "cosim"),
        "the co-sim limit must not enter the gating findings"
    );
}

/// UX audit H1: attaching firmware to the unrouted ESP32 demo flipped the
/// headline from the pre-layout frame to "a finding fails the check gate".
/// The verdict is the unrouted board's, with or without the firmware.
#[test]
fn unrunnable_firmware_does_not_change_an_unrouted_board_verdict() {
    let (board_only, with_fw) = analyze_pair(
        "testdata/boards/esp32_devkit_demo.kicad_pcb",
        "testdata/firmware/esp32_blinky/flash.bin",
    );
    assert_named_not_judged(&with_fw);
    assert_eq!(with_fw.verdict, board_only.verdict);
    assert_eq!(with_fw.verdict_reason, board_only.verdict_reason);
    assert_eq!(with_fw.headline, board_only.headline);
    assert!(
        !with_fw.headline.contains("fails the check gate"),
        "{}",
        with_fw.headline
    );
}

/// The Watchy sample: nothing fails statically, so a co-sim that could not run
/// leaves the run inconclusive, never failed.
#[test]
fn unrunnable_firmware_on_a_board_with_no_failure_is_inconclusive() {
    let (board_only, with_fw) = analyze_pair(
        "frontend/public/samples/watchy.kicad_pcb",
        "testdata/firmware/watchy_display_init/watchy_display_init.elf",
    );
    assert_ne!(board_only.verdict, "fail");
    assert_named_not_judged(&with_fw);
    assert_eq!(with_fw.verdict, "invalid");
    assert!(
        !with_fw.headline.starts_with("Not OK to order"),
        "{}",
        with_fw.headline
    );
    let doc = with_fw.check_report.as_ref().expect("machine document");
    assert_eq!(doc["verdict"], "invalid");
}
