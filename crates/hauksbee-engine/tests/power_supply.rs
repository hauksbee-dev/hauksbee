//! Feature 1 tests: configurable power supplies.
//!
//! Each test binds a tiny synthetic board (a resistive load across +5V→GND),
//! swaps the +5V net's supply, runs the co-sim headless, and checks the rail
//! behaves as the supply model dictates (CC foldback, battery depletion, USB
//! droop).

use hauksbee_engine::power_supply::{Chemistry, PowerSupply, UsbSpec};
use hauksbee_engine::{bind_board, HauksbeeEngine};
use hauksbee_extract::ExtractedBoard;
use hauksbee_models::ModelLibrary;

/// A minimal board: one resistor `R1` from +5V to GND. The resistor value is
/// substituted in so each test can pick its own load. Nets: 1 GND, 2 +5V.
fn load_board(rload_ohms: &str) -> String {
    format!(
        r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+5V")
  (module Resistor_THT:R_Axial_DIN0207_L6.3mm_D2.5mm (layer F.Cu)
    (at 100 100)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value {rload_ohms} (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (net 2 "+5V"))
    (pad 2 thru_hole circle (at 2 0) (net 1 "GND"))
  )
)
"#
    )
}

/// Build a scheduler-backed engine over a load board, with no firmware.
fn engine_for(rload_ohms: &str) -> HauksbeeEngine {
    let text = load_board(rload_ohms);
    HauksbeeEngine::from_board_file(&text, None, "/boards/load.kicad_pcb").expect("build engine")
}

/// Run `n` chunks of `dt` and return the settled +5V rail voltage and the
/// supply's last rail current.
fn settle(engine: &mut HauksbeeEngine, dt: f64, n: usize) -> (f64, f64) {
    use hauksbee_server::engine::Engine;
    let mut last_v = 0.0;
    for _ in 0..n {
        let frame = engine.step(dt);
        last_v = *frame.net_voltages.get("+5V").unwrap_or(&0.0);
    }
    let cur = engine
        .scheduler()
        .supply_states()
        .get("+5V")
        .map(|(_, i, _)| *i)
        .unwrap_or(0.0);
    (last_v, cur)
}

#[test]
fn default_supply_is_ideal_5v() {
    // With no reconfiguration, the +5V rail should hold ~5 V into a 1k load.
    let mut engine = engine_for("1k");
    let (v, _) = settle(&mut engine, 1e-4, 50);
    assert!((v - 5.0).abs() < 0.05, "ideal rail = {v:.3} V, want ~5.0");
}

#[test]
fn bench_supply_current_limit_foldback() {
    // 5 V bench supply, 0.5 A limit, into a 1 Ω load. Unlimited current would be
    // 5 A; CC foldback must clamp it to ~0.5 A and drop the rail voltage.
    let text = load_board("1");
    let board = ExtractedBoard::from_auto(&text).expect("parse");
    let lib = ModelLibrary::builtin();
    let bound = bind_board(&board, &lib);
    // The +5V supply net must be detected.
    assert!(
        bound.supplies.iter().any(|s| s.net_name == "+5V"),
        "supply nets: {:?}",
        bound
            .supplies
            .iter()
            .map(|s| &s.net_name)
            .collect::<Vec<_>>()
    );

    let mut engine = engine_for("1");
    let set = engine.scheduler_mut().set_power_supply(
        "+5V",
        PowerSupply::Bench {
            volts: 5.0,
            current_limit_a: 0.5,
        },
    );
    assert!(set, "bench supply applied to +5V");

    let (v, i) = settle(&mut engine, 1e-4, 200);
    // Current clamps to the 0.5 A limit within 5%.
    assert!(
        (i - 0.5).abs() <= 0.05 * 0.5,
        "rail current {i:.4} A not within 5% of 0.5 A limit (V={v:.3})"
    );
    // The rail voltage folds down toward I*Rload = 0.5 V (well below 5 V).
    assert!(
        v < 1.0,
        "rail folded back to {v:.3} V (want < 1 V under CC)"
    );
}

#[test]
fn usb_droop_under_load() {
    // USB 5V/0.5A spec into a modest load that stays under the limit: terminal
    // voltage should droop below 5 V by roughly I * droop_R but stay near 5 V.
    let mut engine = engine_for("50"); // ~0.1 A draw, under 0.5 A
    engine.scheduler_mut().set_power_supply(
        "+5V",
        PowerSupply::Usb {
            spec: UsbSpec::V5_0_5A,
        },
    );
    let (v, i) = settle(&mut engine, 1e-4, 200);
    assert!(i > 0.05 && i < 0.5, "USB draw {i:.3} A under limit");
    // Droop resistance for 0.5 A spec is 0.5 Ω; ~0.1 A → ~0.05 V droop.
    assert!(
        v < 5.0 && v > 4.7,
        "USB terminal drooped to {v:.3} V (want slightly under 5 V)"
    );

    // Now overload it: 2 Ω load wants 2.5 A, far over the 0.5 A limit. The hard
    // foldback must collapse the rail and clamp current near the limit.
    let mut engine2 = engine_for("2");
    engine2.scheduler_mut().set_power_supply(
        "+5V",
        PowerSupply::Usb {
            spec: UsbSpec::V5_0_5A,
        },
    );
    let (v2, i2) = settle(&mut engine2, 1e-4, 300);
    assert!(
        i2 <= 0.5 * 1.10,
        "USB overload current {i2:.3} A should be held near 0.5 A limit (V={v2:.3})"
    );
    assert!(v2 < 2.0, "USB rail collapsed to {v2:.3} V under overload");
}

#[test]
fn battery_soc_depletes_at_expected_rate() {
    // 1-cell Li-ion, 100 mAh, into a load that draws ~1 A. Over 18 s of sim time
    // that is 1 A * 18 s = 18 As = 0.005 Ah = 5% of 0.1 Ah, so SoC should fall
    // from 1.0 to ~0.95.
    //
    // Pick the load so the *terminal* current is ~1 A. Li-ion 1 cell at full SoC
    // is ~4.2 V; with r_internal 0.05 Ω, a 4.15 Ω load draws ~1 A.
    let mut engine = engine_for("4.15");
    engine.scheduler_mut().set_power_supply(
        "+5V",
        PowerSupply::Battery {
            chemistry: Chemistry::LiIon,
            cells: 1,
            capacity_mah: 100.0,
            soc: 1.0,
            r_internal_ohms: 0.05,
            protection: None,
        },
    );

    // Step 18 s of sim time in 1 ms chunks.
    let dt = 1e-3;
    let n = 18_000;
    let (_v, i) = settle(&mut engine, dt, n);
    // Sanity: the load actually drew on the order of ~1 A.
    assert!(
        (0.8..=1.2).contains(&i),
        "battery delivered {i:.3} A (want ~1 A); load sizing"
    );

    let soc = engine
        .scheduler()
        .supply_states()
        .get("+5V")
        .map(|(_, _, s)| *s)
        .unwrap_or(1.0);
    // Charge drained ≈ i * 18 s / 3600 / 0.1 Ah. Compute the expected SoC from
    // the *actual* average current to avoid coupling to the exact load math.
    let expected_drop = i * 18.0 / 3600.0 / 0.1;
    let expected_soc = 1.0 - expected_drop;
    assert!(
        (soc - expected_soc).abs() < 0.02,
        "battery SoC {soc:.4} not within 2% of expected {expected_soc:.4} (I={i:.3} A)"
    );
    assert!(
        soc < 1.0 && soc > 0.90,
        "SoC {soc:.4} depleted into ~0.95 band"
    );
}
