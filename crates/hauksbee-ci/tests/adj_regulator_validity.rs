//! An ADJUSTABLE regulator carries no output setpoint: an LM317 holds
//! `V(OUT) - V(ADJ)` at its 1.25 V reference and sources IADJ out of ADJ, so the
//! BOARD's R1/R2 set the rail. The stamp is three primitives the IR already has
//! (the reference, a CCCS carrying the pass current onto IN, IADJ out of ADJ),
//! which makes `VOUT = VREF*(1 + R2/R1) + IADJ*R2` a SOLVED value and the input
//! headroom the part needs solved with it rather than read off the card. A card
//! that maps a GND pin is ground-referenced instead: `V(FB) - V(GND)` is held,
//! so the same resistors give `VOUT = VREF*(1 + R1/R2)`.
//!
//! What that owes: the rail matches the datasheet equation to the millivolt; an
//! input that cannot clear rail + dropout REFUSES in the one regulator wording
//! instead of printing a rail; and an ADJ pin with no DC path to ground is never
//! answered with a number. The sign convention of the primitives is settled
//! separately, against ngspice: `hauksbee-solve/tests/decks/lm317_adj.cir` and
//! `ldo_gnd_adj.cir`.

use crate::support::{self, comp, pad};

/// A synthetic GROUND-referenced adjustable LDO (ADJ/FB, OUT, IN, GND; VREF
/// 0.8 V): the `gnd` pin in its map is what makes it hold `V(FB) - V(GND)`.
const GROUNDED_LDO: &str = r#"[[models]]
id = "gref_ldo_test"
kind = "vreg"
description = "synthetic ground-referenced adjustable LDO"
match = { value_re = "^GREF-LDO$" }
params = { vref = 0.8, dropout_v = 0.3, iq_a = 5.0e-5 }
pins = { "1" = "fb", "2" = "out", "3" = "in", "4" = "gnd" }
ratings = { max_current_a = 0.5, max_voltage_v = 6.0 }
"#;

/// U1 (LM317, ADJ/OUT/IN on pads 1/2/3), R1 from OUT to ADJ, R2 from ADJ
/// to ground and an `rl` load, in the board DSL. `r2 = None` omits R2, leaving
/// ADJ with no DC path to ground at all; `rl = None` omits the load, leaving the
/// divider as the only draw. `grounded` fits [`GROUNDED_LDO`] instead, with its
/// GND on pad 4.
fn board(grounded: bool, r1: f64, r2: Option<f64>, rl: Option<f64>) -> String {
    // Pads are offset by pad number so that no two of a part's land coincide.
    let u1 = pad(1, "ADJ") + &pad(2, "VOUT") + &pad(3, "VIN");
    let (lib, value, u1) = match grounded {
        true => (
            "Package_TO_SOT_SMD:SOT-23-5",
            "GREF-LDO",
            u1 + &pad(4, "GND"),
        ),
        false => ("Package_TO_SOT_SMD:SOT-223-3_TabPin2", "LM317", u1),
    };
    let mut parts = comp("U1", lib, value, 20, u1);
    for (i, (r, val, a, b)) in [
        ("R1", Some(r1), "VOUT", "ADJ"),
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
    // Ground-referenced, 0.8 V over R1 = 31.25k and R2 = 10k is 3.300 V at a 33R
    // (100 mA) load, where the LM317 law reads that divider as 1.056 V. Swapped,
    // the divider IS 1.056 V: the faulty control, whose 3.3 V band must fail
    // while the band on the rail it does produce holds.
    let (lm317, grounded) = ((false, 240.0), (true, 31_250.0));
    let swapped = 0.8 * (1.0 + 10.0 / 31.25);
    let card = support::write_tmp("gref.toml", GROUNDED_LDO);
    let models = card.parent().expect("card directory").to_path_buf();
    // `starved` is the SLVS044Y minimum-load-current disclosure (10 mA max spec):
    // the divider alone draws 1.25/240 = 5.21 mA, the 500R load adds 10.07 mA.
    #[rustfmt::skip]
    let rows = [
        ("legal control", lm317, Some(720.0), Some(500.0), 12.0, vout, Some(vout), false),
        ("divider only", lm317, Some(720.0), None, 12.0, vout, Some(vout), true),
        // 5.036 V of rail plus 3 V of dropout needs 8.036 V, and 6 V is not it.
        ("low headroom", lm317, Some(720.0), Some(500.0), 6.0, vout, None, false),
        ("ADJ floating", lm317, None, Some(500.0), 12.0, vout, None, false),
        ("grounded control", grounded, Some(10e3), Some(33.0), 5.0, 3.3, Some(3.3), false),
        ("grounded swapped", (true, 10e3), Some(31_250.0), Some(33.0), 5.0, 3.3, Some(swapped), false),
        // 3.3 V + 0.3 V of dropout needs 3.6 V: 3.5 V is refused here, and would
        // have cleared the 1.356 V the LM317 law asks of this divider.
        ("grounded low headroom", grounded, Some(10e3), Some(33.0), 3.5, 3.3, None, false),
    ];
    for (case, (gnd, r1), r2, rl, vin, want, rail, starved) in rows {
        let board_path = support::write_tmp(&format!("{case}.board"), &board(gnd, r1, r2, rl));
        let band = |name: &str, v: f64| {
            let (min, max) = (v - 1e-3, v + 1e-3);
            format!("[[assert]]\nkind = \"voltage\"\nname = \"{name}\"\nnet = \"VOUT\"\nmin = {min}\nmax = {max}\nafter_ms = 1.0\n")
        };
        let spec = support::write_tmp(
            &format!("{case}.toml"),
            &format!(
                "name = \"{case}\"\nboard = {}\nduration_ms = 3\n\
                 [[supply]]\nnet = \"VIN\"\nkind = \"bench\"\nvolts = {vin}\n\
                 current_limit_a = 0.5\nr_out_ohms = 0.1\n{}{}",
                support::toml_path(&board_path),
                band("vout regulates", want),
                band("solved rail", rail.unwrap_or(want)),
            ),
        );
        let models_dir = gnd.then(|| models.clone());
        let cfg = hauksbee_ci::RunConfig {
            models_dir,
            ..support::run_config(spec)
        };
        let r = hauksbee_ci::run(&cfg).expect("run the spec");
        let a = support::row(&r, "vout regulates");
        if let Some(rail) = rail {
            // SOLVED, not declared: the rail is the same 5.036 V either way, so
            // only the solved output current can tell these two rows apart. A
            // MEASURED violation of the card's minimum load UNDERMINES the
            // assertion: the band holds, but the run is INVALID, not green.
            let said = r.render_human().contains("requires for regulation");
            assert_eq!(said, starved, "{case}: min-current disclosure");
            let solved = support::row(&r, "solved rail");
            assert!(
                solved.passed && solved.invalid == starved,
                "{case}: {solved:?}"
            );
            let hit = (rail - want).abs() < 1e-9;
            assert!(a.passed == hit && a.invalid == starved, "{case}: {a:?}");
            let code = if starved { 3 } else { i32::from(!hit) };
            assert_eq!(r.exit_code(), code, "{case}: {a:?}");
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

/// An UNDERMINED measurement must say so. The divider-only board violates the
/// LM317's minimum load current, which invalidates any voltage read off VOUT --
/// and a measurement is not exempt: dropping the bounds removes the claim, not
/// the reason the number cannot be trusted.
#[test]
fn a_measurement_is_undermined_by_the_same_evidence_a_bound_is() {
    let board_path = support::write_tmp("starved.board", &board(false, 240.0, Some(720.0), None));
    let spec = support::write_tmp(
        "starved-probe.toml",
        &format!(
            "name = \"starved probe\"\nboard = {}\nduration_ms = 3\n\
             [[supply]]\nnet = \"VIN\"\nkind = \"bench\"\nvolts = 12.0\n\
             current_limit_a = 0.5\nr_out_ohms = 0.1\n\
             [[assert]]\nkind = \"voltage\"\nname = \"vout\"\nnet = \"VOUT\"\n\
             after_ms = 1.0\n",
            support::toml_path(&board_path),
        ),
    );
    let r = hauksbee_ci::run(&support::run_config(spec)).expect("run the spec");
    let a = support::row(&r, "vout");
    assert!(a.invalid && a.measured, "{a:?}");
    assert!(a.detail.starts_with("INVALID evidence"), "{a:?}");
    assert!(
        r.exit_code() == 3,
        "an undermined number cannot exit 0: {a:?}"
    );
    assert!(
        r.render_human().contains("requires for regulation"),
        "the undermining reason must still be stated: {}",
        r.render_human()
    );
}
