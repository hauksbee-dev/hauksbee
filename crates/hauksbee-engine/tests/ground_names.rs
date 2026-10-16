//! A ground is a reference whatever it is called.
//!
//! Renaming the one ground net of a board changes no circuit property, so it
//! must change no verdict: not by losing the solver's 0 V node (a board whose
//! only ground was `GND1` or `0V` solved with no reference and printed
//! 424 V on a 5 V part), and not by hiding it from a check that reads net
//! names. The faulty stage keeps its finding under every spelling, the legal
//! one stays quiet, and both read the same apart from the name.

use hauksbee_extract::{ExtractedBoard, LintCheck, Severity};
use hauksbee_models::ModelLibrary;

/// Every spelling of a board's one ground that the net table recognises.
const SPELLINGS: [&str; 8] = [
    "GND",
    "0V",
    "GND1",
    "GROUND",
    "GNDREF",
    "AGND",
    "VSS",
    "/Power/0V",
];

/// U1 74HC04 with input 1 grounded driving Q1 BC847's base through
/// `base_ohms`; Q1's emitter is grounded and its collector pulled to +5V.
/// Every ground pad is on `ground`.
fn stage(ground: &str, base_ohms: &str) -> String {
    format!(
        r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "{ground}")
  (net 2 "+5V")
  (net 3 "DRIVE")
  (net 4 "BASE")
  (net 5 "COLL")
  (module Package_SO:SOIC-14_3.9x8.7mm_P1.27mm (layer F.Cu)
    (at 100 100)
    (fp_text reference U1 (at 0 0) (layer F.SilkS))
    (fp_text value 74HC04 (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 1 "{ground}"))
    (pad 2 smd rect (at 0 1) (net 3 "DRIVE"))
    (pad 7 smd rect (at 0 2) (net 1 "{ground}"))
    (pad 14 smd rect (at 0 3) (net 2 "+5V"))
  )
  (module Resistor_SMD:R_0603_1608Metric (layer F.Cu)
    (at 110 100)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value {base_ohms} (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 3 "DRIVE"))
    (pad 2 smd rect (at 1 0) (net 4 "BASE"))
  )
  (module Package_TO_SOT_SMD:SOT-23 (layer F.Cu)
    (at 120 100)
    (fp_text reference Q1 (at 0 0) (layer F.SilkS))
    (fp_text value BC847 (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 4 "BASE"))
    (pad 2 smd rect (at 1 0) (net 1 "{ground}"))
    (pad 3 smd rect (at 2 0) (net 5 "COLL"))
  )
  (module Resistor_SMD:R_0603_1608Metric (layer F.Cu)
    (at 130 100)
    (fp_text reference R2 (at 0 0) (layer F.SilkS))
    (fp_text value 1k (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 2 "+5V"))
    (pad 2 smd rect (at 1 0) (net 5 "COLL"))
  )
)
"#
    )
}

/// Every finding of the board's lint as (check, severity, message with the
/// ground's spelling replaced by `GND`), so two spellings compare equal when
/// they say the same thing.
fn findings(ground: &str, base_ohms: &str) -> Vec<(LintCheck, Severity, String)> {
    let board = ExtractedBoard::from_auto(&stage(ground, base_ohms)).expect("parse board");
    let lint = hauksbee_engine::checks::engine_lint(&board, &ModelLibrary::builtin());
    let leaf = ground.rsplit('/').next().unwrap_or(ground);
    let mut out: Vec<_> = lint
        .findings
        .iter()
        .map(|f| (f.check, f.severity, f.message.replace(leaf, "GND")))
        .collect();
    out.sort_by(|a, b| a.2.cmp(&b.2));
    out
}

/// The 47 ohm base drive is a real overstress of the 74HC04 output; it is found
/// under every spelling of the ground, with the same words.
#[test]
fn a_real_overstress_is_found_whatever_the_ground_is_called() {
    let control = findings("GND", "47");
    assert!(
        control
            .iter()
            .any(|(check, sev, _)| *check == LintCheck::DcOverstress && *sev == Severity::High),
        "the control board must carry the overstress: {control:#?}"
    );
    for ground in SPELLINGS {
        assert_eq!(
            findings(ground, "47"),
            control,
            "{ground}: the same board must say the same things"
        );
    }
}

/// The legal stage (10k base resistor) is quiet under every spelling: no
/// overstress, and nothing gating that the `GND` spelling does not say.
#[test]
fn a_legal_board_stays_quiet_whatever_the_ground_is_called() {
    let control = findings("GND", "10k");
    assert!(
        control
            .iter()
            .all(|(check, sev, _)| *check != LintCheck::DcOverstress || *sev != Severity::High),
        "the legal control must not overstress: {control:#?}"
    );
    for ground in SPELLINGS {
        assert_eq!(
            findings(ground, "10k"),
            control,
            "{ground}: renaming the ground must change no finding"
        );
    }
}
