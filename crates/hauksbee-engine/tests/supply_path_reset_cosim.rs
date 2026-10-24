//! A processor's supply rail reaches it through a part the co-sim may not
//! model: a load switch, an ideal diode, a Schottky into an LDO.
//!
//! An STM32F103 with a 10 k NRST pull-up to its own rail and a power LED on
//! that rail. When the rail's name states no voltage (`VDD_MCU`) and the part
//! feeding it has no card, nothing powered it: the rail and NRST sat at 0 V,
//! and the LED's diode made that 0 V read as driven, so the core was held in
//! reset for the whole run. The rail now follows the path the static rail
//! analysis walks. A switch whose enable pin is tied low keeps the rail down,
//! and the bind report says which part holds it off. The boot decision needs
//! no emulator, so these run without Renode.

use hauksbee_engine::bind_report::BindReport;
use hauksbee_engine::HauksbeeEngine;
use hauksbee_frontdoor_api::engine::Engine;
use hauksbee_ir::boot::BootOutcome;
use std::fmt::Write as _;

#[derive(Clone)]
struct Part<'a> {
    reference: &'a str,
    value: &'a str,
    footprint: &'a str,
    /// (pad, net, pin function)
    pads: Vec<(&'a str, &'a str, &'a str)>,
}

fn board_text(mcu_rail: &str, extra: &[Part<'_>]) -> String {
    let mut parts = vec![
        Part {
            reference: "U1",
            value: "STM32F103C8T6",
            footprint: "Package_QFP:LQFP-48_7x7mm_P0.5mm",
            pads: vec![
                ("5", "OSC_IN", ""),
                ("6", "OSC_OUT", ""),
                ("7", "NRST", ""),
                ("8", "GND", ""),
                ("9", mcu_rail, ""),
                ("20", "GND", ""),
                ("23", "GND", ""),
                ("24", mcu_rail, ""),
                ("35", "GND", ""),
                ("36", mcu_rail, ""),
                // BOOT0 to ground: boot the user firmware.
                ("44", "GND", ""),
                ("48", mcu_rail, ""),
            ],
        },
        Part {
            reference: "Y1",
            value: "8MHz",
            footprint: "Crystal:Crystal_SMD_3225-4Pin",
            pads: vec![
                ("1", "OSC_IN", ""),
                ("2", "GND", ""),
                ("3", "OSC_OUT", ""),
                ("4", "GND", ""),
            ],
        },
        Part {
            reference: "R3",
            value: "10k",
            footprint: "Resistor:R",
            pads: vec![("1", "NRST", ""), ("2", mcu_rail, "")],
        },
        // A power LED on the processor's rail: a diode the solve counts as
        // drive.
        Part {
            reference: "D9",
            value: "LED",
            footprint: "LED_SMD:LED_0603_1608Metric",
            pads: vec![("1", "LED_K", "K"), ("2", mcu_rail, "A")],
        },
        Part {
            reference: "R9",
            value: "1k",
            footprint: "Resistor:R",
            pads: vec![("1", "LED_K", ""), ("2", "GND", "")],
        },
    ];
    parts.extend(extra.iter().cloned());
    let mut nets: Vec<&str> = vec!["GND"];
    for p in &parts {
        for (_, n, _) in &p.pads {
            if !nets.contains(n) {
                nets.push(n);
            }
        }
    }
    let id = |n: &str| nets.iter().position(|x| *x == n).unwrap() + 1;
    let mut s = String::from("(kicad_pcb (version 20171130) (host pcbnew 5.1.0)\n  (net 0 \"\")\n");
    for n in &nets {
        writeln!(s, "  (net {} \"{n}\")", id(n)).unwrap();
    }
    for (i, p) in parts.iter().enumerate() {
        writeln!(
            s,
            "  (module {} (layer F.Cu)\n    (at {} 100)\n    (fp_text reference {} (at 0 0) (layer F.SilkS))\n    (fp_text value {} (at 0 2) (layer F.Fab))",
            p.footprint,
            100 + 10 * i,
            p.reference,
            p.value
        )
        .unwrap();
        for (j, (pad, net, func)) in p.pads.iter().enumerate() {
            let f = if func.is_empty() {
                String::new()
            } else {
                format!(" (pinfunction \"{func}\")")
            };
            writeln!(
                s,
                "    (pad {pad} smd rect (at 0 {j}) (net {} \"{net}\"){f})",
                id(net)
            )
            .unwrap();
        }
        s += "  )\n";
    }
    s += ")\n";
    s
}

/// The board's power entry: `net` and ground on a two-pin header.
fn connector(net: &str) -> Part<'_> {
    Part {
        reference: "J1",
        value: "Conn_01x02",
        footprint: "Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical",
        pads: vec![("1", net, ""), ("2", "GND", "")],
    }
}

fn pull(r: &'static str, a: &'static str, b: &'static str) -> Part<'static> {
    Part {
        reference: r,
        value: "100k",
        footprint: "Resistor:R",
        pads: vec![("1", a, ""), ("2", b, "")],
    }
}

/// A load switch with no card (VIN, GND, EN, VOUT) from `+3V3` onto `out`.
fn bare_switch(out: &'static str, en: &'static str) -> Part<'static> {
    Part {
        reference: "U2",
        value: "LSW9001",
        footprint: "Package_TO_SOT_SMD:SOT-23-5",
        pads: vec![
            ("1", "+3V3", "VIN"),
            ("2", "GND", "GND"),
            ("3", en, "EN"),
            ("5", out, "VOUT"),
        ],
    }
}

/// The AP2553 load switch, which has a card, from `+3V3` onto `out`.
fn carded_switch(out: &'static str, en: &'static str) -> Part<'static> {
    Part {
        reference: "U2",
        value: "AP2553W6-7",
        footprint: "Package_TO_SOT_SMD:SOT-23-6",
        pads: vec![
            ("1", "+3V3", ""),
            ("2", "GND", ""),
            ("3", en, ""),
            ("6", out, ""),
        ],
    }
}

/// A diode from `VBUS` onto `VSYS`.
fn diode(value: &'static str) -> Part<'static> {
    Part {
        reference: "D1",
        value,
        footprint: "Diode_SMD:D_SMA",
        pads: vec![("A", "VBUS", ""), ("K", "VSYS", "")],
    }
}

/// An ideal-diode controller with no card, from `VBUS` onto `VSYS`.
fn ideal_diode() -> Part<'static> {
    Part {
        reference: "U4",
        value: "IDD7001",
        footprint: "Package_TO_SOT_SMD:SOT-23-6",
        pads: vec![
            ("1", "VBUS", "VIN"),
            ("2", "GND", "GND"),
            ("3", "VBUS", "CE"),
            ("6", "VSYS", "VOUT"),
        ],
    }
}

/// An LDO from `VSYS` onto `out`, enabled from its input.
fn ldo(value: &'static str, out: &'static str) -> Part<'static> {
    Part {
        reference: "U3",
        value,
        footprint: "Package_TO_SOT_SMD:SOT-23-5",
        pads: vec![
            ("1", "VSYS", "VIN"),
            ("2", "GND", "GND"),
            ("3", "VSYS", "EN"),
            ("5", out, "VOUT"),
        ],
    }
}

struct Run {
    rail: f64,
    nrst: f64,
    outcome: BootOutcome,
    report: BindReport,
}

fn run(rail: &str, extra: &[Part<'_>]) -> Option<Run> {
    let fw = hauksbee_testkit::skip_none!(
        crate::support::testdata("firmware/stm32_clock_ready/clock_ready.elf") => None,
        "clock_ready.elf not built"
    );
    let mut engine = HauksbeeEngine::from_board_file(
        &board_text(rail, extra),
        Some(&fw),
        "/boards/supply_path.kicad_pcb",
    )
    .expect("build the supply-path engine");
    engine.scheduler_mut().chunk_s = 1e-3;
    let mut last = None;
    for _ in 0..5 {
        last = Some(engine.step(1e-3));
    }
    engine.scheduler_mut().refresh_execution_evidence();
    let evidence = engine.scheduler().execution_evidence().first()?.clone();
    let frame = last?;
    let v = |n: &str| frame.net_voltages.get(n).copied().unwrap_or(f64::NAN);
    Some(Run {
        rail: v(rail),
        nrst: v("NRST"),
        outcome: evidence.boot.as_ref()?.outcome,
        report: engine.report().clone(),
    })
}

fn assert_runs(name: &str, r: &Run, volts: f64) {
    assert!(
        (r.rail - volts).abs() < 0.05,
        "{name}: rail {:.3} V, want {volts} V",
        r.rail
    );
    assert!(r.nrst > 0.9 * volts, "{name}: NRST {:.3} V", r.nrst);
    assert_eq!(r.outcome, BootOutcome::UserFlash, "{name}");
}

/// The bind row's warning for the assumed `net` rail.
fn rail_warning<'a>(r: &'a Run, net: &str) -> &'a str {
    let reference = format!("RAIL:{net}");
    (r.report.rows.iter())
        .find(|row| row.reference == reference)
        .and_then(|row| row.warning.as_deref())
        .unwrap_or_else(|| panic!("no warning on a {reference} row"))
}

#[test]
fn an_uncarded_load_switch_feeding_the_mcu_rail_lets_the_core_out_of_reset() {
    let Some(r) = run(
        "VDD_MCU",
        &[connector("+3V3"), bare_switch("VDD_MCU", "+3V3")],
    ) else {
        return;
    };
    assert_runs("uncarded switch", &r, 3.3);
    let warning = rail_warning(&r, "VDD_MCU");
    assert!(
        warning.contains("U2 (LSW9001)") && warning.contains("+3V3"),
        "{warning}"
    );
}

#[test]
fn a_carded_load_switch_with_its_enable_pulled_up_lets_the_core_out_of_reset() {
    let Some(r) = run(
        "VDD_MCU",
        &[
            connector("+3V3"),
            carded_switch("VDD_MCU", "LS_EN"),
            pull("R5", "LS_EN", "+3V3"),
        ],
    ) else {
        return;
    };
    assert_runs("carded switch", &r, 3.3);
    // The card's switch is the rail's source: no assumed rail stands in.
    assert!(!r
        .report
        .rows
        .iter()
        .any(|row| row.reference == "RAIL:VDD_MCU"));
}

#[test]
fn a_schottky_from_vbus_into_an_uncarded_ldo_lets_the_core_out_of_reset() {
    let Some(r) = run(
        "VDD_MCU",
        &[
            connector("VBUS"),
            diode("1N5819"),
            ldo("XYZ1234-33", "VDD_MCU"),
        ],
    ) else {
        return;
    };
    // 5 V is past the STM32F103's supply range, so the regulator's output is
    // taken as the common 3.3 V rail under it, and said to be assumed.
    assert_runs("schottky + uncarded LDO", &r, 3.3);
    let warning = rail_warning(&r, "VDD_MCU");
    assert!(
        warning.contains("U3 (XYZ1234-33)") && warning.contains("assumed"),
        "{warning}"
    );
}

#[test]
fn an_uncarded_ideal_diode_into_a_carded_ldo_lets_the_core_out_of_reset() {
    let Some(r) = run(
        "VDD_MCU",
        &[
            connector("VBUS"),
            ideal_diode(),
            ldo("AP2112K-3.3", "VDD_MCU"),
        ],
    ) else {
        return;
    };
    assert_runs("ideal diode + carded LDO", &r, 3.3);
}

#[test]
fn an_uncarded_load_switch_with_enable_tied_low_keeps_the_core_down_and_says_so() {
    let Some(r) = run(
        "VDD_MCU",
        &[connector("+3V3"), bare_switch("VDD_MCU", "GND")],
    ) else {
        return;
    };
    assert!(r.rail.abs() < 0.05, "rail {:.3} V", r.rail);
    assert_ne!(r.outcome, BootOutcome::UserFlash);
    let (net, why) = r
        .report
        .held_off_feeds
        .first()
        .expect("the held-off rail is named");
    assert_eq!(net, "VDD_MCU");
    assert!(why.contains("U2 (LSW9001) is held off"), "{why}");
}
