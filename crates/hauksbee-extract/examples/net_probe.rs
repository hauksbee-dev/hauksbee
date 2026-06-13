//! Dump every (component, pad, pinfunction, pintype) on the named nets.
//! Usage: net_probe <board-file> <net-name-substr> [<more substrs>...]
use hauksbee_extract::ExtractedBoard;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut a = std::env::args().skip(1);
    let path = a.next().expect("board file");
    let needles: Vec<String> = a.map(|s| s.to_ascii_uppercase()).collect();
    let text = std::fs::read_to_string(&path)?;
    let board = if path.ends_with(".kicad_sch") {
        ExtractedBoard::from_kicad_schematic_path(std::path::Path::new(&path))?
    } else {
        ExtractedBoard::from_auto(&text)?
    };
    for net in &board.nets {
        let up = net.name.to_ascii_uppercase();
        if !needles.iter().any(|n| up.contains(n.as_str())) { continue; }
        println!("NET {} (id {})", net.name, net.id);
        for (c, p) in board.net_members(net.id) {
            println!("   {:8} val='{}' fp='{}' pad={} func='{}' type='{}'",
                c.reference, c.value, c.footprint, p.number, p.function, p.kind);
        }
    }
    Ok(())
}
