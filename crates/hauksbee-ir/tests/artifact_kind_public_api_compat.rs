//! Downstream exhaustive-match compatibility for the planned first release.

use hauksbee_ir::evidence::ArtifactKind;

fn legacy_kind_name(kind: ArtifactKind) -> &'static str {
    match kind {
        ArtifactKind::KiCadPcb => "kicad_pcb",
        ArtifactKind::KiCadSchematic => "kicad_schematic",
        ArtifactKind::KiCadNetlist => "kicad_netlist",
        ArtifactKind::EagleBoard => "eagle_board",
        ArtifactKind::AltiumPcbDoc => "altium_pcb_doc",
        ArtifactKind::GerberArchive => "gerber_archive",
        ArtifactKind::OdbPlusPlus => "odb_plus_plus",
        ArtifactKind::Ipc2581 => "ipc_2581",
        ArtifactKind::Ipc356 => "ipc_356",
        ArtifactKind::BoardCode => "board_code",
        ArtifactKind::Bom => "bom",
        ArtifactKind::Placement => "placement",
        ArtifactKind::Elf => "elf",
        ArtifactKind::IntelHex => "intel_hex",
        ArtifactKind::Toml => "toml",
    }
}

#[test]
fn existing_exhaustive_artifact_kind_matches_still_compile() {
    assert_eq!(legacy_kind_name(ArtifactKind::EagleBoard), "eagle_board");
}
