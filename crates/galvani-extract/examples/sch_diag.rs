//! Cross-validate a schematic-derived netlist against the project's PCB.
//!
//! Usage: `cargo run -p galvani-extract --example sch_diag -- <dir> <stem>`
//! where `<dir>` is relative to `board-corpus` and `<stem>` is the shared file
//! name of the `<stem>.kicad_sch` / `<stem>.kicad_pcb` pair. Set `DETAIL=1`
//! to print PCB nets that split across schematic nets, `MERGE=1` to print
//! schematic nets that merge several PCB nets. A clean board prints zero of
//! each.

use galvani_extract::ExtractedBoard;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;

fn partition(b: &ExtractedBoard) -> usize {
    let mut by: BTreeMap<i64, usize> = BTreeMap::new();
    for c in &b.components {
        for p in &c.pins {
            if let Some(id) = p.net {
                *by.entry(id).or_default() += 1;
            }
        }
    }
    by.values().filter(|&&n| n >= 2).count()
}

fn pin_to_net(b: &ExtractedBoard) -> HashMap<(String, String), i64> {
    let mut m = HashMap::new();
    for c in &b.components {
        for p in &c.pins {
            if let Some(id) = p.net {
                m.insert((c.reference.clone(), p.number.clone()), id);
            }
        }
    }
    m
}

fn main() {
    let corpus = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../board-corpus");
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (dir, stem) = (&args[0], &args[1]);
    let schp = corpus.join(dir).join(format!("{stem}.kicad_sch"));
    let pcbp = corpus.join(dir).join(format!("{stem}.kicad_pcb"));
    let sch = ExtractedBoard::from_kicad_schematic_path(&schp).unwrap();
    let pcb = ExtractedBoard::from_kicad_pcb(&std::fs::read_to_string(&pcbp).unwrap()).unwrap();
    println!(
        "SCH: {} comps, {} nets, {} multi-pin nets",
        sch.components.len(),
        sch.nets.len(),
        partition(&sch)
    );
    println!(
        "PCB: {} comps, {} nets, {} multi-pin nets",
        pcb.components.len(),
        pcb.nets.len(),
        partition(&pcb)
    );
    let sn = pin_to_net(&sch);
    let pn = pin_to_net(&pcb);
    let shared: Vec<_> = sn.keys().filter(|k| pn.contains_key(*k)).cloned().collect();
    println!("shared pins: {}", shared.len());

    let mut p2s: BTreeMap<i64, BTreeSet<i64>> = BTreeMap::new();
    let mut s2p: BTreeMap<i64, BTreeSet<i64>> = BTreeMap::new();
    for k in &shared {
        p2s.entry(pn[k]).or_default().insert(sn[k]);
        s2p.entry(sn[k]).or_default().insert(pn[k]);
    }
    let psplit = p2s
        .iter()
        .filter(|(p, s)| s.len() > 1 && shared.iter().filter(|k| pn[*k] == **p).count() >= 2)
        .count();
    let smerge = s2p
        .iter()
        .filter(|(s, p)| p.len() > 1 && shared.iter().filter(|k| sn[*k] == **s).count() >= 2)
        .count();
    println!("PCB nets that split in SCH: {psplit}");
    println!("SCH nets that merge PCB nets: {smerge}");

    let np: HashMap<i64, &str> = pcb.nets.iter().map(|n| (n.id, n.name.as_str())).collect();
    let ns: HashMap<i64, &str> = sch.nets.iter().map(|n| (n.id, n.name.as_str())).collect();
    if std::env::var("DETAIL").is_ok() {
        for (p, s) in &p2s {
            let mem: Vec<_> = shared.iter().filter(|k| pn[*k] == *p).collect();
            if mem.len() >= 2 && s.len() > 1 {
                println!(
                    "SPLIT PCB {:?} pins {:?} -> SCH {:?}",
                    np.get(p).unwrap_or(&"?"),
                    mem.iter().map(|k| format!("{}-{}", k.0, k.1)).collect::<Vec<_>>(),
                    s.iter().map(|x| ns.get(x).copied().unwrap_or("?")).collect::<Vec<_>>()
                );
            }
        }
    }
    if std::env::var("MERGE").is_ok() {
        for (s, p) in &s2p {
            let mem: Vec<_> = shared.iter().filter(|k| sn[*k] == *s).collect();
            if mem.len() >= 2 && p.len() > 1 {
                println!(
                    "MERGE SCH {:?} pins {:?} -> PCB {:?}",
                    ns.get(s).unwrap_or(&"?"),
                    mem.iter().map(|k| format!("{}-{}", k.0, k.1)).collect::<Vec<_>>(),
                    p.iter().map(|x| np.get(x).copied().unwrap_or("?")).collect::<Vec<_>>()
                );
            }
        }
    }
}
