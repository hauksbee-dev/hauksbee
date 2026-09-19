//! An ADJUSTABLE regulator carries no output setpoint: an LM317 holds
//! `V(OUT) - V(ADJ)` at its 1.25 V reference and sources IADJ out of ADJ, so the
//! BOARD's R1/R2 set the rail. The stamp is three primitives the IR already has
//! (the reference, a CCCS carrying the pass current onto IN, IADJ out of ADJ),
//! which makes `VOUT = VREF*(1 + R2/R1) + IADJ*R2` a SOLVED value and the input
//! headroom the part needs solved with it rather than read off the card.
//!
//! What that owes: the rail matches the datasheet equation to the millivolt; an
//! input that cannot clear rail + dropout REFUSES in the one regulator wording
//! instead of printing a rail; and an ADJ pin with no DC path to ground is never
//! answered with a number. The sign convention of the three primitives is
//! settled separately, against ngspice: `hauksbee-solve/tests/decks/lm317_adj.cir`.

use crate::support;

/// U1 (LM317, ADJ/OUT/IN on pads 1/2/3), R1 = 240 from OUT to ADJ, R2 from ADJ
/// to ground and an `rl` load, in the board DSL. `r2 = None` omits R2, leaving
/// ADJ with no DC path to ground at all; `rl = None` omits the load, leaving the
/// divider as the only draw.
fn board(r2: Option<f64>, rl: Option<f64>) -> String {
    // Pads are offset by pad number so that no two of a part's land coincide.
    let pad = |n: u32, net: &str| {
        format!("pad \"{n}\" smd rect at {n} 0 size 2 1.5 layers [F.Cu F.Mask] net \"{net}\"\n")
    };
    let comp = |r: &str, lib: &str, val: &str, x: usize, pads: String| {
        format!(
            "comp {r} lib \"{lib}\" val \"{val}\" layer \"F.Cu\" at {x} 20 rot 0 {{\n{pads}}}\n"
        )
    };
    let u1 = pad(1, "ADJ") + &pad(2, "VOUT") + &pad(3, "VIN");
    let mut parts = comp(
        "U1",
        "Package_TO_SOT_SMD:SOT-223-3_TabPin2",
        "LM317",
        20,
        u1,
    );
    for (i, (r, val, a, b)) in [
        ("R1", Some(240.0), "VOUT", "ADJ"),
        ("R2", r2, "ADJ", "GND"),
        ("RL", rl, "VOUT", "GND"),
    ]
    .into_iter()
    .enumerate()
    {
        let Some(val) = val else { continue };
        let lib = "Resistor_SMD:R_0805_2012Metric";
        parts += &comp(r, lib, &val.to_string(), 40 + i * 8, pad(1, a) + &pad(2, b));
    }
    format!("board version 20171130\nfn main {{\nnet \"VIN\"\nnet \"VOUT\"\nnet \"GND\"\nnet \"ADJ\"\n{parts}}}\n")
}

#[test]
fn an_adjustable_regulator_solves_its_rail_and_refuses_what_it_cannot_support() {
    // The LM317 card's figures (TI SLVS044Y): VREF = 1.25 V, IADJ = 50 uA, and a
    // 3 V minimum input-to-output differential. The assertion band is +/-1 mV
    // around the datasheet equation, so a PASS is that equation and not merely a
    // plausible 5 V.
    let vout = 1.25 * (1.0 + 720.0 / 240.0) + 50.0e-6 * 720.0;
    let dir = tempfile::tempdir().expect("tempdir");
    // `starved` is the SLVS044Y minimum-load-current disclosure (10 mA max spec):
    // the divider alone draws 1.25/240 = 5.21 mA, the 500R load adds 10.07 mA.
    for (case, r2, rl, vin, supported, starved) in [
        ("legal control", Some(720.0), Some(500.0), 12.0, true, false),
        ("divider only", Some(720.0), None, 12.0, true, true),
        // 5.036 V of rail plus 3 V of dropout needs 8.036 V, and 6 V is not it.
        ("low headroom", Some(720.0), Some(500.0), 6.0, false, false),
        ("ADJ floating", None, Some(500.0), 12.0, false, false),
    ] {
        let board_path = dir.path().join(format!("{case}.board"));
        std::fs::write(&board_path, board(r2, rl)).expect("write board");
        let spec = dir.path().join(format!("{case}.toml"));
        std::fs::write(
            &spec,
            format!(
                "name = \"{case}\"\nboard = {}\nduration_ms = 3\n\
                 [[supply]]\nnet = \"VIN\"\nkind = \"bench\"\nvolts = {vin}\n\
                 current_limit_a = 0.5\nr_out_ohms = 0.1\n\
                 [[assert]]\nkind = \"voltage\"\nname = \"vout regulates\"\nnet = \"VOUT\"\n\
                 min = {}\nmax = {}\nafter_ms = 1.0\n",
                support::toml_path(&board_path),
                vout - 1e-3,
                vout + 1e-3
            ),
        )
        .expect("write spec");
        let r = hauksbee_ci::run(&support::run_config(spec)).expect("run the spec");
        let a = r
            .results
            .iter()
            .find(|x| x.label == "vout regulates")
            .unwrap_or_else(|| panic!("{case}: no VOUT assertion in {:?}", r.results));
        if supported {
            assert!(a.passed && !a.invalid, "{case}: {a:?}");
            assert_eq!(r.exit_code(), 0, "{case}: {:?}", r.results);
            // SOLVED, not declared: the rail is the same 5.036 V either way, so
            // only the solved output current can tell these two rows apart.
            let said = r.render_human().contains("requires for regulation");
            assert_eq!(said, starved, "{case}: min-current disclosure");
            continue;
        }
        // Never a number on this net: the run refuses, and the assertion is not
        // green. The headroom case refuses in the shared regulator wording, which
        // must name the part and the rail it could not support.
        assert!(!a.passed && r.exit_code() == 3, "{case}: {a:?}");
        if r2.is_some() {
            let reason = r
                .coverage_warnings
                .iter()
                .find(|w| w.contains(" regulation of "))
                .unwrap_or_else(|| panic!("{case}: {:?}", r.coverage_warnings));
            assert!(reason.contains("U1") && reason.contains("VOUT"), "{reason}");
            assert!(a.invalid && a.detail.contains(reason), "{case}: {a:?}");
        }
    }
}
