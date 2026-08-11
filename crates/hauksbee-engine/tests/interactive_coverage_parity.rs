//! Co-sim coverage parity on the INTERACTIVE surfaces.
//!
//! `docs/cosim/MCU.md` measured the typed per-run co-sim disclosures across the
//! run-report surfaces and found the interactive ones quiet. Two silent classes
//! reachable on the AVR backend it actually runs (watchdog reboots, and per-core
//! timing coverage). A user watching a live co-sim saw no caveat and concluded
//! the board was fully modelled, while the same run under `--check --json` said
//! an ADC channel was dropped.
//!
//! The TUI half is tested in-crate against the real `draw` through a
//! TestBackend (`tui::render`'s `every_coverage_class_reaches_the_cosim_pane_and_
//! its_overlay`), because `draw` is crate-private. This file covers the web
//! front door, whose `analyze_with_firmware` is public, two-sided per class:
//!
//!   * a firmware that starves its watchdog (`wdt.elf`) must produce the reboot
//!     caveat, and the SAME firmware with the one arming line removed
//!     (`nowdt.elf`) must produce none, so the caveat tracks the real
//!     `Mcu::watchdog_resets` signal rather than being always-on;
//!   * a run with a live core must report the timing resolution it measured, and
//!     a board with no MCU (no co-sim, so no core) must report none.
//!
//! Runs AVR firmware in process, so it needs the GPL-gated `avr` feature (the
//! GPL-free renode/qemu build refuses AVR firmware by design).

#![cfg(feature = "avr")]

use std::path::PathBuf;

use hauksbee_engine::frontdoor::{
    analyze_with_firmware, analyze_with_firmware_json, WebCosimSection, WebReport,
};

fn repo(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel)
}

/// An ATmega328P board, the same one the batch-surface watchdog test uses.
fn avr_board() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../hauksbee-ci/examples/boards/blinky.kicad_pcb")
}

/// Run the web front door over the AVR board and a firmware image.
fn web_run(fw_rel: &str) -> WebReport {
    let fw = repo(fw_rel);
    assert!(fw.exists(), "tracked required fixture is absent: {fw_rel}");
    let board = std::fs::read(avr_board()).expect("the AVR board fixture reads");
    let fw_bytes = std::fs::read(&fw).expect("the firmware fixture reads");
    let fw_name = fw.file_name().unwrap().to_str().unwrap().to_string();
    analyze_with_firmware("blinky.kicad_pcb", &board, &fw_name, &fw_bytes)
}

fn web_run_json(fw_rel: &str) -> serde_json::Value {
    let fw = repo(fw_rel);
    assert!(fw.exists(), "tracked required fixture is absent: {fw_rel}");
    let board = std::fs::read(avr_board()).expect("the AVR board fixture reads");
    let fw_bytes = std::fs::read(&fw).expect("the firmware fixture reads");
    let fw_name = fw.file_name().unwrap().to_str().unwrap();
    serde_json::from_str(&analyze_with_firmware_json(
        "blinky.kicad_pcb",
        &board,
        fw_name,
        &fw_bytes,
    ))
    .expect("firmware web report is JSON")
}

fn cosim(report: &WebReport) -> &WebCosimSection {
    let cosim = report
        .cosim
        .as_ref()
        .expect("a firmware run has a co-sim section");
    assert!(
        cosim.ran,
        "the AVR co-sim must have run: {:?}",
        cosim.findings
    );
    cosim
}

/// Every note-level `why` on the co-sim section, joined, so an assertion reads
/// against the sentence the surface actually rendered.
fn note_prose(cosim: &WebCosimSection) -> String {
    cosim
        .findings
        .iter()
        .map(|f| format!("{} :: {}", f.what, f.why))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn a_watchdog_reboot_reaches_the_web_front_door_and_a_fed_watchdog_does_not() {
    let report = web_run("testdata/firmware/avr_watchdog/wdt.elf");
    let section = cosim(&report);
    let prose = note_prose(section);
    // The shared formatter's sentence, not a web paraphrase of it.
    assert!(
        prose.contains("the watchdog rebooted the core"),
        "the web co-sim card must carry the reboot caveat:\n{prose}"
    );
    assert!(
        prose.contains("behaviour observed after the first reboot belongs to a rebooted core"),
        "and the whole sentence, verbatim:\n{prose}"
    );
    // It is an honesty caveat about what the run means, not a board defect, so
    // it must be note-level (and therefore demote the headline rather than
    // inventing a serious fault).
    let reboot = section
        .findings
        .iter()
        .find(|f| f.why.contains("the watchdog rebooted the core"))
        .expect("the reboot finding exists");
    assert_eq!(reboot.level, "note", "{reboot:?}");
    assert!(
        !reboot.fix.trim().is_empty(),
        "an abstention names the input that would unlock it: {reboot:?}"
    );
    // A run whose firmware was rebooted mid-window cannot read "Looks healthy".
    assert!(
        !report.headline.contains("Looks healthy"),
        "headline: {}",
        report.headline
    );

    // The silence control: the same firmware with the arming line removed.
    let control = web_run("testdata/firmware/avr_watchdog/nowdt.elf");
    let control_prose = note_prose(cosim(&control));
    assert!(
        !control_prose.contains("watchdog rebooted"),
        "a watchdog that was never armed must produce NO reboot caveat:\n{control_prose}"
    );
}

#[test]
fn per_core_timing_coverage_reaches_the_web_front_door_and_a_run_with_no_core_carries_none() {
    let report = web_run_json("testdata/firmware/avr_watchdog/nowdt.elf");
    let section = &report["cosim"];
    // One row per live MCU, from the same accessor the CLI `--json`
    // `cosim.timing_coverage` field reads.
    assert_eq!(
        section["timing_coverage"].as_array().unwrap().len(),
        1,
        "one live MCU on this board: {:?}",
        section["timing_coverage"]
    );
    let row = &section["timing_coverage"][0];
    assert_eq!(row["mcu_ref"], "U1");
    assert!(
        row["backend"].as_str().unwrap().starts_with("simavr:"),
        "backend: {}",
        row["backend"]
    );
    // A measurement, not a placeholder: the resolution numbers are finite and
    // positive, and the chunk is the one the run actually used.
    assert!(
        row["timestamp_precision_s"].as_f64().unwrap() > 0.0
            && row["timestamp_precision_s"].as_f64().unwrap().is_finite(),
        "{row:?}"
    );
    assert!(
        row["minimum_guaranteed_pulse_s"].as_f64().unwrap() > 0.0
            && row["minimum_guaranteed_pulse_s"]
                .as_f64()
                .unwrap()
                .is_finite(),
        "{row:?}"
    );
    assert!(
        row["chunk_s"].as_f64().unwrap() > 0.0 && row["chunk_s"].as_f64().unwrap().is_finite(),
        "{row:?}"
    );
    // The in-process AVR core stamps edges from cycles, so this run is the
    // cycle-exact side of the tier split (a poll backend reports the other side,
    // covered in-crate in `reports::coverage`).
    assert_eq!(row["cycle_exact"], true, "{row:?}");

    // The other side: a board with no MCU runs no co-sim, so there is no core
    // whose resolution could be reported. The field is empty, never a fabricated
    // row.
    let bytes = std::fs::read(repo(
        "crates/hauksbee-ci/examples/boards/tolerance_divider.kicad_pcb",
    ))
    .expect("the MCU-less example board reads");
    let quiet: serde_json::Value = serde_json::from_str(&analyze_with_firmware_json(
        "no_mcu.kicad_pcb",
        &bytes,
        "nowdt.elf",
        &[0u8; 4],
    ))
    .expect("MCU-less report is JSON");
    let quiet_cosim = &quiet["cosim"];
    assert_eq!(quiet_cosim["ran"], false, "no MCU, so no co-sim ran");
    assert!(
        quiet_cosim.get("timing_coverage").is_none(),
        "no live core means no measured resolution: {:?}",
        quiet_cosim
    );
}
