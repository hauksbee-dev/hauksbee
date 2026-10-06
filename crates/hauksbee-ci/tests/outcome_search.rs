//! Outcome-driven fault finding: a spec states what the board is FOR (this
//! comparator fires at 0.387 V; its output is a valid logic high at the gate it
//! drives) and a `[search]` grid of operating conditions, and every violated
//! outcome comes back at its worst condition with the parts that set it ranked
//! and a solved repair value.
//!
//! One board: an LMV7219 comparing a 0-1 V ramp on VIN against an 820k/R2
//! divider from VCC, its output divided by RA/RB into a 74LVC1G04 input. With
//! R2 = 150k the threshold sits at 0.773 V, twice the intent, and with
//! RA = RB = 10k the gate sees half of the 4.95 V output, under its VIH. The
//! repaired board (R2 = 68k, RB = 47k) holds both outcomes and says nothing.

use crate::support::{self, comp, pad};

fn board(r2: &str, rb: &str) -> String {
    // LMV7219 SOT-23-5: 1 OUT, 2 GND, 3 +IN, 4 -IN, 5 V+.
    let u1: String = ["OUT", "GND", "VIN", "TH", "VCC"]
        .iter()
        .zip(1..)
        .map(|(n, i)| pad(i, n))
        .collect();
    // 74LVC1G04 SOT-23-5: 1 NC, 2 A, 3 GND, 4 Y, 5 VCC.
    let u2: String = ["NC", "X", "GND", "NY", "VCC"]
        .iter()
        .zip(1..)
        .map(|(n, i)| pad(i, n))
        .collect();
    let sot = "Package_TO_SOT_SMD:SOT-23-5";
    let mut parts = comp("U1", sot, "LMV7219", 10, u1) + &comp("U2", sot, "74LVC1G04", 20, u2);
    for (i, (r, val, a, b)) in [
        ("R1", "820k", "VCC", "TH"),
        ("R2", r2, "TH", "GND"),
        ("RA", "10k", "OUT", "X"),
        ("RB", rb, "X", "GND"),
    ]
    .into_iter()
    .enumerate()
    {
        let lib = "Resistor_SMD:R_0805_2012Metric";
        parts += &comp(r, lib, val, 40 + i * 8, pad(1, a) + &pad(2, b));
    }
    let nets = ["VCC", "GND", "OUT", "TH", "VIN", "X", "NY", "NC"]
        .map(|n| format!("net \"{n}\"\n"))
        .concat();
    format!("board version 20171130\nfn main {{\n{nets}{parts}}}\n")
}

fn run(case: &str, r2: &str, rb: &str) -> hauksbee_ci::CiResult {
    let path = support::write_tmp("outcome.board", &board(r2, rb));
    support::run_body(
        &format!("{case}.toml"),
        &format!(
            "name = \"{case}\"\nboard = {}\nduration_ms = 2\n\
             [[net_drive]]\nnet = \"VCC\"\nvolts = 5.0\n\
             [[peripheral]]\nid = \"RAMP\"\ntype = \"stimulus\"\nnet = \"VIN\"\n\
             waveform = \"pwl\"\npwl = [[0.0, 0.0], [2.0, 1.0]]\n\
             [search]\n[[search.sweep]]\nnet = \"VCC\"\nvolts = [4.75, 5.0]\n\
             [[assert]]\nname = \"trips\"\nkind = \"fires\"\nref = \"U1\"\n\
             target = 0.387\ntolerance = 0.1\n\
             [[assert]]\nname = \"gate\"\nkind = \"logic_level\"\nnet = \"X\"\n",
            support::toml_path(&path),
        ),
    )
}

fn why(r: &hauksbee_ci::CiResult, name: &str) -> String {
    let row = support::row(r, name);
    assert!(!row.passed, "{name} must fail: {}", r.render_human());
    format!("{} {}", row.detail, row.why.clone().unwrap_or_default())
}

#[test]
fn a_missed_threshold_names_its_worst_condition_culprits_and_repair() {
    let r = run("outcome threshold", "150k", "47k");
    assert_eq!(r.exit_code(), 1, "{}", r.render_human());
    assert!(support::row(&r, "gate").passed, "{}", r.render_human());

    // 5 V * 150/970 = 0.773 V; the worst corner is the higher supply, and the
    // divider's bottom leg solves toward 820k * 0.387 / 4.613 = 68.8k and stops
    // once the rerun lands inside the 10% tolerance.
    let trips = why(&r, "trips");
    assert!(trips.contains("first tripping at 0.77"), "{trips}");
    assert!(trips.contains("VCC=5V"), "{trips}");
    assert!(
        trips.contains("R2 150k -> 7") || trips.contains("R2 150k -> 6"),
        "{trips}"
    );
    assert!(trips.contains("meets the intent"), "{trips}");
    assert!(trips.contains("R1"), "{trips}");
}

#[test]
fn a_weak_logic_high_names_the_divider_that_starves_it() {
    // Half of the 4.95 V output is under the inverter's VIH at 5 V. The
    // operating-envelope check flags the same undefined input, so the verdict
    // is INVALID rather than RED; the outcome row still carries the search.
    let r = run("outcome gate", "68k", "10k");
    assert!(!r.passed(), "{}", r.render_human());
    assert!(support::row(&r, "trips").passed, "{}", r.render_human());
    let gate = why(&r, "gate");
    assert!(gate.contains("X peaks at 2.4"), "{gate}");
    assert!(gate.contains("U2"), "{gate}");
    assert!(gate.contains("RB") && gate.contains("RA"), "{gate}");
}

/// A board-DSL board from `(ref, footprint, value, nets)` rows.
fn parts_board(parts: &[(&str, &str, &str, &[&str])]) -> String {
    let mut nets: Vec<&str> = parts.iter().flat_map(|p| p.3.iter().copied()).collect();
    nets.sort_unstable();
    nets.dedup();
    let body: String = (parts.iter().enumerate())
        .map(|(i, &(r, lib, val, pins))| {
            let pads = pins.iter().zip(1..).map(|(n, k)| pad(k, n)).collect();
            comp(r, lib, val, 10 + i * 8, pads)
        })
        .collect();
    let nets = nets
        .iter()
        .map(|n| format!("net \"{n}\"\n"))
        .collect::<Vec<_>>()
        .concat();
    format!("board version 20171130\nfn main {{\n{nets}{body}}}\n")
}

const R0805: &str = "Resistor_SMD:R_0805_2012Metric";

#[test]
fn a_driven_rail_reaches_the_parts_that_read_their_levels_from_it() {
    // A 74LVC1G66 on a rail whose NAME states no voltage, and a 2:1 divider
    // off the same rail. Only the spec's drive (swept by the search) says
    // what the rail is, so the switch's control threshold must be read at
    // the swept level, not at a name-guessed default.
    let board = parts_board(&[
        (
            "U1",
            "Package_TO_SOT_SMD:SOT-353_SC-70-5",
            "SN74LVC1G66DCKR",
            &["A", "B", "GND", "CTL", "AVDD"],
        ),
        ("R1", R0805, "20k", &["AVDD", "TH"]),
        ("R2", R0805, "10k", &["TH", "GND"]),
        ("R3", R0805, "10k", &["CTL", "GND"]),
        ("R4", R0805, "10k", &["B", "GND"]),
    ]);
    let path = support::write_tmp("rail.board", &board);
    let spec = support::load_spec(
        "rail.toml",
        &format!(
            "name = \"rail\"\nboard = {}\nduration_ms = 0.1\n\
             [[net_drive]]\nnet = \"AVDD\"\nvolts = 5.0\n\
             [search]\n[[search.sweep]]\nnet = \"AVDD\"\nvolts = [3.0, 5.0]\n\
             [[assert]]\nkind = \"voltage\"\nnet = \"TH\"\n",
            support::toml_path(&path),
        ),
    );
    let runs = hauksbee_ci::runner::run_spec(&spec).expect("spec runs");
    let mut seen: Vec<(f64, f64)> = runs
        .iter()
        .map(|o| {
            let rail = o.probe.conditions[0].value;
            let th = o.windows[&("TH".to_string(), 0f64.to_bits())].max_v;
            assert!((th - rail / 3.0).abs() < 0.01, "TH {th} V at {rail} V");
            (rail, o.probe.switches["CTL"][0].1)
        })
        .collect();
    // The card's VIL/VIH midpoint: 2.50 V at 5 V, about 1.42 V at 3 V; an
    // unresolved rail reads the 2.31 V fallback at every level.
    seen.sort_by(|a, b| a.0.total_cmp(&b.0));
    assert_eq!(seen.len(), 2, "{seen:?}");
    assert!((seen[0].1 - 1.42).abs() < 0.03, "{seen:?}");
    assert!((seen[1].1 - 2.5).abs() < 0.03, "{seen:?}");
}

#[test]
fn a_pulse_must_be_high_enough_for_long_enough_at_the_input_it_drives() {
    // A 50 us 5 V pulse through R1 into C1 drives a 74LVC1G04 input (VIH
    // 3.5 V at 5 V); the outcome is VIH held for the width the gate needs.
    let run = |farads: &str, ms: f64| {
        let board = parts_board(&[
            (
                "U2",
                "Package_TO_SOT_SMD:SOT-23-5",
                "74LVC1G04",
                &["NC", "X", "GND", "NY", "VCC"],
            ),
            ("R1", R0805, "1k", &["IN", "X"]),
            (
                "C1",
                "Capacitor_SMD:C_0805_2012Metric",
                farads,
                &["X", "GND"],
            ),
        ]);
        let path = support::write_tmp("pulse.board", &board);
        support::run_body(
            "pulse.toml",
            &format!(
                "name = \"pulse\"\nboard = {}\nduration_ms = 0.2\nframe_ms = 0.002\n\
                 [[net_drive]]\nnet = \"VCC\"\nvolts = 5.0\n\
                 [[peripheral]]\nid = \"P\"\ntype = \"stimulus\"\nnet = \"IN\"\n\
                 waveform = \"pwl\"\npwl = [[0.0, 0.0], [0.01, 0.0], [0.0101, 5.0], \
                 [0.06, 5.0], [0.0601, 0.0]]\n\
                 [search]\n[[search.sweep]]\nnet = \"VCC\"\nvolts = [4.5, 5.0]\n\
                 [[assert]]\nname = \"held\"\nkind = \"logic_level\"\nnet = \"X\"\n\
                 min_width_ms = {ms}\n",
                support::toml_path(&path),
            ),
        )
    };
    // tau 1 us: X sits near 5 V for ~50 us, so a 20 us need holds silently.
    let r = run("1n", 0.02);
    assert!(support::row(&r, "held").passed, "{}", r.render_human());
    // tau 47 us: the pulse ends before X reaches VIH (too low). tau 10 us: X
    // is past VIH for ~41 us, short of a 45 us need (too short). Both name
    // the RC that sets it.
    for (farads, ms, says) in [
        ("47n", 0.02, "for at most 0.0000 ms"),
        ("10n", 0.045, "in 0 pulse(s)"),
    ] {
        let w = why(&run(farads, ms), "held");
        assert!(w.contains(says) && w.contains("long enough"), "{w}");
        assert!(w.contains("VCC=5V"), "{w}");
        assert!(w.contains("C1") && w.contains("R1"), "{w}");
    }
}

#[test]
fn a_board_that_meets_its_outcomes_passes_silently() {
    let r = run("outcome repaired", "68k", "47k");
    assert!(r.passed(), "{}", r.render_human());
    assert!(
        r.results.iter().all(|row| row.why.is_none()),
        "{}",
        r.render_human()
    );
}
