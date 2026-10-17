//! A non-logic-level MOSFET driven from 3.3 V logic is the classic field
//! failure: VGS sits below the gate voltage its RDS(on) is specified at and may
//! be under VGS(th) max, so the part idles in its linear region and cooks. The
//! same low-side switch must also survive its own inrush: a peak inside the
//! card's PULSED rating is not a continuous-rating fault.

use crate::support::{self, comp, pad};

/// A part, its footprint and the nets its three pads carry. The pad order is
/// package-dependent and NOT a rotation: TO-220-3 is gate-drain-source
/// (Infineon `IRF540NPbF` Rev. 2.1 page 1), SOT-23 is gate-source-drain.
type Q = (&'static str, &'static str, [&'static str; 3]);
const R0805: &str = "Resistor_SMD:R_0805_2012Metric";
const IRF: Q = (
    "IRF540N",
    "Package_TO_SOT_THT:TO-220-3_Vertical",
    ["G", "D", "GND"],
);
const AOS: Q = ("AO3400A", "Package_TO_SOT_SMD:SOT-23", ["G", "GND", "D"]);

/// Q1 as a low-side switch: `load` from the `vcc` rail to its drain, a 100 R
/// gate series off a logic input held at `vg`, a 10 k pulldown and `bulk`
/// across the rail. Returns the lower-cased report and the exit code.
fn run(q: Q, vcc: f64, vg: f64, load: &str, bulk: &str) -> (String, i32) {
    let mut b = comp(
        "Q1",
        q.1,
        q.0,
        10,
        pad(1, q.2[0]) + &pad(2, q.2[1]) + &pad(3, q.2[2]),
    );
    for (i, (r, lib, val, a, k)) in [
        ("RL", "Resistor_SMD:R_2512_6332Metric", load, "VCC", "D"),
        ("RG", R0805, "100", "IN", "G"),
        ("RGS", R0805, "10k", "G", "GND"),
        ("CB", "Capacitor_SMD:C_1210_3225Metric", bulk, "VCC", "GND"),
    ]
    .into_iter()
    .enumerate()
    {
        b += &comp(r, lib, val, 40 + i * 30, pad(1, a) + &pad(2, k));
    }
    let p = support::placement_board(
        "q.kicad_pcb",
        &support::board_text(&["G", "D", "GND", "VCC", "IN"], &b),
    );
    let spec = format!(
        "name = \"{0} at {vg} V\"\nboard = {1}\nduration_ms = 5\n[[supply]]\nnet = \"VCC\"\n\
         kind = \"ideal\"\nvolts = {vcc}\n[[net_drive]]\nnet = \"IN\"\nvolts = {vg}\n[[assert]]\n\
         kind = \"voltage\"\nname = \"d\"\nnet = \"D\"\nmin = 0.0\nmax = {vcc}\n[[assert]]\n\
         kind = \"no_faults\"\n",
        q.0,
        support::toml_path(&p)
    );
    let out = support::run_body(&format!("{}-{vg}.toml", q.0), &spec);
    support::assert_placement_waived(&out);
    (out.render_human().to_lowercase(), out.exit_code())
}

#[test]
fn a_mosfet_conducting_under_its_specified_gate_drive_is_never_a_clean_pass() {
    // (part, rail, gate, load, bulk, exit code, names the gate drive). IRF540N
    // specifies RDS(on) at VGS = 10 V only and its VGS(th) runs to 4.0 V, so
    // 3.3 V is a MEASURED limit violation and 10 V is the tested point itself.
    // AO3400A specifies RDS(on) down to VGS = 2.5 V, so 2.5 V is a clean pass.
    // The loads are sized to stay inside their own 2512 rating: a hot resistor
    // is a real finding, and one here would say nothing about the gate. The
    // start-up peak a bulk cap drives is covered end to end by the c11 deck.
    for (q, vcc, vg, load, bulk, exit, gate) in [
        (IRF, 12.0, 3.3, "220", "100uF", 3, true),
        (IRF, 12.0, 10.0, "220", "100uF", 0, false),
        (AOS, 5.0, 2.5, "220", "220uF", 0, false),
        (AOS, 5.0, 5.0, "220", "220uF", 0, false),
    ] {
        let (human, code) = run(q, vcc, vg, load, bulk);
        let c = format!("{} at {vg} V: {human}", q.0);
        assert_eq!(human.contains("specifies rds(on) at"), gate, "{c}");
        assert!(!human.contains("overcurrent"), "inrush is not a fault; {c}");
        assert_eq!(code, exit, "{c}");
    }
}
