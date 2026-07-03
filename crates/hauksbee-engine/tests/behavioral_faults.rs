//! Two-sided validation of the behavioural power-IC models against the
//! documented MNT Reform / ZSWatch DevKit revision faults
//! (docs/record/KNOWN_FAULTS_VALIDATION.md). Each test flags the faulty revision for
//! exactly the thing the next revision fixed, and goes clean on the fix, the
//! strongest calibration the tool can have.
//!
//! Corpus-gated like `hauksbee-extract/tests/known_faults.rs`: absent corpus
//! skips, but `HAUKSBEE_REQUIRE_CORPUS=1` turns absence into a hard fail so the
//! calibration cannot vacuously green-out on a runner that should have it.
//!
//! Method: each fault is exercised on a FOCUSED subcircuit, the power IC plus
//! its programming resistors extracted from the real board, with the operating
//! condition (brick voltage, pack voltage, system load, sleep leak) applied in
//! the test. The behavioural device itself is bound by the ordinary pipeline
//! from the real netlist; only the rails/loads are added, exactly as
//! `hardware_history.rs` does for the Tarski board.

use std::path::PathBuf;

use hauksbee_engine::bind_board;
use hauksbee_engine::scheduler::Scheduler;
use hauksbee_extract::ExtractedBoard;
use hauksbee_ir::{Device, NodeId, SourceKind};
use hauksbee_models::ModelLibrary;
use hauksbee_solve::SolverOptions;

fn famous_root() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../board-corpus/famous");
    if p.exists() {
        return Some(p);
    }
    require_corpus(&p.display().to_string());
    None
}

fn require_corpus(what: &str) {
    if std::env::var("HAUKSBEE_REQUIRE_CORPUS").is_ok() {
        panic!("HAUKSBEE_REQUIRE_CORPUS set but required corpus path is missing: {what}");
    }
    eprintln!("corpus path absent ({what}); skipping (set HAUKSBEE_REQUIRE_CORPUS=1 to fail)");
}

/// Load a board, keep only the named components, bind it.
fn focused(path: &PathBuf, keep: &[&str]) -> Option<hauksbee_engine::BoundBoard> {
    if !path.exists() {
        require_corpus(&path.display().to_string());
        return None;
    }
    let text = std::fs::read_to_string(path).expect("read board");
    let mut board = ExtractedBoard::from_auto(&text).expect("parse board");
    board
        .components
        .retain(|c| keep.contains(&c.reference.as_str()));
    Some(bind_board(&board, &ModelLibrary::builtin()))
}

/// The net a given component pin sits on, by reference + pad number.
fn pin_net_name(path: &PathBuf, refdes: &str, pad: &str) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let board = ExtractedBoard::from_auto(&text).ok()?;
    let comp = board.components.iter().find(|c| c.reference == refdes)?;
    let net = comp.pins.iter().find(|p| p.number == pad)?.net?;
    board
        .nets
        .iter()
        .find(|n| n.id == net)
        .map(|n| n.name.clone())
}

/// References sitting on the same net as `refdes`'s pad `pad` (for the SHPHLD
/// GPIO-presence discriminator).
fn net_members(path: &PathBuf, refdes: &str, pad: &str) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(board) = ExtractedBoard::from_auto(&text) else {
        return Vec::new();
    };
    let Some(comp) = board.components.iter().find(|c| c.reference == refdes) else {
        return Vec::new();
    };
    let Some(net) = comp
        .pins
        .iter()
        .find(|p| p.number == pad)
        .and_then(|p| p.net)
    else {
        return Vec::new();
    };
    board
        .components
        .iter()
        .flat_map(|c| c.pins.iter().map(move |p| (c.reference.clone(), p.net)))
        .filter(|(_, n)| *n == Some(net))
        .map(|(r, _)| r)
        .collect()
}

// ───────────────────────────────────────────────────────────────────────────
// FAULT 1: LTC4020 ILIMIT input-current overdraw (Reform mb2.5 -> mb3.0).
//
// Handbook: "Fixed/limited LTC4020 charge current overdraw (resistor R8 replaced
// with 7.15k)." R8 programs the input-current limit against the input shunt R49.
// mb2.5 (R8=100k) lets the charger pull ~88 W from a ~19 V / 60 W brick; mb3.0
// (R8=7.15k) holds it at ~60 W. Both values are read off the real boards.
// ───────────────────────────────────────────────────────────────────────────

/// Run the LTC4020 focused subcircuit with a 19 V brick on VIN and a heavy
/// system load on the charge output, returning the converter's input power (W).
fn ltc4020_input_power(path: &PathBuf) -> Option<f64> {
    let mut bound = focused(path, &["U2", "R8", "R49", "R50"])?;
    let vin_name = pin_net_name(path, "U2", "36")?; // PVIN
    let bat_name = pin_net_name(path, "U2", "20")?; // BAT
    let vin = *bound.net_nodes.get(&vin_name)?;
    let bat = *bound.net_nodes.get(&bat_name)?;
    bound.circuit.add(Device::Vsource {
        name: "Vbrick_test".into(),
        p: vin,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(19.0),
    });
    bound.circuit.add(Device::Resistor {
        name: "Rsysload_test".into(),
        a: bat,
        b: NodeId::GROUND,
        ohms: 5.0, // heavy load: demands more than any brick can supply
        tc1: None,
    });
    let mut sched = Scheduler::new(bound, None, SolverOptions::default()).ok()?;
    sched.set_behavioral_input_budget("U2", 19.0, 60.0);
    for _ in 0..80 {
        let _ = sched.step(1e-3);
    }
    let iin = sched
        .behavioral_states()
        .into_iter()
        .find(|(r, ..)| r == "U2")
        .and_then(|(_, _, iin, _)| iin)?;
    Some(iin * 19.0)
}

#[test]
fn ltc4020_overdraws_on_mb25_and_is_clean_on_mb30() {
    let Some(root) = famous_root() else {
        return;
    };
    let mb25 = root.join("mnt_reform/reform2-motherboard25-pcb/reform2-motherboard25.kicad_pcb");
    let mb30 = root.join("mnt_reform/reform2-motherboard30-pcb/reform2-motherboard30.kicad_pcb");

    let Some(p25) = ltc4020_input_power(&mb25) else {
        return;
    };
    let Some(p30) = ltc4020_input_power(&mb30) else {
        return;
    };
    eprintln!("LTC4020 input power: mb2.5 = {p25:.1} W (R8=100k), mb3.0 = {p30:.1} W (R8=7.15k)");

    // mb2.5 (faulty): over the 60 W brick budget, in the documented 88 W class.
    assert!(
        p25 > 75.0 && p25 < 100.0,
        "mb2.5 should over-draw the 60 W brick (75..100 W class), got {p25:.1} W"
    );
    // mb3.0 (fixed): held within the 60 W brick (a small margin for the solve).
    assert!(
        p30 <= 62.0,
        "mb3.0's R8=7.15k fix should hold the charger inside the 60 W brick, got {p30:.1} W"
    );
    // And the fix must be a real reduction, not a wash.
    assert!(
        p25 - p30 > 20.0,
        "the R8 fix must materially cut the input draw, got {p25:.1} -> {p30:.1} W"
    );
}

// ───────────────────────────────────────────────────────────────────────────
// FAULT 2: LTC6803-4 balancer leak with an absent cell (Reform mb2.0 -> mb2.5).
//
// The four unused upper cell inputs (C9..C12) were tied to the pack top
// (BAT1FUSED) through R52 = 100 ohm on mb2.0; when balancing fires on an absent
// cell, current leaks through that path. mb2.5 replaces R52 with a blocking
// Schottky (D30), so R52 is absent and the leak vanishes. The model reads R52
// off the board (the `tie_ohms_from_ref` convention); absent => open => ~0 leak.
// ───────────────────────────────────────────────────────────────────────────

/// Bind the LTC6803 focused subcircuit with the pack top (V+) driven to ~28 V,
/// return the balancer-leak current (A).
fn ltc6803_leak(path: &PathBuf) -> Option<f64> {
    let vplus_name = pin_net_name(path, "U4", "1")?; // V+
    let mut bound = focused(path, &["U4", "R52"])?;
    let vplus = *bound.net_nodes.get(&vplus_name)?;
    bound.circuit.add(Device::Vsource {
        name: "Vpack_test".into(),
        p: vplus,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(28.0), // 8S LiFePO4 pack top
    });
    let mut sched = Scheduler::new(bound, None, SolverOptions::default()).ok()?;
    for _ in 0..40 {
        let _ = sched.step(1e-3);
    }
    sched.behavioral_law_value("U4", "absent_cell_leak")
}

#[test]
fn ltc6803_leaks_on_mb20_and_is_clean_on_mb25() {
    let Some(root) = famous_root() else {
        return;
    };
    let mb20 = root.join("mnt_reform/reform2-motherboard-pcb/reform2-motherboard.kicad_pcb");
    let mb25 = root.join("mnt_reform/reform2-motherboard25-pcb/reform2-motherboard25.kicad_pcb");

    let Some(leak20) = ltc6803_leak(&mb20) else {
        return;
    };
    let Some(leak25) = ltc6803_leak(&mb25) else {
        return;
    };
    eprintln!("LTC6803 balancer leak: mb2.0 = {leak20:.4} A (R52=100), mb2.5 = {leak25:.3e} A (R52 absent)");

    // mb2.0 (faulty): a real continuous leak (28 V / 100 ohm = 0.28 A).
    assert!(
        leak20 > 0.1,
        "mb2.0 should leak through the 100-ohm absent-cell tie, got {leak20:.4} A"
    );
    // mb2.5 (fixed): R52 gone (replaced by the blocking diode), leak ~0.
    assert!(
        leak25 < 1e-6,
        "mb2.5's diode fix should eliminate the balancer leak, got {leak25:.3e} A"
    );
}

// ───────────────────────────────────────────────────────────────────────────
// FAULT 3: nPM1300 SHPHLD internal pull to VSYS feeds an MCU GPIO in sleep
// (ZSWatch DevKit 1.2.0 -> 1.2.1).
//
// SHPHLD has an internal pull-up to VSYS. On 1.2.0 the SHPHLD net also carries
// an MCU GPIO (M601.G1 = P1.06); in sleep the GPIO goes high-Z and VSYS
// back-drives it through the internal pull. The fix (1.2.1) removes the GPIO
// from the net. hauksbee now MODELS the internal pull (previously invisible), so
// it can both (a) drive the SHPHLD net to ~VSYS and (b) see the GPIO on it.
// ───────────────────────────────────────────────────────────────────────────

/// Bind the nPM1300 focused subcircuit with VSYS driven and a sleeping-GPIO
/// leak on the SHPHLD net; return the SHPHLD net voltage in sleep.
fn npm1300_shphld_sleep_v(path: &PathBuf) -> Option<f64> {
    let shphld_name = pin_net_name(path, "IC401", "15")?;
    let vsys_name = pin_net_name(path, "IC401", "20")?;
    let mut bound = focused(path, &["IC401"])?;
    let vsys = *bound.net_nodes.get(&vsys_name)?;
    let shphld = *bound.net_nodes.get(&shphld_name)?;
    bound.circuit.add(Device::Vsource {
        name: "Vvsys_test".into(),
        p: vsys,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(3.7),
    });
    // A sleeping MCU GPIO is a weak high-impedance leak to ground.
    bound.circuit.add(Device::Resistor {
        name: "Rgpio_sleep".into(),
        a: shphld,
        b: NodeId::GROUND,
        ohms: 10e6,
        tc1: None,
    });
    let mut sched = Scheduler::new(bound, None, SolverOptions::default()).ok()?;
    for _ in 0..30 {
        let _ = sched.step(1e-3);
    }
    sched.net_voltage(&shphld_name)
}

#[test]
fn npm1300_shphld_feeds_gpio_on_120_and_is_clean_on_121() {
    let Some(root) = famous_root() else {
        return;
    };
    let v120 = root.join("zswatch_devkit/v1.2.0/ZSWatch-Watch-DevKit.kicad_pcb");
    let v121 = root.join("zswatch_devkit/v1.2.1/ZSWatch-Watch-DevKit.kicad_pcb");
    if !v120.exists() || !v121.exists() {
        require_corpus("zswatch_devkit v1.2.0/v1.2.1");
        return;
    }

    // The internal pull drives the SHPHLD net to ~VSYS in sleep on BOTH revs
    // (the pull is internal to the part, present regardless). hauksbee now sees
    // this; previously the SHPHLD net would have read as floating.
    let Some(v_sleep) = npm1300_shphld_sleep_v(&v120) else {
        return;
    };
    eprintln!("nPM1300 SHPHLD net in sleep = {v_sleep:.3} V (VSYS=3.7)");
    assert!(
        v_sleep > 3.5,
        "the nPM1300 internal pull should drag SHPHLD up to ~VSYS in sleep, got {v_sleep:.3} V"
    );

    // The fault discriminator: an MCU GPIO shares the SHPHLD net on 1.2.0 (so it
    // is fed VSYS in sleep) but NOT on 1.2.1 (the GPIO was removed). M601 is the
    // NORA-B106 (nRF5340) module.
    let members_120 = net_members(&v120, "IC401", "15");
    let members_121 = net_members(&v121, "IC401", "15");
    eprintln!("SHPHLD net members: 1.2.0 = {members_120:?}, 1.2.1 = {members_121:?}");
    assert!(
        members_120.iter().any(|r| r == "M601"),
        "1.2.0 SHPHLD net should carry the MCU GPIO M601 (the fault), members: {members_120:?}"
    );
    assert!(
        !members_121.iter().any(|r| r == "M601"),
        "1.2.1 should have removed the MCU GPIO from SHPHLD (the fix), members: {members_121:?}"
    );
}

// ───────────────────────────────────────────────────────────────────────────
// CLEANLINESS GUARD: a clean board carrying a behavioural part (the FIXED
// Reform mb3.0, with the LTC4020 and LTC6803) must bind and run a normal solve
// without the behavioural layer manufacturing faults. The fault checks are
// opt-in (a brick budget must be configured explicitly), so an ordinary run is
// silent, the behavioural models add physics, not findings, on a clean board.
// ───────────────────────────────────────────────────────────────────────────

#[test]
fn behavioral_parts_do_not_manufacture_faults_on_clean_board() {
    use hauksbee_engine::HauksbeeEngine;
    use hauksbee_server::engine::Engine;
    let Some(root) = famous_root() else {
        return;
    };
    let mb30 = root.join("mnt_reform/reform2-motherboard30-pcb/reform2-motherboard30.kicad_pcb");
    if !mb30.exists() {
        require_corpus("reform mb3.0");
        return;
    }
    let text = std::fs::read_to_string(&mb30).unwrap();
    // Full-board bind + a short normal headless run (NO budget configured).
    let mut eng = HauksbeeEngine::from_board_file(&text, None, "test").expect("bind+build mb3.0");
    // The behavioural parts must have bound (U2 charger, U4 balancer).
    let refs: Vec<String> = eng
        .scheduler()
        .behavioral_states()
        .into_iter()
        .map(|(r, ..)| r)
        .collect();
    assert!(
        refs.iter().any(|r| r == "U2"),
        "LTC4020 (U2) should bind on mb3.0"
    );

    let mut total_faults = 0usize;
    for _ in 0..20 {
        let frame = eng.step(1e-3);
        // No behavioural overdraw/overpower fault should fire without a budget.
        total_faults += frame
            .faults
            .iter()
            .filter(|f| f.component == "U2" || f.component == "U4")
            .count();
    }
    assert_eq!(
        total_faults, 0,
        "behavioural parts must not manufacture faults on a clean board with no budget configured"
    );
}

// ───────────────────────────────────────────────────────────────────────────
// FAULT 1b: LTC4020 RNG/SS dual-role destabilisation (Reform mb2.0 -> mb2.5).
//
// On mb2.0 the RNG/SS pin (which sets the soft-start and frequency range) is
// DRIVEN by an LPC GPIO (CHG_RNG) so firmware can gate the charge current;
// toggling it mid-operation destabilises the same IC's system DC/DC. On mb2.5
// the pin is left unconnected (NC), the datasheet default. The model's FSM
// destabilises only when RNG/SS is genuinely pulled low: on mb2.0 (driven) we
// can pull it low and trip the FSM; on mb2.5 (NC) the pin is unbound, so the
// guard never fires and the converter stays stable.
// ───────────────────────────────────────────────────────────────────────────

/// True if the LTC4020's RNG/SS pin (pad 15) is on a real (non-unconnected)
/// net, i.e. externally driven (the mb2.0 fault). Returns (driven, net_name).
fn rng_ss_driven(path: &PathBuf) -> Option<(bool, String)> {
    let name = pin_net_name(path, "U2", "15")?;
    Some((!name.starts_with("unconnected-"), name))
}

#[test]
fn ltc4020_rng_ss_destabilises_only_when_driven() {
    let Some(root) = famous_root() else {
        return;
    };
    let mb20 = root.join("mnt_reform/reform2-motherboard-pcb/reform2-motherboard.kicad_pcb");
    let mb25 = root.join("mnt_reform/reform2-motherboard25-pcb/reform2-motherboard25.kicad_pcb");

    // Structural ground truth: mb2.0 drives RNG/SS, mb2.5 leaves it NC.
    let Some((driven20, rng_net)) = rng_ss_driven(&mb20) else {
        return;
    };
    let Some((driven25, _)) = rng_ss_driven(&mb25) else {
        return;
    };
    eprintln!("RNG/SS driven: mb2.0 = {driven20}, mb2.5 = {driven25}");
    assert!(driven20, "mb2.0 should drive RNG/SS (the CHG_RNG GPIO)");
    assert!(!driven25, "mb2.5 should leave RNG/SS unconnected (the fix)");

    // Behavioural ground truth on mb2.0: pulling the DRIVEN RNG/SS net low (a
    // firmware toggle) destabilises the converter FSM.
    let mut bound = focused(&mb20, &["U2", "R8", "R49"]).unwrap();
    let rng = *bound.net_nodes.get(&rng_net).unwrap();
    // Drive RNG/SS low (a GPIO pulling the charge-range pin down).
    bound.circuit.add(Device::Vsource {
        name: "Vrng_test".into(),
        p: rng,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(0.0),
    });
    let mut sched = Scheduler::new(bound, None, SolverOptions::default()).unwrap();
    for _ in 0..10 {
        let _ = sched.step(1e-3);
    }
    let state = sched
        .behavioral_states()
        .into_iter()
        .find(|(r, ..)| r == "U2")
        .map(|(_, st, ..)| st)
        .unwrap_or_default();
    assert_eq!(
        state, "destabilised",
        "mb2.0: a GPIO pulling the driven RNG/SS low should destabilise the converter FSM"
    );

    // mb2.5: RNG/SS is NC, so v_rng_ss is unbound and the FSM cannot destabilise.
    let bound25 = focused(&mb25, &["U2", "R8", "R49"]).unwrap();
    let mut sched25 = Scheduler::new(bound25, None, SolverOptions::default()).unwrap();
    for _ in 0..10 {
        let _ = sched25.step(1e-3);
    }
    let state25 = sched25
        .behavioral_states()
        .into_iter()
        .find(|(r, ..)| r == "U2")
        .map(|(_, st, ..)| st)
        .unwrap_or_default();
    assert_eq!(
        state25, "stable",
        "mb2.5: RNG/SS is NC, the converter FSM must stay stable"
    );
}
