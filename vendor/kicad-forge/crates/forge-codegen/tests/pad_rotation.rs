//! A footprint placed at 90/270 degrees must compile to pads that carry that
//! rotation. KiCad stores a pad's angle as its ABSOLUTE board orientation, and a
//! pad's `size` is in the pad's own frame, so a compiled board that writes a zero
//! angle under a rotated footprint draws every fine-pitch pad turned a quarter
//! turn: neighbouring pads then overlap and read as copper shorts.

use forge_model::Pcb;
use hauksbee_forge_codegen::dsl::{to_code, Program};

const ROTATED_FINE_PITCH: &str = r#"board version 20241229
fn main {
    net "A"
    net "B"
    comp J1 lib "Connector:Fine" val "J" layer "F.Cu" at 100 50 rot 270 {
        pad "1" smd rect at -0.5 -4 size 0.3 1.45 layers [F.Cu F.Paste F.Mask] net "A"
        pad "2" smd rect at 0 -4 size 0.3 1.45 layers [F.Cu F.Paste F.Mask] net "B"
        pad "3" smd rect at 0.5 -4 size 0.3 1.45 rot 90 layers [F.Cu F.Paste F.Mask] nonet
    }
}
"#;

fn pad_angles(pcb: &Pcb) -> Vec<f64> {
    pcb.footprints()[0]
        .pads()
        .iter()
        .map(|p| p.at().2)
        .collect()
}

#[test]
fn a_rotated_footprints_pads_carry_its_rotation_in_the_compiled_board() {
    let pcb = Program::parse(ROTATED_FINE_PITCH).expect("parses").build();
    // Pads 1 and 2 follow the footprint (270); pad 3 is turned 90 more (0).
    assert_eq!(pad_angles(&pcb), vec![270.0, 270.0, 0.0]);
}

#[test]
fn an_unrotated_footprint_still_writes_zero_pad_angles() {
    let text = ROTATED_FINE_PITCH
        .replace("rot 270 {", "rot 0 {")
        .replace(" rot 90", "");
    let pcb = Program::parse(&text).expect("parses").build();
    assert_eq!(pad_angles(&pcb), vec![0.0, 0.0, 0.0]);
}

#[test]
fn decompiling_keeps_only_the_pad_rotation_that_differs_from_the_footprint() {
    let pcb = Program::parse(ROTATED_FINE_PITCH).expect("parses").build();
    let code = to_code(&pcb);
    // Only the one genuinely turned pad carries a `rot`; the pads that merely
    // follow their footprint stay terse, as every shipped example already is.
    let pad_lines: Vec<&str> = code
        .lines()
        .filter(|l| l.trim().starts_with("pad "))
        .collect();
    assert_eq!(pad_lines.len(), 3, "{code}");
    assert_eq!(
        pad_lines.iter().filter(|l| l.contains(" rot ")).count(),
        1,
        "{code}"
    );
    // And it round-trips to the same absolute angles.
    let again = Program::parse(&code).expect("re-parses").build();
    assert_eq!(pad_angles(&again), pad_angles(&pcb));
}
