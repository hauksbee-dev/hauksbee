//! A Pico module placed with its 3V3 pad on a net nothing else touches (it is
//! powered through VSYS) and a GPIO pad on a net of its own must still solve
//! as a board: the pad drivers draw their output current from the supply pin,
//! and a supply pin with no DC path used to balance that current at `I / gmin`
//! volts (millions of volts), which the activity summary then printed as a
//! reading. A real pad is clamped by its ESD diodes to within a diode drop of
//! the rails, and the activity summary may never show a level the board's
//! sources cannot produce.

use hauksbee_cosim::envelope::Envelope;
use hauksbee_engine::binder::bind_board;
use hauksbee_engine::scheduler::Scheduler;
use hauksbee_extract::{Component, ExtractedBoard};
use hauksbee_models::ModelLibrary;
use hauksbee_solve::SolverOptions;

fn comp(r: &str, value: &str, lib: &str, footprint: &str, pins: &[(&str, i64, &str)]) -> Component {
    crate::support::component(
        r.to_string(),
        value.to_string(),
        lib.to_string(),
        footprint.to_string(),
        pins.iter()
            .map(|(n, net, f)| crate::support::pin(n, Some(*net), f, ""))
            .collect(),
    )
}

/// Pico: GND, VSYS on +5V, GPIO0 on a 1 kOhm load, the 3V3 pad on a net with
/// no other pad (`NC_3V3`), and GPIO1 on a net with no other pad (`NC_GP1`).
fn board() -> ExtractedBoard {
    ExtractedBoard {
        name: "floating-pad".into(),
        nets: vec![
            crate::support::net(1, "GND".into()),
            crate::support::net(2, "NC_3V3".into()),
            crate::support::net(3, "LED".into()),
            crate::support::net(4, "+5V".into()),
            crate::support::net(5, "NC_GP1".into()),
        ],
        components: vec![
            comp(
                "U1",
                "Pico",
                "MCU_RaspberryPi_and_Boards:Pico",
                "Module:RaspberryPi_Pico_Common_THT",
                &[
                    ("3", 1, "GND"),
                    ("36", 2, "3V3"),
                    ("39", 4, "VSYS"),
                    ("1", 3, "GPIO0"),
                    ("2", 5, "GPIO1"),
                ],
            ),
            crate::support::r0402(
                "R1",
                "1k",
                vec![
                    crate::support::passive_pin("1", 3),
                    crate::support::passive_pin("2", 1),
                ],
            ),
        ],
    }
}

#[test]
fn a_pico_with_unconnected_pads_solves_inside_its_source_envelope() {
    let bound = bind_board(&board(), &ModelLibrary::builtin());
    let mcu = bound.mcus.first().expect("the Pico binds an RP2040");
    let mut drivers = mcu.gpio_drivers.clone();
    let mut sched = Scheduler::new(bound, None, SolverOptions::default()).expect("scheduler");

    // The firmware drives every pad it has high: GPIO0 into the load, GPIO1
    // into nothing.
    for drv in drivers.values_mut() {
        drv.set_enabled(&mut sched.circuit, true);
        drv.set_volts(&mut sched.circuit, 3.3);
    }
    for _ in 0..30 {
        sched.step(1e-3);
    }

    assert!(
        sched.analog_valid(),
        "the board must solve: {:?}",
        sched.failed_window_reasons()
    );
    let env = Envelope::of(&sched.circuit).expect("a board of plain sources has an envelope");
    for (net, stat) in &sched.stats {
        assert!(
            stat.min_v >= env.lo && stat.max_v <= env.hi,
            "net {net} reads {:.4} V .. {:.4} V, outside what the sources can produce \
             ({:.1} V to {:.1} V)",
            stat.min_v,
            stat.max_v,
            env.lo,
            env.hi
        );
    }
    // The unconnected 3V3 pad sits at a physical level: through its clamps, not
    // at I/gmin.
    let float = sched.net_voltage("NC_3V3").expect("net exists");
    assert!((-1.0..=6.0).contains(&float), "NC_3V3 settled at {float} V");
}
