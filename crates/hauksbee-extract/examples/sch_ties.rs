//! Print the deliberate net ties a companion Eagle `.sch` declares, and how many
//! of a `.brd`'s copper shorts they qualify. The diagnostic behind the
//! schematic-net-tie tests, in the shape of the other `drc_probe`-style examples.
fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(brd), Some(sch)) = (args.next(), args.next()) else {
        eprintln!("usage: sch_ties <board.brd> <schematic.sch>");
        std::process::exit(2);
    };
    // Lossy, matching the product path: real Eagle schematics in the wild are not
    // all valid UTF-8 (emonTx V3.2.sch is not), and the CLI reads them anyway.
    let sch_text =
        String::from_utf8_lossy(&std::fs::read(&sch).expect("read schematic")).into_owned();
    let ties = hauksbee_extract::declared_net_ties(&sch_text).expect("parse schematic");
    println!("declared ties: {}", ties.len());
    for t in &ties {
        println!("  {} <-> {}: {}", t.net, t.tied_net, t.describe());
    }
    let brd_text = String::from_utf8_lossy(&std::fs::read(&brd).expect("read board")).into_owned();
    let report = hauksbee_extract::ExtractedBoard::drc(&brd_text).expect("drc runs");
    println!("shorts: {}", report.short_count());
    let qualification = report.qualify_with_declared_ties(&sch, &ties);
    println!("qualified: {}", qualification.qualified_count());
    println!("source: {}", qualification.source_summary());
    println!(
        "still gating: {}",
        qualification.undeclared_shorts(&report).count()
    );
    for s in report.shorts() {
        println!(
            "  {}/{} on {} gap {:.4} declared={:?}",
            s.net_a_name,
            s.net_b_name,
            s.layer,
            s.gap_mm,
            qualification.tie_for(s).map(|d| &d.declaration)
        );
    }
}
