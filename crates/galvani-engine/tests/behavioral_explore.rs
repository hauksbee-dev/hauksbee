//! EXPLORATORY (ignored): bind the real Reform / ZSWatch boards and dump what
//! the behavioural layer found, so the validation assertions are grounded.
use std::path::PathBuf;
use galvani_engine::bind_board;
use galvani_extract::ExtractedBoard;
use galvani_models::ModelLibrary;

fn famous() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../board-corpus/famous")
}

fn dump(board_path: PathBuf, label: &str) {
    if !board_path.exists() { eprintln!("MISSING {}", board_path.display()); return; }
    let text = std::fs::read_to_string(&board_path).unwrap();
    let board = ExtractedBoard::from_auto(&text).unwrap();
    let lib = ModelLibrary::builtin();
    let bound = bind_board(&board, &lib);
    println!("\n=== {label}: {} behavioural device(s) ===", bound.behavioral.len());
    for d in &bound.behavioral {
        println!("  {} state={:?} iin_limit={:?}", d.reference, d.state(), d.converter_iin_limit());
    }
    // Show R8 / R49 / R52 values found on the board.
    for r in ["R8","R49","R52","R9"] {
        if let Some(c) = board.components.iter().find(|c| c.reference==r) {
            println!("  board {r} = {:?}", c.value);
        } else { println!("  board {r} = ABSENT"); }
    }
}

#[test]
#[ignore]
fn explore_reform_behavioral() {
    let f = famous();
    dump(f.join("mnt_reform/reform2-motherboard-pcb/reform2-motherboard.kicad_pcb"), "Reform mb2.0");
    dump(f.join("mnt_reform/reform2-motherboard25-pcb/reform2-motherboard25.kicad_pcb"), "Reform mb2.5");
    dump(f.join("mnt_reform/reform2-motherboard30-pcb/reform2-motherboard30.kicad_pcb"), "Reform mb3.0");
}

#[test]
#[ignore]
fn explore_zswatch_behavioral() {
    let f = famous();
    dump(f.join("zswatch_devkit/v1.2.0/ZSWatch-Watch-DevKit.kicad_pcb"), "ZSWatch DevKit 1.2.0");
    dump(f.join("zswatch_devkit/v1.2.1/ZSWatch-Watch-DevKit.kicad_pcb"), "ZSWatch DevKit 1.2.1");
}

#[test]
#[ignore]
fn explore_ltc4020_operating_point() {
    use galvani_engine::GalvaniEngine;
    use galvani_server::engine::Engine;
    let f = famous();
    for (path, label, brick_w) in [
        ("mnt_reform/reform2-motherboard25-pcb/reform2-motherboard25.kicad_pcb","mb2.5",60.0),
        ("mnt_reform/reform2-motherboard30-pcb/reform2-motherboard30.kicad_pcb","mb3.0",60.0),
    ] {
        let p = f.join(path);
        if !p.exists() { eprintln!("MISSING {label}"); continue; }
        let text = std::fs::read_to_string(&p).unwrap();
        let mut eng = GalvaniEngine::from_board_file(&text, None, "test").unwrap();
        // Configure U2's input budget at the ~19V brick.
        eng.scheduler_mut().set_behavioral_input_budget("U2", 19.0, brick_w);
        for _ in 0..50 { let _ = eng.step(1e-3); }
        let states = eng.scheduler().behavioral_states();
        for (refn, st, iin, lim) in &states {
            if refn == "U2" {
                let p_in = iin.unwrap_or(0.0) * 19.0;
                println!("{label} U2: state={st} iin={:?} A limit={:?} A  => input ~{:.1} W (budget {:.0} W)",
                    iin, lim, p_in, brick_w);
            }
        }
    }
}

#[test]
#[ignore]
fn explore_ltc4020_nets() {
    let f = famous();
    let p = f.join("mnt_reform/reform2-motherboard25-pcb/reform2-motherboard25.kicad_pcb");
    let text = std::fs::read_to_string(&p).unwrap();
    let board = ExtractedBoard::from_auto(&text).unwrap();
    // Find U2 and its PVIN(36)/BAT(20)/RNG_SS(15) nets.
    let u2 = board.components.iter().find(|c| c.reference=="U2").unwrap();
    for pin in &u2.pins {
        if ["36","20","15","7","6","5","23","22","25"].contains(&pin.number.as_str()) {
            let net = pin.net.and_then(|nid| board.nets.iter().find(|n| n.id==nid)).map(|n| n.name.clone());
            // how many components on that net?
            let deg = pin.net.map(|nid| board.components.iter().flat_map(|c| &c.pins).filter(|pp| pp.net==Some(nid)).count()).unwrap_or(0);
            println!("U2 pad {} -> net {:?} (degree {})", pin.number, net, deg);
        }
    }
}
