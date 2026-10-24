//! The static suite's firmware-intent findings: what the image given with
//! `--firmware` says the program uses, against what the board connects.
//!
//! The image is read here, where the AVR image reader lives
//! ([`hauksbee_mcu::avr_image`], [`hauksbee_mcu::avr_vectors`]); the judgement is
//! `checks::firmware_intent`, which sees only the vector names each part's
//! firmware handles. A part whose vector table is not known, an image that is
//! not AVR, or a file that cannot be read adds nothing: the run's own firmware
//! validation reports those.

use std::path::Path;

use hauksbee_extract::{ExtractedBoard, LintFinding};
use hauksbee_models::ModelLibrary;

use crate::checks::firmware_intent::{firmware_intent_lint, simavr_parts, FirmwareIntent};

/// The vectors the AVR image at `firmware` handles, for each AVR part on `board`.
pub fn read_intent(board: &ExtractedBoard, lib: &ModelLibrary, firmware: &Path) -> FirmwareIntent {
    let mut intent = FirmwareIntent::default();
    let parts = simavr_parts(board, lib);
    if parts.is_empty() {
        return intent;
    }
    let Ok(image) = hauksbee_mcu::avr_image::AvrImage::read(firmware) else {
        return intent;
    };
    for part in &parts {
        if let Some(used) = hauksbee_mcu::avr_vectors::used_vectors(part, &image.flash_head) {
            intent.insert(part, used);
        }
    }
    intent
}

/// The findings for `firmware` on `board`; none without a firmware image.
pub fn findings(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    firmware: Option<&Path>,
) -> Vec<LintFinding> {
    let Some(firmware) = firmware else {
        return Vec::new();
    };
    let intent = read_intent(board, lib, firmware);
    firmware_intent_lint(board, lib, &intent).findings
}
