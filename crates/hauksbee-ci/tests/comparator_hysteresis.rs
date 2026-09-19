//! An open-collector comparator's thresholds belong to the BOARD. An LM393 has
//! no internal hysteresis and no driver to the high rail, so an inverting
//! Schmitt trigger's two trip points AND both output levels come out of the
//! pull-up and the feedback resistor, solved.
//!
//! One board: Vcc 5 V, `RP` 10 k pull-up on OUT, `R1`/`R2` 10 k from VCC and
//! GND to IN+, `RF` 100 k from IN+ back to OUT, a 1 V/ms up-then-down PWL ramp
//! on IN-. Closed form, with OUT loaded by RF so high is NOT Vcc:
//!
//! ```text
//! OUT high = 1100/230 = 4.782609 V  ->  VTH_rising  = 11*OUT - 50 = 2.608696 V
//! OUT low  = 0.016962 V (RP vs RON) ->  VTH_falling = (50+OUT)/21 = 2.381760 V
//! ```
//!
//! ngspice-47 on the same topology measures 2.60952 / 2.38196 V off the same
//! ramp; this model, bisected on a 2 mV sample grid, answers 2.612 / 2.382 V
//! with levels 4.783 / 0.017 V, so the closed form is the bar for all four and
//! every band below is set from it, not from what the model happened to say.
//!
//! Each row brackets one threshold the way `timer555.rs` brackets an interval:
//! the ramp makes time a proxy for input volts, and the level bands are
//! themselves the open-collector claim. The last two rows are the fault: the
//! SAME board with RF removed has ONE threshold, the bare divider's 2.5 V.

use crate::support;

/// The inverting Schmitt trigger above; `rf = None` removes the feedback
/// resistor, which is the fault case.
fn board(rf: Option<&str>) -> String {
    let pad = |n: u32, net: &str| {
        format!("pad \"{n}\" smd rect at {n} 0 size 2 1.5 layers [F.Cu F.Mask] net \"{net}\"\n")
    };
    let comp = |r: &str, val: &str, x: usize, lib: &str, pads: String| {
        format!(
            "comp {r} lib \"{lib}\" val \"{val}\" layer \"F.Cu\" at {x} 20 rot 0 {{\n{pads}}}\n"
        )
    };
    // LM393 DIP-8: 1 OUT_A, 2 IN-_A, 3 IN+_A, 4 GND, 5 IN+_B, 6 IN-_B, 7 OUT_B,
    // 8 VCC. Section B is the UNUSED one, tied off the way a board ties off a
    // spare comparator: inputs strapped to the rails, output on its own net
    // with no pull-up. It must bind, and it must not make the board refuse.
    let u1: String = ["OUT", "VIN", "NP", "GND", "GND", "VCC", "OUT_B", "VCC"]
        .iter()
        .enumerate()
        .map(|(i, n)| pad(i as u32 + 1, n))
        .collect();
    let mut parts = comp("U1", "LM393", 20, "Package_DIP:DIP-8_W7.62mm", u1);
    for (i, (r, val, a, b)) in [
        ("RP", Some("10k"), "VCC", "OUT"),
        ("R1", Some("10k"), "VCC", "NP"),
        ("R2", Some("10k"), "NP", "GND"),
        ("RF", rf, "NP", "OUT"),
    ]
    .into_iter()
    .enumerate()
    {
        let Some(val) = val else { continue };
        let lib = "Resistor_SMD:R_0805_2012Metric";
        parts += &comp(r, val, 40 + i * 8, lib, pad(1, a) + &pad(2, b));
    }
    let nets = ["VCC", "GND", "OUT", "NP", "VIN", "OUT_B"]
        .map(|n| format!("net \"{n}\"\n"))
        .concat();
    format!("board version 20171130\nfn main {{\n{nets}{parts}}}\n")
}

/// Run one window of the ramp and report whether OUT held the expected level
/// over it. The run ends AT the window, so `after_ms` makes the assertion read
/// exactly `[after_ms, end_ms]`; the input there is `t` volts on the way up and
/// `10 - t` on the way down.
fn out_is(case: &str, rf: Option<&str>, after_ms: f64, end_ms: f64, high: bool) -> bool {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("schmitt.board");
    std::fs::write(&path, board(rf)).expect("write board");
    // The high level is WHATEVER THE BOARD MAKES IT -- the open-collector claim:
    // 4.7826 V when RF loads the pull-up, 5.000 V when it does not. A push-pull
    // stage answers its own `out_hi` both times and fails one. Low is VOL = RON
    // * I_sink = 16.96 mV, not an ideal 0 V.
    let hi = [(4.95, 5.01), (4.70, 4.86)][usize::from(rf.is_some())];
    let (min, max) = if high { hi } else { (0.006, 0.05) };
    let r = support::run_body(
        &format!("{case}.toml"),
        &format!(
            "name = \"{case}\"\nboard = {}\nduration_ms = {end_ms}\nframe_ms = 0.005\n\
             [[supply]]\nnet = \"VCC\"\nkind = \"ideal\"\nvolts = 5.0\n\
             [[peripheral]]\nid = \"RAMP\"\ntype = \"stimulus\"\nnet = \"VIN\"\n\
             waveform = \"pwl\"\npwl = [[0.0, 0.0], [5.0, 5.0], [10.0, 0.0]]\n\
             [[assert]]\nname = \"out\"\nkind = \"voltage\"\nnet = \"OUT\"\n\
             after_ms = {after_ms}\nmin = {min}\nmax = {max}\n",
            support::toml_path(&path),
        ),
    );
    // Both sections must have bound whatever the level answers: a tied-off half
    // reading as unmodelled makes every row a claim the report does not back.
    let cov = &r.coverage_warnings;
    assert!(!cov.iter().any(|w| w.contains("U1")), "{case}: {cov:?}");
    // The card says response time is not modelled; that has to REACH the reader
    // rather than sit in a TOML comment. `warning` -> `[partial model]` ->
    // `Assumption` is the path every other partial model uses.
    let human = r.render_human().to_lowercase();
    assert!(
        human.contains("modelled only in part") && human.contains("response time"),
        "{case}: the LM393's unmodelled response time never reached the report"
    );
    r.results
        .iter()
        .find(|x| x.label == "out")
        .unwrap_or_else(|| panic!("{case}: no out result in {:?}", r.results))
        .passed
}

#[test]
fn a_comparator_gets_its_hysteresis_and_its_levels_from_the_board() {
    // (case, RF, after_ms, end_ms, expect high). The two no-feedback rows sit
    // INSIDE the hysteretic board's 227 mV band and answer the OPPOSITE of the
    // rows above them: that opposition is the hysteresis.
    for (case, rf, after_ms, end_ms, high) in [
        // VTH_rising = 2.608696 V: high through 2.56 V, low from 2.66 V.
        ("rising below vth", Some("100k"), 2.30, 2.56, true),
        ("rising above vth", Some("100k"), 2.66, 2.72, false),
        // VTH_falling = 2.381760 V: low down to 2.45 V, high from 2.35 V.
        ("falling above vth", Some("100k"), 7.30, 7.55, false),
        ("falling below vth", Some("100k"), 7.65, 7.72, true),
        // No feedback resistor: ONE threshold, at the bare divider's 2.5 V.
        ("no feedback rising", None, 2.56, 2.62, false),
        ("no feedback falling", None, 7.55, 7.62, true),
    ] {
        let want = if high { "high" } else { "at VOL" };
        assert!(
            out_is(case, rf, after_ms, end_ms, high),
            "{case}: OUT was not {want} over [{after_ms}, {end_ms}] ms"
        );
    }
}
