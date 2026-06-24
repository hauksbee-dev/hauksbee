//! Diagnostic: dump DRC shorts with the primitive kinds/nets/layers involved,
//! to root-cause false positives. Usage: drc_probe <board.kicad_pcb> [max]
use hauksbee_extract::drc_from_text;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut a = std::env::args().skip(1);
    let path = a.next().expect("board file");
    let max: usize = a.next().and_then(|s| s.parse().ok()).unwrap_or(20);
    let text = std::fs::read_to_string(&path)?;
    let r = drc_from_text(&text)?;
    println!(
        "clearance_mm={} primitives={} shorts={} clearance_violations={}",
        r.clearance_mm,
        r.primitive_count,
        r.short_count(),
        r.clearance_violations().count()
    );
    // Histogram of short item-kind pairs.
    use std::collections::BTreeMap;
    let mut hist: BTreeMap<String, usize> = BTreeMap::new();
    for f in r.shorts() {
        let key = format!("{:?}-{:?}", f.item_a.kind, f.item_b.kind);
        *hist.entry(key).or_default() += 1;
    }
    println!("short item-kind histogram: {hist:?}");
    println!("--- first {max} shorts ---");
    for f in r.shorts().take(max) {
        println!(
            "{:6?}-{:6?}  {:>10}<->{:<10} {} gap={:.4} @({:.1},{:.1}) owners[{}|{}]",
            f.item_a.kind,
            f.item_b.kind,
            f.net_a_name,
            f.net_b_name,
            f.layer,
            f.gap_mm,
            f.x,
            f.y,
            f.item_a.owner,
            f.item_b.owner
        );
    }
    Ok(())
}
