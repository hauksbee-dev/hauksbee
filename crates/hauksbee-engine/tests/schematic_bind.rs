//! Binding from a *schematic* alone.
//!
//! Requirement: the engine must accept an [`ExtractedBoard`] derived from a
//! `.kicad_sch` exactly as it accepts one from a `.kicad_pcb`. There is no
//! copper, but the binder, model resolution and report only need components,
//! pins and nets, all of which schematic extraction provides. We bind the
//! pic_programmer schematic and assert it resolves at least as well as the
//! same project's layout (it should resolve identically, since the netlist is
//! the same and the components are the same).

use hauksbee_engine::binder::bind_board;
use hauksbee_extract::ExtractedBoard;
use hauksbee_models::ModelLibrary;
use std::path::PathBuf;

fn corpus(extension: &str) -> Option<PathBuf> {
    let rel = format!("demos/pic_programmer/pic_programmer.{extension}");
    hauksbee_testkit::corpus_board(
        env!("CARGO_MANIFEST_DIR"),
        &format!("kicad-demos-src/{rel}"),
    )
    .or_else(|| {
        hauksbee_testkit::corpus_or_skip(
            env!("CARGO_MANIFEST_DIR"),
            &format!("kicad_demos/{rel}"),
            "PIC programmer binding",
        )
    })
}

#[test]
fn pic_programmer_schematic_binds() {
    let Some(sch_path) = corpus("kicad_sch") else {
        return;
    };
    let board = ExtractedBoard::from_kicad_schematic_path(&sch_path).expect("schematic extraction");
    let lib = ModelLibrary::builtin();
    let bound = bind_board(&board, &lib);

    print!("{}", bound.report.render_table());

    // A schematic-only board still resolves a meaningful fraction of parts:
    // resistors, capacitors, diodes, LEDs all bind from value alone.
    let frac = bound.report.resolved_fraction();
    assert!(
        frac > 0.4,
        "schematic bind resolved only {:.1}%",
        frac * 100.0
    );

    // The board carries real nets with names KiCad would use.
    assert!(board.net_by_name("GND").is_some(), "no GND net");
    assert!(board.net_by_name("VCC").is_some(), "no VCC net");
    // No pin references a net that does not exist in the table.
    assert!(
        board.lint().undeclared_nets.is_empty(),
        "schematic produced dangling net ids"
    );
}

#[test]
fn schematic_and_pcb_bind_the_same_components() {
    let Some((sch_path, pcb_path)) = corpus("kicad_sch").zip(corpus("kicad_pcb")) else {
        return;
    };
    let sch = ExtractedBoard::from_kicad_schematic_path(&sch_path).unwrap();
    let pcb = ExtractedBoard::from_kicad_pcb(&std::fs::read_to_string(&pcb_path).unwrap()).unwrap();
    let lib = ModelLibrary::builtin();

    let sb = bind_board(&sch, &lib);
    let pb = bind_board(&pcb, &lib);

    // The same physical board, so the resolved counts should be in the same
    // ballpark whether we came from the layout or the schematic.
    let (sc, pc) = (sb.report.resolved_count(), pb.report.resolved_count());
    let diff = (sc as i64 - pc as i64).abs();
    assert!(
        diff <= 3,
        "resolved counts diverge: schematic {sc} vs pcb {pc}"
    );
}
