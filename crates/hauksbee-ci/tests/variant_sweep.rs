//! A part whose name does not pin one orderable variant is never simulated as
//! whichever sibling sorts first: CI sweeps a same-pinout set and passes only
//! if every member passes, and refuses a different-pinout set until the spec
//! selects one.

use crate::support;

/// A synthetic fixed-LDO family, 3.3 V (A) and 5 V (B); `b_pins` is B's pinout.
fn card(b_pins: &str) -> String {
    let member = |id: char, vout: f64, pins: &str| {
        format!(
            "[[models]]\nid = \"xyz1234{id}\"\nkind = \"vreg\"\nvariant_of = \"XYZ1234\"\n\
             [models.match]\nvalue_re = \"(?i)^XYZ1234\"\n\
             [models.params]\nvout = {vout}\ndropout_v = 0.3\niq_a = 1e-4\n[models.pins]\n{pins}\n\
             [models.variant]\norderable_re = \"^XYZ1234{id}\"\norderable = [\"XYZ1234{id}DBV\"]\n\
             basis = \"p. 3, Ordering Information\"\n"
        )
    };
    let a_pins = "\"1\" = \"in\"\n\"2\" = \"gnd\"\n\"3\" = \"out\"";
    member('A', 3.3, a_pins) + &member('B', 5.0, b_pins)
}

#[test]
fn an_undetermined_variant_set_is_swept_or_refused_never_guessed() {
    let parts = support::comp(
        "U3",
        "Package_TO_SOT_SMD:SOT-23",
        "XYZ1234",
        10,
        support::pad(1, "VIN") + &support::pad(2, "GND") + &support::pad(3, "VOUT"),
    ) + &support::comp(
        "R1",
        "Resistor_SMD:R_0805_2012Metric",
        "1k",
        20,
        support::pad(1, "VOUT") + &support::pad(2, "GND"),
    );
    let board = support::write_tmp(
        "xyz.board",
        &format!("board version 1\nfn main {{\n{parts}}}\n"),
    );
    let same = "\"1\" = \"in\"\n\"2\" = \"gnd\"\n\"3\" = \"out\"";
    let other = "\"1\" = \"out\"\n\"2\" = \"gnd\"\n\"3\" = \"in\"";
    let select = "[[override]]\nref = \"U3\"\nselect = \"xyz1234A\"\n";
    #[rustfmt::skip]
    let rows = [
        // (case, B pinout, max V, spec extra) -> passed, invalid, seeds, detail
        ("every member passes", same, 5.5, "", true, false, 2, "held across 2 seeds"),
        ("the members split", same, 3.4, "", false, false, 2, "seed 0 with U3 as variant xyz1234B"),
        ("pinouts differ", other, 3.4, "", false, true, 1, "Selection required for U3"),
        ("the spec selects", other, 3.4, select, true, false, 1, "settled 3.300V"),
    ];
    for (case, b_pins, max, extra, passed, invalid, seeds, detail) in rows {
        let models = support::write_tmp("xyz.toml", &card(b_pins))
            .parent()
            .map(Into::into);
        let spec = support::write_tmp(
            "spec.toml",
            &format!(
                "name = \"xyz\"\nboard = {}\nduration_ms = 3\n{extra}\
                 [[supply]]\nnet = \"VIN\"\nkind = \"bench\"\nvolts = 9.0\ncurrent_limit_a = 1.0\n\
                 [[assert]]\nkind = \"voltage\"\nname = \"vout\"\nnet = \"VOUT\"\nmin = 3.2\nmax = {max}\nafter_ms = 2\n",
                support::toml_path(&board)
            ),
        );
        let r = hauksbee_ci::run(&hauksbee_ci::RunConfig {
            models_dir: models,
            ..support::run_config(spec)
        })
        .expect(case);
        let row = support::row(&r, "vout");
        assert_eq!(
            (row.passed, row.invalid, r.seeds),
            (passed, invalid, seeds),
            "{case}: {row:?}"
        );
        assert!(row.detail.contains(detail), "{case}: {}", row.detail);
        if invalid {
            assert!(
                row.detail.contains("select=\"xyz1234A\""),
                "{case}: {}",
                row.detail
            );
        }
        if !extra.is_empty() {
            let json = r.render_json();
            assert!(
                json.contains("xyz1234A, selected in the spec"),
                "{case}: provenance row"
            );
        }
    }
}
