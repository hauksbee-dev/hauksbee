//! A MOSFET the board names only by class (KiCad's generic `Q_PMOS_GSD` with no
//! part number) binds a set of ASSUMED threshold and on-resistance corners, and
//! CI sweeps them: a high-side switch passes only if it works at every corner.
//! A single typical device passed a gate drive the fitted part may not honour.

use crate::support::{self, comp, pad};

const R0805: &str = "Resistor_SMD:R_0805_2012Metric";

/// A P-channel high-side switch from a 5 V rail to a 1 k load: the gate is pulled
/// up to the rail by 10 k and driven through 100 R from a logic net held at `vg`.
fn run(vg: f64) -> hauksbee_ci::CiResult {
    let q1 = pad(1, "G") + &pad(2, "VCC") + &pad(3, "OUT");
    let mut b = comp("Q1", "Package_TO_SOT_SMD:SOT-23", "Q_PMOS_GSD", 10, q1);
    for (i, (r, val, a, k)) in [
        ("RL", "1k", "OUT", "GND"),
        ("RG", "100", "IN", "G"),
        ("RGS", "10k", "G", "VCC"),
    ]
    .into_iter()
    .enumerate()
    {
        b += &comp(r, R0805, val, 40 + i * 30, pad(1, a) + &pad(2, k));
    }
    let nets = "net \"G\"\nnet \"OUT\"\nnet \"GND\"\nnet \"VCC\"\nnet \"IN\"\n";
    let p = support::write_tmp(
        "pmos-corners.board",
        &format!("board version 1\nfn main {{\n{nets}{b}}}\n"),
    );
    let spec = format!(
        "name = \"generic pmos at {vg} V\"\nboard = {}\nduration_ms = 3\n[[supply]]\nnet = \"VCC\"\n\
         kind = \"ideal\"\nvolts = 5.0\n[[net_drive]]\nnet = \"IN\"\nvolts = {vg}\n[[assert]]\n\
         kind = \"voltage\"\nname = \"load\"\nnet = \"OUT\"\nmin = 4.5\nmax = 5.5\nafter_ms = 2\n",
        support::toml_path(&p)
    );
    support::run_body(&format!("pmos-corners-{vg}.toml"), &spec)
}

#[test]
fn a_generic_mosfet_passes_only_if_it_works_at_every_assumed_corner() {
    // Gate driven to 0 V: |VGS| about 5 V clears the envelope's 3.0 V maximum
    // threshold and its 4.5 V RDS(on) drive, so the load holds at both corners.
    let on = run(0.0);
    let row = support::row(&on, "load");
    assert_eq!(on.seeds, 2, "both corners are simulated: {row:?}");
    assert!(row.passed && !row.invalid, "{row:?}");
    let json = on.render_json();
    assert!(
        json.contains("generic_pmos_sot_corner_low_vth"),
        "sweep names the corners"
    );
    assert!(
        json.contains("generic_pmos_sot_corner_high_vth"),
        "sweep names the corners"
    );

    // Gate driven to 2.5 V: |VGS| = 2.5 V is below the envelope's 3.0 V maximum
    // threshold. A single typical device (|VGS(th)| 1.5 V) switched on and the
    // load held; at the high-threshold corner the switch stays off, and at the
    // low one it conducts on a gate drive no datasheet guarantees. Never a pass.
    let marginal = run(2.5);
    let row = support::row(&marginal, "load");
    assert_eq!(marginal.seeds, 2, "{row:?}");
    assert!(!row.passed, "a marginal gate drive is not a pass: {row:?}");
    let human = marginal.render_human().to_lowercase();
    assert!(
        human.contains("high_vth") || human.contains("maximum vgs(th)"),
        "the report names why: {human}"
    );
}
