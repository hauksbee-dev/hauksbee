//! The firmware-intent check from the CI front door: a spec that names
//! firmware gets the same findings `hauksbee check --firmware` reports for that
//! image, as failing rows of the run, and a spec without firmware is unchanged.
//!
//! The firmware is a hand-made `ATmega328P` ELF whose vector table handles the
//! Timer/Counter1 capture event, which fires only from `ICP1` (pad 14). Each
//! flawed board has controls that differ in one thing only: the capture pad is
//! wired, or the image does not handle the capture vector.

use std::path::{Path, PathBuf};

use hauksbee_engine::reports::check::StaticGate;
use hauksbee_engine::result::JsonFinding;
use hauksbee_testkit::avr_firmware::{atmega328p_vector_table, avr_elf};

use crate::support::{board_text, comp, pad, placement_board, write_tmp};

/// Vector numbers are 1-based with reset at 1: `TIMER1_CAPT` is 11.
const TIMER1_CAPT: usize = 11;
/// `TIMER0_OVF`.
const TIMER0_OVF: usize = 17;

/// An `ATmega328P` with its supply, and pad 14 (`ICP1`) wired to a header when
/// `capture_wired`.
fn board(capture_wired: bool) -> PathBuf {
    let mut mcu = pad(7, "+5V") + &pad(8, "GND") + &pad(20, "+5V") + &pad(22, "GND");
    if capture_wired {
        mcu += &pad(14, "SENSE");
    }
    let mut parts = comp("U1", "Package_DIP:DIP-28_W7.62mm", "ATmega328P", 0, mcu);
    if capture_wired {
        parts += &comp(
            "J1",
            "Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical",
            "Conn_01x02",
            40,
            pad(1, "SENSE") + &pad(2, "GND"),
        );
    }
    placement_board("fw_intent.kicad_pcb", &board_text(&["+5V", "GND"], &parts))
}

/// A firmware ELF in a fresh directory whose image handles `handled`.
fn firmware(handled: &[usize]) -> PathBuf {
    let path = write_tmp("fw.elf", "");
    std::fs::write(&path, avr_elf(&atmega328p_vector_table(handled))).expect("write the image");
    path
}

/// The engine's own static gate over `board` with `firmware`: what `hauksbee
/// check --firmware` gates on, whichever way the run reaches it.
fn engine_gate(board: &Path, firmware: Option<&Path>) -> StaticGate {
    let lib = hauksbee_models::ModelLibrary::builtin_with_user_dirs(&[]);
    hauksbee_engine::reports::check::static_gate_as_built(board, &lib, None, firmware)
        .expect("the static suite runs")
}

fn intent_findings(gate: &StaticGate) -> Vec<&JsonFinding> {
    gate.gating
        .iter()
        .filter(|f| f.kind == "firmware_unconnected_pad")
        .collect()
}

#[test]
fn the_static_gate_given_the_image_gates_on_the_unconnected_capture_pad() {
    let board = board(false);
    let fw = firmware(&[TIMER1_CAPT, TIMER0_OVF]);
    let gate = engine_gate(&board, Some(&fw));
    let found = intent_findings(&gate);
    assert_eq!(found.len(), 1, "{:?}", gate.gating);
    assert_eq!(found[0].refs, ["U1"]);
    assert!(found[0].message.contains("ICP1"), "{}", found[0].message);
}

#[test]
fn the_static_gate_without_the_image_says_nothing_about_the_capture_pad() {
    let gate = engine_gate(&board(false), None);
    assert!(intent_findings(&gate).is_empty(), "{:?}", gate.gating);
}

#[cfg(feature = "avr")]
mod run {
    use hauksbee_ir::vocabulary::STATIC_CHECK_KIND;

    use super::*;
    use crate::support::toml_path;

    /// A no-faults spec on `board`, naming `firmware` when given.
    fn spec(board: &Path, firmware: Option<&Path>) -> String {
        let fw = firmware.map_or_else(String::new, |p| {
            format!("firmware = {}\nmcu = \"atmega328p\"\n", toml_path(p))
        });
        format!(
            "name = \"fw intent\"\nboard = {}\n{fw}duration_ms = 2\n\n\
             [[supply]]\nnet = \"+5V\"\nkind = \"ideal\"\nvolts = 5.0\n\n\
             [[assert]]\nkind = \"no_faults\"\n",
            toml_path(board)
        )
    }

    fn run(board: &Path, fw: Option<&Path>) -> hauksbee_ci::CiResult {
        crate::support::run_body("fw intent.toml", &spec(board, fw))
    }

    /// The static rows of the run that speak of the firmware-intent finding.
    fn intent_rows(result: &hauksbee_ci::CiResult) -> Vec<&hauksbee_ci::assertions::AssertResult> {
        result
            .results
            .iter()
            .filter(|r| r.kind == STATIC_CHECK_KIND && r.label.contains("firmware_unconnected_pad"))
            .collect()
    }

    #[test]
    fn a_spec_naming_firmware_that_handles_the_capture_vector_fails_on_the_unwired_pad() {
        let board = board(false);
        let fw = firmware(&[TIMER1_CAPT, TIMER0_OVF]);
        let result = run(&board, Some(&fw));
        let human = result.render_human();
        let rows = intent_rows(&result);
        assert_eq!(rows.len(), 1, "one row, no duplicate:\n{human}");
        let row = rows[0];
        assert!(
            !row.passed && !row.invalid && row.waived.is_none(),
            "{row:?}"
        );
        assert!(row.label.contains("U1"), "names the part: {row:?}");
        assert_ne!(result.exit_code(), 0, "never GREEN over it:\n{human}");
        // It is the finding `hauksbee check --firmware` makes for the image.
        let gate = engine_gate(&board, Some(&fw));
        let engine = intent_findings(&gate);
        assert_eq!(engine.len(), 1);
        let said = if engine[0].plain.trim().is_empty() {
            &engine[0].message
        } else {
            &engine[0].plain
        };
        assert!(
            row.detail.contains(said.as_str()),
            "the row carries the check's own wording:\n{}\n{said}",
            row.detail
        );
    }

    #[test]
    fn the_same_firmware_with_icp1_wired_is_silent() {
        let result = run(&board(true), Some(&firmware(&[TIMER1_CAPT, TIMER0_OVF])));
        assert!(intent_rows(&result).is_empty(), "{}", result.render_human());
    }

    #[test]
    fn firmware_that_does_not_handle_the_capture_vector_is_silent_on_the_same_board() {
        let result = run(&board(false), Some(&firmware(&[TIMER0_OVF])));
        assert!(intent_rows(&result).is_empty(), "{}", result.render_human());
    }

    #[test]
    fn a_spec_without_firmware_is_unchanged() {
        let result = run(&board(false), None);
        assert!(intent_rows(&result).is_empty(), "{}", result.render_human());
    }
}
