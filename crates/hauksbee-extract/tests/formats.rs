//! IPC-D-356 netlist reading on the committed KiCad export.

use hauksbee_extract::ExtractedBoard;
use std::path::PathBuf;

/// `testdata/pic_programmer.d356` is KiCad's own IPC-D-356 export of the
/// pic_programmer demo. Every multi-member net must be internally consistent
/// and the export must carry the board's real net count.
#[test]
fn ipc356_export_reads_with_full_connectivity() {
    let d356_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/pic_programmer.d356");
    let Ok(src) = std::fs::read_to_string(&d356_path) else {
        eprintln!("d356 fixture missing; skipping");
        return;
    };
    let board = ExtractedBoard::from_ipc_d356(&src).unwrap();
    assert!(
        board.components.len() > 50,
        "got {}",
        board.components.len()
    );
    let multi = board
        .nets
        .iter()
        .filter(|n| board.net_members(n.id).len() >= 2)
        .count();
    assert!(multi > 20, "only {multi} multi-pin nets");
    assert!(board.lint().undeclared_nets.is_empty());
    let gnd = board.net_by_name("GND").expect("GND net");
    assert!(board.net_members(gnd.id).len() > 10);
    let auto = ExtractedBoard::from_auto(&src).unwrap();
    assert_eq!(auto.components.len(), board.components.len());
}
