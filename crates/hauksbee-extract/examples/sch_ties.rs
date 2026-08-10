//! Print the deliberate net ties a companion Eagle `.sch` declares, and how many
//! of a `.brd`'s copper shorts they qualify. The diagnostic behind the
//! schematic-net-tie tests, in the shape of the other `drc_probe`-style examples.
fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(brd), Some(sch)) = (args.next(), args.next()) else {
        eprintln!("usage: sch_ties <board.brd> <schematic.sch>");
        std::process::exit(2);
    };
    let sch_text = std::fs::read_to_string(&sch).expect("read schematic");
    let ties = hauksbee_extract::declared_net_ties(&sch_text).expect("parse schematic");
    println!("declared ties: {}", ties.len());
    for t in &ties {
        println!("  {} <-> {}: {}", t.net, t.tied_net, t.describe());
    }
    let brd_text = std::fs::read_to_string(&brd).expect("read board");
    let mut report = hauksbee_extract::ExtractedBoard::drc(&brd_text).expect("drc runs");
    println!("shorts: {}", report.short_count());
    println!("hint: {:?}", report.tie_declaration_hint.is_some());
    let n = report.qualify_with_declared_ties(&sch, &ties);
    println!("qualified: {n}");
    println!("source: {:?}", report.declared_tie_source);
    println!("still gating: {}", report.undeclared_short_count());
    for s in report.shorts() {
        println!(
            "  {}/{} on {} gap {:.4} declared={:?}",
            s.net_a_name,
            s.net_b_name,
            s.layer,
            s.gap_mm,
            s.declared_tie.as_ref().map(|d| &d.declaration)
        );
    }
}
