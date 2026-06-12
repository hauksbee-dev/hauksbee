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

#[test]
#[ignore]
fn explore_ltc4020_loaded() {
    use galvani_engine::GalvaniEngine;
    use galvani_server::engine::Engine;
    use galvani_ir::{Device, NodeId, SourceKind};
    let f = famous();
    for (path, label) in [
        ("mnt_reform/reform2-motherboard25-pcb/reform2-motherboard25.kicad_pcb","mb2.5 (R8=100k, faulty)"),
        ("mnt_reform/reform2-motherboard30-pcb/reform2-motherboard30.kicad_pcb","mb3.0 (R8=7.15k, fixed)"),
    ] {
        let p = f.join(path);
        let text = std::fs::read_to_string(&p).unwrap();
        let mut eng = GalvaniEngine::from_board_file(&text, None, "test").unwrap();
        // Operating point: drive the brick rail (VIN) at 19 V, load the charge
        // output (CHGBAT) with a system load that demands more than the input
        // limit allows (a low resistance to ground).
        let sched = eng.scheduler_mut();
        let vin_net = sched.net_nodes.get("/Reform 2 Power/VIN").or_else(|| sched.net_nodes.get("VIN")).copied();
        let bat_net = sched.net_nodes.get("CHGBAT").copied();
        if let Some(vin) = vin_net {
            sched.circuit_mut().add(Device::Vsource{name:"Vbrick_test".into(),p:vin,n:NodeId::GROUND,kind:SourceKind::Dc(19.0)});
        }
        if let Some(bat) = bat_net {
            // Heavy system load: 5 ohm at ~28 V => wants ~5.6 A * 28 V = 157 W,
            // far beyond any brick, so the input limit MUST bind.
            sched.circuit_mut().add(Device::Resistor{name:"Rsysload_test".into(),a:bat,b:NodeId::GROUND,ohms:5.0,tc1:None});
        }
        // relayout via attach (no-op peripheral) - use the public relayout path:
        eng.scheduler_mut().set_behavioral_input_budget("U2", 19.0, 60.0);
        for _ in 0..80 { let _ = eng.step(1e-3); }
        for (refn, st, iin, lim) in eng.scheduler().behavioral_states() {
            if refn=="U2" {
                println!("{label}: iin={:?} A limit={:?} A input~{:.1} W", iin, lim, iin.unwrap_or(0.0)*19.0);
            }
        }
    }
}

#[test]
#[ignore]
fn explore_ltc4020_loaded2() {
    use galvani_engine::scheduler::Scheduler;
    use galvani_ir::{Device, NodeId, SourceKind};
    use galvani_solve::SolverOptions;
    let f = famous();
    for (path, label) in [
        ("mnt_reform/reform2-motherboard25-pcb/reform2-motherboard25.kicad_pcb","mb2.5 (R8=100k, faulty)"),
        ("mnt_reform/reform2-motherboard30-pcb/reform2-motherboard30.kicad_pcb","mb3.0 (R8=7.15k, fixed)"),
    ] {
        let text = std::fs::read_to_string(f.join(path)).unwrap();
        let board = ExtractedBoard::from_auto(&text).unwrap();
        let lib = ModelLibrary::builtin();
        let mut bound = bind_board(&board, &lib);
        let vin = bound.net_nodes.get("/Reform 2 Power/VIN").or_else(|| bound.net_nodes.get("VIN")).copied();
        let bat = bound.net_nodes.get("CHGBAT").copied();
        if let Some(vin)=vin { bound.circuit.add(Device::Vsource{name:"Vbrick_test".into(),p:vin,n:NodeId::GROUND,kind:SourceKind::Dc(19.0)}); }
        if let Some(bat)=bat { bound.circuit.add(Device::Resistor{name:"Rsysload_test".into(),a:bat,b:NodeId::GROUND,ohms:5.0,tc1:None}); }
        let mut sched = Scheduler::new(bound, None, SolverOptions::default()).unwrap();
        sched.set_behavioral_input_budget("U2", 19.0, 60.0);
        for _ in 0..80 { let _ = sched.step(1e-3); }
        for (refn, st, iin, lim) in sched.behavioral_states() {
            if refn=="U2" {
                println!("{label}: state={st} iin={:?} A limit={:?} A input~{:.1} W BAT={:.2} VIN={:.2}",
                    iin, lim, iin.unwrap_or(0.0)*19.0,
                    sched.net_voltage("CHGBAT").unwrap_or(0.0),
                    sched.net_voltage("/Reform 2 Power/VIN").or_else(|| sched.net_voltage("VIN")).unwrap_or(0.0));
            }
        }
    }
}

#[test]
#[ignore]
fn explore_ltc4020_focused() {
    use galvani_engine::scheduler::Scheduler;
    use galvani_ir::{Device, NodeId, SourceKind};
    use galvani_solve::SolverOptions;
    let f = famous();
    for (path, label) in [
        ("mnt_reform/reform2-motherboard25-pcb/reform2-motherboard25.kicad_pcb","mb2.5 (R8=100k, faulty)"),
        ("mnt_reform/reform2-motherboard30-pcb/reform2-motherboard30.kicad_pcb","mb3.0 (R8=7.15k, fixed)"),
    ] {
        let text = std::fs::read_to_string(f.join(path)).unwrap();
        let mut board = ExtractedBoard::from_auto(&text).unwrap();
        // Focus: keep only U2 (the charger) and the programming resistors R8/R49
        // (so their values are read), drop everything else. The behavioural
        // converter does not need the surrounding board to solve — just a driven
        // input and a loaded output.
        board.components.retain(|c| ["U2","R8","R49","R50"].contains(&c.reference.as_str()));
        let lib = ModelLibrary::builtin();
        let mut bound = bind_board(&board, &lib);
        let vin = bound.net_nodes.get("/Reform 2 Power/VIN").or_else(|| bound.net_nodes.get("VIN")).copied();
        let bat = bound.net_nodes.get("CHGBAT").copied();
        eprintln!("{label}: vin_node={:?} bat_node={:?}", vin, bat);
        if let Some(vin)=vin { bound.circuit.add(Device::Vsource{name:"Vbrick_test".into(),p:vin,n:NodeId::GROUND,kind:SourceKind::Dc(19.0)}); }
        if let Some(bat)=bat { bound.circuit.add(Device::Resistor{name:"Rsysload_test".into(),a:bat,b:NodeId::GROUND,ohms:5.0,tc1:None}); }
        let mut sched = Scheduler::new(bound, None, SolverOptions::default()).unwrap();
        sched.set_behavioral_input_budget("U2", 19.0, 60.0);
        for _ in 0..80 { let _ = sched.step(1e-3); }
        for (refn, st, iin, lim) in sched.behavioral_states() {
            if refn=="U2" {
                println!("{label}: state={st} iin={:?} A limit={:?} A input~{:.1} W BAT={:.2} VIN={:.2}",
                    iin, lim, iin.unwrap_or(0.0)*19.0,
                    sched.net_voltage("CHGBAT").unwrap_or(0.0),
                    sched.net_voltage("/Reform 2 Power/VIN").or_else(|| sched.net_voltage("VIN")).unwrap_or(0.0));
            }
        }
    }
}

#[test]
#[ignore]
fn explore_ltc6803_focused() {
    use galvani_engine::scheduler::Scheduler;
    use galvani_ir::{Device, NodeId, SourceKind};
    use galvani_solve::SolverOptions;
    let f = famous();
    for (path, label) in [
        ("mnt_reform/reform2-motherboard-pcb/reform2-motherboard.kicad_pcb","mb2.0 (R52=100, faulty)"),
        ("mnt_reform/reform2-motherboard25-pcb/reform2-motherboard25.kicad_pcb","mb2.5 (R52 absent->diode, fixed)"),
    ] {
        let text = std::fs::read_to_string(f.join(path)).unwrap();
        let mut board = ExtractedBoard::from_auto(&text).unwrap();
        // Find U4 and the net on its V+ (pad 1).
        let u4 = board.components.iter().find(|c| c.reference=="U4").cloned();
        let vplus_net = u4.as_ref().and_then(|c| c.pins.iter().find(|p| p.number=="1").and_then(|p| p.net));
        let vplus_name = vplus_net.and_then(|nid| board.nets.iter().find(|n| n.id==nid)).map(|n| n.name.clone());
        board.components.retain(|c| ["U4","R52"].contains(&c.reference.as_str()));
        let lib = ModelLibrary::builtin();
        let mut bound = bind_board(&board, &lib);
        // Drive V+ (top of pack) to the 8S LiFePO4 stack voltage ~28 V.
        if let Some(name)=&vplus_name {
            if let Some(&node) = bound.net_nodes.get(name) {
                bound.circuit.add(Device::Vsource{name:"Vpack_test".into(),p:node,n:NodeId::GROUND,kind:SourceKind::Dc(28.0)});
            }
        }
        let mut sched = Scheduler::new(bound, None, SolverOptions::default()).unwrap();
        for _ in 0..40 { let _ = sched.step(1e-3); }
        // Read the leak current = the law Isource value. Use behavioral device's
        // converter_iin? No — for a law we expose via the source. Measure the V+
        // node and infer: leak current flows through the law from vplus->vminus.
        // We can read the device's pending state; simplest: print V+ stays driven
        // and report the tie_ohms the binder resolved.
        let leak = sched.behavioral_law_value("U4","absent_cell_leak");
        let r52 = board.components.iter().find(|c| c.reference=="R52").map(|c| c.value.clone());
        println!("{label}: R52={:?} leak_current={:?} A V+={:.2}", r52, leak, sched.net_voltage(vplus_name.as_deref().unwrap_or("")).unwrap_or(0.0));
    }
}

#[test]
#[ignore]
fn explore_npm1300_focused() {
    use galvani_engine::scheduler::Scheduler;
    use galvani_ir::{Device, NodeId, SourceKind};
    use galvani_solve::SolverOptions;
    let f = famous();
    for (path, label) in [
        ("zswatch_devkit/v1.2.0/ZSWatch-Watch-DevKit.kicad_pcb","1.2.0 (SHPHLD wired to GPIO, faulty)"),
        ("zswatch_devkit/v1.2.1/ZSWatch-Watch-DevKit.kicad_pcb","1.2.1 (GPIO removed, fixed)"),
    ] {
        let text = std::fs::read_to_string(f.join(path)).unwrap();
        let board = ExtractedBoard::from_auto(&text).unwrap();
        // Find IC401 SHPHLD (pad 15) net + VSYS (pad 20) net, and which other
        // pins (the MCU GPIO) sit on the SHPHLD net.
        let ic = board.components.iter().find(|c| c.reference=="IC401").unwrap();
        let shphld_net = ic.pins.iter().find(|p| p.number=="15").and_then(|p| p.net);
        let shphld_name = shphld_net.and_then(|nid| board.nets.iter().find(|n| n.id==nid)).map(|n| n.name.clone());
        let vsys_net = ic.pins.iter().find(|p| p.number=="20").and_then(|p| p.net);
        let vsys_name = vsys_net.and_then(|nid| board.nets.iter().find(|n| n.id==nid)).map(|n| n.name.clone());
        // Degree of SHPHLD net (how many pads): >1 besides IC401 = GPIO present.
        let members: Vec<String> = shphld_net.map(|nid| board.components.iter()
            .flat_map(|c| c.pins.iter().map(move |p| (c.reference.clone(), p.number.clone(), p.net)))
            .filter(|(_,_,n)| *n==Some(nid))
            .map(|(r,pn,_)| format!("{r}.{pn}")).collect()).unwrap_or_default();
        println!("{label}:");
        println!("  SHPHLD net = {:?} members={:?}", shphld_name, members);
        // Bind and solve: drive VSYS to 3.7 V, leave the SHPHLD net's GPIO side
        // high-Z (a weak leak to GND). Measure the SHPHLD-net voltage: if the
        // internal pull reaches the GPIO it sits near VSYS (the fault); if the
        // GPIO is disconnected the net still has SHPHLD+pull but no GPIO load.
        let mut board = board;
        board.components.retain(|c| c.reference=="IC401");
        let lib = ModelLibrary::builtin();
        let mut bound = bind_board(&board, &lib);
        if let Some(name)=&vsys_name { if let Some(&n)=bound.net_nodes.get(name) {
            bound.circuit.add(Device::Vsource{name:"Vvsys_test".into(),p:n,n:NodeId::GROUND,kind:SourceKind::Dc(3.7)});
        }}
        // Add a weak GPIO-sleep leak (10M to GND) on the SHPHLD net.
        if let Some(name)=&shphld_name { if let Some(&n)=bound.net_nodes.get(name) {
            bound.circuit.add(Device::Resistor{name:"Rgpio_sleep".into(),a:n,b:NodeId::GROUND,ohms:10e6,tc1:None});
        }}
        let mut sched = Scheduler::new(bound, None, SolverOptions::default()).unwrap();
        for _ in 0..30 { let _ = sched.step(1e-3); }
        let v = shphld_name.as_deref().and_then(|nm| sched.net_voltage(nm)).unwrap_or(f64::NAN);
        println!("  SHPHLD-net voltage in sleep = {:.3} V (VSYS=3.7)", v);
    }
}
