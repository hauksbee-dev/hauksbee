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

use crate::support::{self, comp, pad};

/// The inverting Schmitt trigger above; `rf = None` removes the feedback
/// resistor, which is the fault case.
fn board(rf: Option<&str>) -> String {
    // LM393 DIP-8: 1 OUT_A, 2 IN-_A, 3 IN+_A, 4 GND, 5 IN+_B, 6 IN-_B, 7 OUT_B,
    // 8 VCC. Section B is the UNUSED one, tied off the way a board ties off a
    // spare comparator: inputs strapped to the rails, output on its own net
    // with no pull-up. It must bind, and it must not make the board refuse.
    let u1: String = ["OUT", "VIN", "NP", "GND", "GND", "VCC", "OUT_B", "VCC"]
        .iter()
        .zip(1..)
        .map(|(n, i)| pad(i, n))
        .collect();
    let mut parts = comp("U1", "Package_DIP:DIP-8_W7.62mm", "LM393", 20, u1);
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
        parts += &comp(r, lib, val, 40 + i * 8, pad(1, a) + &pad(2, b));
    }
    support::board_text(&["VCC", "GND", "OUT", "NP", "VIN", "OUT_B"], &parts)
}

/// Run one window of the ramp and report whether OUT held the expected level
/// over it (the row passed and is not INVALID). The run ends AT the window, so `after_ms` makes the assertion read
/// exactly `[after_ms, end_ms]`; the input there is `t` volts on the way up and
/// `10 - t` on the way down.
fn out_is(case: &str, rf: Option<&str>, after_ms: f64, end_ms: f64, high: bool) -> bool {
    let path = support::placement_board("schmitt.kicad_pcb", &board(rf));
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
    support::row_is_green(support::row(&r, "out"))
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

/// The same discipline one level up, on an op-amp's OUTPUT SWING. An LM358
/// saturates 1.5 V below the voltage actually on its V+ PIN and 20 mV above V-
/// (SLOS068AB, both MAX, at this board's RL = 2 kOhm), so the gain-11 amp in
/// `fixtures/lm358_gain11.kicad_pcb` delivers 4.4 V on a 12 V rail and cannot on a
/// 5 V one. The card used to carry the part's 32 V ABSOLUTE-MAXIMUM SUPPLY as
/// `rail_hi` and the binder never read the supply pins, so this board answered
/// 4.400 V from a 5 V rail -- 8.799 V when asked for 8.8 -- evidence `clean`.
fn opamp_out(case: &str, rail: f64, vsig: f64, band: (f64, f64)) -> bool {
    let board = support::repo("crates/hauksbee-ci/tests/fixtures/lm358_gain11.kicad_pcb");
    let (min, max) = band;
    let r = support::run_body(
        &format!("{case}.toml"),
        &format!(
            "name = \"{case}\"\nboard = {}\nduration_ms = 5\n\
             [[supply]]\nnet = \"V5\"\nkind = \"ideal\"\nvolts = {rail}\n\
             [[net_drive]]\nnet = \"VSIG\"\nvolts = {vsig}\n\
             [[assert]]\nname = \"out\"\nkind = \"voltage\"\nnet = \"OUT\"\n\
             min = {min}\nmax = {max}\n",
            support::toml_path(&board),
        ),
    );
    // The card's coverage note has to REACH the reader: the LM358 carried no
    // `warning` at all, which is why the unbounded answer read as clean.
    let human = r.render_human().to_lowercase();
    assert!(
        human.contains("modelled only in part") && human.contains("output current limit"),
        "{case}: the LM358's coverage note never reached the report"
    );
    support::row_is_green(support::row(&r, "out"))
}

#[test]
fn an_opamp_output_saturates_against_the_voltage_on_its_supply_pins() {
    // (case, V+ rail, VSIG, the band asserted on OUT, whether it PASSES). Gain
    // is 11, so 0.400 V in asks for 4.4 V out and 0.800 V for 8.8 V; the first
    // row is the frozen c04 case, which used to pass on a 5 V rail.
    for (case, rail, vsig, band, want) in [
        ("5V: 4.4 unreachable", 5.0, 0.400, (4.35, 4.45), false),
        ("5V: caps at V+ -1.5", 5.0, 0.400, (3.40, 3.60), true),
        ("12V: 4.4 reached", 12.0, 0.400, (4.35, 4.45), true),
        ("5V: 8.8 ideal caps", 5.0, 0.800, (3.40, 3.60), true),
        ("5V: floor V- +20mV", 5.0, 0.0, (0.015, 0.025), true),
    ] {
        assert_eq!(opamp_out(case, rail, vsig, band), want, "{case} {band:?}");
    }
}

/// The card's VICR ceiling is a run outcome, not a note. With BOTH inputs of a
/// channel above V+ - 1.5 V the LM339 datasheet guarantees NO output state, so
/// an assertion on OUT must not be gradeable however well the solve converged:
/// `operating_limit_violated` -> `Undermined` -> INVALID -> exit 3. The control
/// fixture is the same board with the two dividers swapped so both taps sit
/// inside the range, and it must still come back a plain pass.
#[test]
fn an_input_pair_above_the_common_mode_ceiling_refuses_instead_of_answering() {
    // (case, fixture, exit code). `pair_high` puts IN+ at 4.306 V and IN- at
    // 4.000 V, both above 5.000 - 1.500 = 3.500 V; `in_range` puts them at
    // 0.694 V and 2.500 V. Both are LM339B on a 5 V rail, 10 k pull-up on OUT.
    for (case, fixture, want) in [
        (
            "both inputs above VICR",
            "lm339_vicr_pair_high.kicad_pcb",
            3,
        ),
        ("both inputs in range", "lm339_vicr_in_range.kicad_pcb", 0),
    ] {
        let board = support::repo(&format!("crates/hauksbee-ci/tests/fixtures/{fixture}"));
        let r = support::run_body(
            &format!("{case}.toml"),
            &format!(
                "name = \"{case}\"\nboard = {}\nduration_ms = 5\n\
                 [[supply]]\nnet = \"V5\"\nkind = \"ideal\"\nvolts = 5.0\n\
                 [[assert]]\nname = \"out\"\nkind = \"voltage\"\nnet = \"OUT\"\n\
                 min = 0.0\nmax = 5.0\n",
                support::toml_path(&board),
            ),
        );
        let human = r.render_human();
        assert_eq!(r.exit_code(), want, "{case}: {human}");
        // The card's own `warning` also says "common-mode range", so the row
        // keys on the FINDING's wording, not on the phrase.
        let flagged = human.contains("every input of one channel above its common-mode range");
        if want == 3 {
            assert!(
                human.contains("[INVALID]") && flagged && human.contains("SLCS006Z"),
                "{case}: the refusal must name the limit and its datasheet: {human}"
            );
        } else {
            assert!(
                human.contains("[PASS]") && !flagged,
                "{case}: an in-range pair must not be flagged: {human}"
            );
        }
    }
}

/// A card's sourced supply envelope is judged on the rails the run SOLVED, not
/// only by the static lint: an LM741 on +5 V sits under its 10 V minimum, a
/// serious limit, so the run is INVALID; an LM339B at 31 V is past a Medium
/// limit, which qualifies the pass. The legal control stays silent.
#[test]
fn sourced_supply_envelopes_follow_the_solved_rails() {
    // U1 is a follower on OUT with its offset-null pads on stubs; U2 is idle.
    let pads =
        |nets: &[&str], at: u32| (at..).zip(nets).map(|(i, n)| pad(i, n)).collect::<String>();
    let u1 = pads(&["N1", "OUT", "GND", "VN", "N5", "OUT", "VP"], 1);
    let u2 = pads(&["VCC", "VCC", "GND", "GND"], 2) + &pad(12, "GND");
    // The rails come in on J1, as on a real board: without it nothing on the
    // board feeds U1's supply pads and the static check rightly fails it.
    let j1 = pads(&["VP", "VN", "VCC", "GND"], 1);
    let parts = comp("U1", "Package_DIP:DIP-8_W7.62mm", "LM741", 20, u1)
        + &comp("U2", "Package_DIP:DIP-14_W7.62mm", "LM339B", 40, u2)
        + &comp(
            "J1",
            "Connector_PinHeader_2.54mm:PinHeader_1x04_P2.54mm_Vertical",
            "Conn_01x04",
            60,
            j1,
        );
    let board = support::board_text(&["VP", "VN", "VCC", "GND", "OUT", "N1", "N5"], &parts);
    let path = support::placement_board("envelopes.kicad_pcb", &board);
    for (case, vp, vcc, net, want, says) in [
        ("under minimum", 5.0, 5.0, "OUT", 3, "the 10..18 V"),
        ("legal control", 12.0, 5.0, "OUT", 0, ""),
        ("medium limit", 12.0, 31.0, "VCC", 0, "the 2..30 V"),
    ] {
        let rails = [("VP", vp), ("VN", -12.0), ("VCC", vcc)]
            .map(|(n, v)| format!("[[supply]]\nnet = \"{n}\"\nkind = \"ideal\"\nvolts = {v}\n"));
        let r = support::run_body(
            &format!("{case}.toml"),
            &format!(
                "name = \"{case}\"\nboard = {}\nduration_ms = 1\n{}[[assert]]\nname = \"a\"\n\
                 kind = \"voltage\"\nnet = \"{net}\"\nmin = -20.0\nmax = 40.0\n",
                support::toml_path(&path),
                rails.concat(),
            ),
        );
        let (human, flagged) = (r.render_human(), !says.is_empty());
        assert_eq!(r.exit_code(), want, "{case}: {human}");
        assert_eq!(human.contains("[INVALID]"), want == 3, "{case}: {human}");
        assert_eq!(human.contains("outside the"), flagged, "{case}: {human}");
        assert!(human.contains(says), "{case}: {human}");
        support::assert_placement_waived(&r);
    }
}

/// The other half of the VICR row above: a limit the run measured as violated
/// refuses a PASS, but a FAIL on that board is the board's own failure and
/// must say so. Refusing it as "not a failure of your board" (exit 3) hid
/// exactly the faults an outcome spec is written to find. Here IN+ sits above
/// IN- with both above the ceiling, so OUT idles high on its pull-up and an
/// outcome asking for it low fails, naming the limit as the reason.
#[test]
fn a_failure_on_a_board_outside_a_datasheet_limit_is_a_failure_that_names_it() {
    let board = support::repo("crates/hauksbee-ci/tests/fixtures/lm339_vicr_pair_high.kicad_pcb");
    let r = support::run_body(
        "vicr fail.toml",
        &format!(
            "name = \"vicr fail\"\nboard = {}\nduration_ms = 5\n\
             [[supply]]\nnet = \"V5\"\nkind = \"ideal\"\nvolts = 5.0\n\
             [[assert]]\nname = \"out low\"\nkind = \"voltage\"\nnet = \"OUT\"\n\
             min = 0.0\nmax = 1.0\n",
            support::toml_path(&board),
        ),
    );
    let human = r.render_human();
    assert_eq!(r.exit_code(), 1, "{human}");
    let row = support::row(&r, "out low");
    assert!(!row.passed && !row.invalid, "{row:?}");
    let why = hauksbee_ci::report::why_line(row).unwrap_or_default();
    assert!(
        why.contains("does not hold as designed")
            && why.contains("every input of one channel above its common-mode range"),
        "the failure must name the violated limit as its cause: {why}"
    );
    assert!(human.contains("[FAIL] out low"), "{human}");
    assert!(!human.contains("not a failure of your board"), "{human}");
}

/// A supply rail fed from a powered rail through a resistor too large for its
/// parts' draw is STARVED, a board fault, not an unpowered rail the spec
/// forgot: the run solves it where the feed leaves it, never banners it as
/// "nothing powered them" (which sends the user to declare a supply that
/// would hide the fault), and an outcome that needs the rail up fails naming
/// the feed resistor and the most it may be.
#[test]
fn a_rail_starved_through_its_feed_resistor_fails_naming_the_feed() {
    // Three LMV7219s (SNOS458I: 1.1 mA typ each at 5 V, 2.7 V minimum supply;
    // SOT-23-5 pads 1 OUT, 2 V-, 3 IN+, 4 IN-, 5 V+) on VDD_A, fed from +5V
    // through 1 k: their 3.3 mA would drop 3.3 V, past the 2.3 V between the
    // 5 V feed and their minimum. Inputs sit at 0 V, inside the common-mode
    // range at any supply, so the starved rail is the only limit broken.
    let sot = "Package_TO_SOT_SMD:SOT-23-5";
    let r0805 = "Resistor_SMD:R_0805_2012Metric";
    let mut parts = String::new();
    for i in 1..=3_usize {
        let out = format!("OUT{i}");
        let pads = support::pad(1, &out)
            + &support::pad(2, "GND")
            + &support::pad(3, "GND")
            + &support::pad(4, "GND")
            + &support::pad(5, "VDD_A");
        parts += &support::comp(&format!("U{i}"), sot, "LMV7219", 20 * i, pads);
        let load = support::pad(1, &out) + &support::pad(2, "GND");
        parts += &support::comp(&format!("RL{i}"), r0805, "10k", 20 * i + 10, load);
    }
    parts += &support::comp(
        "RFEED",
        r0805,
        "1k",
        100,
        support::pad(1, "+5V") + &support::pad(2, "VDD_A"),
    );
    parts += &support::comp(
        "CA",
        "Capacitor_SMD:C_0805_2012Metric",
        "100nF",
        120,
        support::pad(1, "VDD_A") + &support::pad(2, "GND"),
    );
    let board = support::board_text(&["+5V", "VDD_A", "GND", "OUT1", "OUT2", "OUT3"], &parts);
    let path = support::placement_board("starved_feed.kicad_pcb", &board);
    let r = support::run_body(
        "starved.toml",
        &format!(
            "name = \"starved\"\nboard = {}\nduration_ms = 5\n\
             [[assert]]\nname = \"analog rail is up\"\nkind = \"voltage\"\nnet = \"VDD_A\"\n\
             min = 4.5\nmax = 5.5\n",
            support::toml_path(&path),
        ),
    );
    let human = r.render_human();
    assert!(
        r.dead_rails.is_empty(),
        "VDD_A is fed through RFEED: {:?}\n{human}",
        r.dead_rails
    );
    assert!(!human.contains("UNPOWERED RAIL"), "{human}");
    assert_eq!(r.exit_code(), 1, "{human}");
    let row = support::row(&r, "analog rail is up");
    let why = hauksbee_ci::report::why_line(row).unwrap_or_default();
    assert!(
        why.contains("'VDD_A' is starved") && why.contains("RFEED") && why.contains("no more than"),
        "the failure must name the starved rail, its feed and the fix: {why}\n{human}"
    );
}
