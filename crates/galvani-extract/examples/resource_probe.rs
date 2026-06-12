//! Debug probe for the MCU resource-conflict check: dumps the matched MCU and,
//! per resource-bearing pad, the net and the inferred board function. Re-runnable
//! so the validation/calibration claims are auditable.
//!   cargo run -p galvani-extract --example resource_probe <board-file>
use galvani_extract::ExtractedBoard;

fn main() {
    let path = std::env::args().nth(1).expect("usage: resource_probe <board>");
    let text = std::fs::read_to_string(&path).expect("read board");
    let board = if path.ends_with("kicad_sch") {
        ExtractedBoard::from_kicad_schematic_path(std::path::Path::new(&path)).unwrap()
    } else {
        ExtractedBoard::from_auto(&text).unwrap()
    };
    println!("--- demands ---");
    for (a, b, c, d, e) in galvani_extract::resource_conflict::debug_demands(&board) {
        println!("{a:14} pad={b:5} net={c:28} res={d:10} {e}");
    }
    println!("--- findings ---");
    print!("{}", galvani_extract::render_netlint(&board.resource_conflicts()));
}
