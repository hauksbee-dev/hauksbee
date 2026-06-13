//! Probe: CC attach classifier + double-termination audit on a board.
//! Usage: cc_classify <board-file>
use hauksbee_engine::checks::usb_c::*;
use hauksbee_extract::ExtractedBoard;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).expect("board file");
    let text = std::fs::read_to_string(&path)?;
    let board = if path.ends_with(".kicad_sch") {
        ExtractedBoard::from_kicad_schematic_path(std::path::Path::new(&path))?
    } else { ExtractedBoard::from_auto(&text)? };
    match audit_cc_termination(&board) {
        None => println!("AUDIT: no USB-C receptacle CC nets found"),
        Some(a) => {
            for rec in &a.receptacles {
                println!("receptacle {}:", if rec.reference.is_empty() { "?" } else { &rec.reference });
                for (name, t) in [("CC1", &rec.cc1), ("CC2", &rec.cc2)] {
                    println!("  {name}: ext_rd={:?} int_rd={:?} ({:?}) eff_rd={:?} doubled={}",
                        t.external_rd_ohms, t.internal_rd_ohms, t.controller_ref,
                        t.effective_rd_ohms(), t.is_double_terminated());
                }
            }
            println!("  => has_double_termination = {}", a.has_double_termination());
            println!("  => all_receptacles_terminated = {}", a.all_receptacles_terminated());
        }
    }
    Ok(())
}
