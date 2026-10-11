//! A simple regulator's EN/SHDN pin switches its rail.
//!
//! The ideal-source regulator stamp used to hold its rail whatever the board
//! did with the enable pin, so a board that ties EN to GND (a regulator it
//! means to keep off, or a wiring fault) reported a healthy 3.3 V rail and
//! every check downstream of it passed on a rail that is never there. These
//! tests pin the model on synthetic boards and synthetic cards:
//!
//! * EN tied to VIN: the rail is the one it always was.
//! * EN tied to GND: the rail is unpowered, every report surface says so and
//!   says why, and a check on the rail fails rather than passing silently.
//! * An active-low SHDN pin: the same, with the levels reversed.
//! * EN left open: the card's internal pull decides.
//! * EN driven by a divider: the switch reads the solved level.
//! * EN pulled down and driven high by firmware on simavr: the rail comes up
//!   only after the GPIO does, and the timing is judged, not left host-paced.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use hauksbee_ci::{report::CiResult, RunConfig};

use crate::support;

/// A 3.3 V LDO card with an EN pin on pad 3, written to `dir/models/`.
/// `en` is the extra `[models.params]` lines describing the pin, and `role` the
/// pin's role name (`en` or `shdn`).
fn models_dir(dir: &Path, value: &str, role: &str, en: &str) -> PathBuf {
    let models = dir.join("models");
    std::fs::create_dir_all(&models).expect("models dir");
    let id = value.to_ascii_lowercase().replace('-', "_");
    std::fs::write(
        models.join("synthetic_ldo.toml"),
        format!(
            "[[models]]\n\
             id = \"{id}\"\n\
             kind = \"vreg\"\n\
             description = \"synthetic 3.3 V LDO with an enable pin, for tests\"\n\
             [models.match]\n\
             value_re = \"^{value}$\"\n\
             [models.params]\n\
             vout = 3.3\n\
             dropout_v = 0.3\n\
             iq_a = 50.0e-6\n\
             {en}\n\
             [models.pins]\n\
             \"1\" = \"in\"\n\
             \"2\" = \"gnd\"\n\
             \"3\" = \"{role}\"\n\
             \"4\" = \"nc\"\n\
             \"5\" = \"out\"\n\
             [models.ratings]\n\
             max_current_a = 0.6\n\
             max_voltage_v = 6.5\n"
        ),
    )
    .expect("write card");
    models
}

/// Card fields for an active-high EN with stated levels and a discharge.
const EN_HIGH: &str = "en_on_v = 1.5\nen_off_v = 0.4\nen_discharge_ohms = 60.0";

/// One part: reference, value, footprint, and (pad, net) pairs.
type Part<'a> = (&'a str, &'a str, &'a str, &'a [(&'a str, &'a str)]);

/// A KiCad netlist of `parts`, written to `path`.
fn netlist(path: &Path, parts: &[Part<'_>]) {
    let mut comps = String::new();
    let mut nets: Vec<(&str, Vec<(&str, &str)>)> = Vec::new();
    for (r, value, footprint, pads) in parts {
        let _ = writeln!(
            comps,
            "    (comp (ref \"{r}\") (value \"{value}\") (footprint \"{footprint}\"))"
        );
        for (pad, net) in *pads {
            match nets.iter_mut().find(|(n, _)| n == net) {
                Some((_, nodes)) => nodes.push((r, pad)),
                None => nets.push((net, vec![(r, pad)])),
            }
        }
    }
    let mut body = String::new();
    for (code, (name, nodes)) in nets.iter().enumerate() {
        let _ = writeln!(body, "    (net (code \"{}\") (name \"{name}\")", code + 1);
        for (r, pad) in nodes {
            let _ = writeln!(body, "      (node (ref \"{r}\") (pin \"{pad}\"))");
        }
        body.push_str("    )\n");
    }
    let text = format!("(export (version \"E\")\n  (components\n{comps}  )\n  (nets\n{body}  ))\n");
    std::fs::write(path, text).expect("write board");
}

const SOT25: &str = "Package_TO_SOT_SMD:SOT-23-5";
const R0603: &str = "Resistor_SMD:R_0603_1608Metric";
const C0603: &str = "Capacitor_SMD:C_0603_1608Metric";

/// The regulator U1 from `+5V` to `VOUT` with pad 3 on `en` (or left
/// unconnected for `None`), a 1 uF output cap and a 330 R load, plus `extra`.
fn ldo_board(dir: &Path, value: &str, en: Option<&str>, extra: &[Part<'_>]) -> PathBuf {
    let mut pads = vec![("1", "+5V"), ("2", "GND"), ("5", "VOUT")];
    pads.extend(en.map(|net| ("3", net)));
    let mut parts: Vec<Part<'_>> = vec![
        ("U1", value, SOT25, &pads),
        ("C1", "1uF", C0603, &[("1", "VOUT"), ("2", "GND")]),
        ("R1", "330", R0603, &[("1", "VOUT"), ("2", "GND")]),
    ];
    parts.extend_from_slice(extra);
    let path = dir.join("board.net");
    netlist(&path, &parts);
    path
}

/// A spec powering `+5V` and checking `VOUT` holds 3.3 V after it settles.
fn spec(dir: &Path, board: &Path, head: &str, tail: &str) -> PathBuf {
    let path = dir.join("spec.toml");
    std::fs::write(
        &path,
        format!(
            "name = \"regulator enable\"\n\
             board = {}\n\
             {head}\n\
             [[supply]]\n\
             net = \"+5V\"\n\
             kind = \"ideal\"\n\
             volts = 5.0\n\
             [[assert]]\n\
             name = \"VOUT regulated\"\n\
             kind = \"voltage\"\n\
             net = \"VOUT\"\n\
             min = 3.2\n\
             max = 3.4\n\
             after_ms = 1.0\n{tail}",
            support::toml_path(board)
        ),
    )
    .expect("write spec");
    path
}

fn run(spec: &Path, models: &Path) -> CiResult {
    hauksbee_ci::run(&RunConfig {
        spec: spec.to_path_buf(),
        models_dir: Some(models.to_path_buf()),
        ..RunConfig::default()
    })
    .expect("run the spec")
}

/// Run the plain board: `value` card with `role`/`en` fields, pad 3 on `pin`.
fn run_ldo(value: &str, role: &str, en: &str, pin: Option<&str>) -> CiResult {
    let dir = tempfile::tempdir().expect("tempdir");
    let models = models_dir(dir.path(), value, role, en);
    let board = ldo_board(dir.path(), value, pin, &[]);
    run(&spec(dir.path(), &board, "duration_ms = 3", ""), &models)
}

/// The card's supply current follows the enable. U1 hangs off `+5V` through a
/// sense resistor on a `VIN` node, so the voltage there reads the input
/// current, `5 - sense * I(IN)`, and a window on it pins that current. The
/// 330 R load takes 10 mA from a live 3.3 V rail and nothing from a dead one.
/// `extra` parts hang on the board beside them.
fn input_current_run(
    (value, role, card, pin): (&str, &str, &str, Option<&str>),
    (sense, extra): (&str, &[Part<'_>]),
    (min, max): (f64, f64),
) -> CiResult {
    let dir = tempfile::tempdir().expect("tempdir");
    let models = models_dir(dir.path(), value, role, card);
    let mut pads = vec![("1", "VIN"), ("2", "GND"), ("5", "VOUT")];
    pads.extend(pin.map(|net| ("3", net)));
    let mut parts: Vec<Part<'_>> = vec![
        ("U1", value, SOT25, &pads),
        ("C1", "1uF", C0603, &[("1", "VOUT"), ("2", "GND")]),
        ("R1", "330", R0603, &[("1", "VOUT"), ("2", "GND")]),
        ("R4", sense, R0603, &[("1", "+5V"), ("2", "VIN")]),
    ];
    parts.extend_from_slice(extra);
    let board = dir.path().join("board.net");
    netlist(&board, &parts);
    let window = format!(
        "[[assert]]\nname = \"IN current\"\nkind = \"voltage\"\nnet = \"VIN\"\n\
         min = {min}\nmax = {max}\nafter_ms = 1.0\n"
    );
    run(
        &spec(dir.path(), &board, "duration_ms = 3", &window),
        &models,
    )
}

/// The input window held: its row passed, else the detail says what it read.
fn assert_input_current(result: &CiResult, why: &str) {
    let row = support::row(result, "IN current");
    support::assert_row_green(row, &format!("{why}\n{}", result.render_human()));
}

/// EN fed from the supply through a 100k/100k divider (2.5 V, past the 1.5 V on
/// level): nothing fixes the pin, so the enable switches on the solve.
const EN_DIVIDER: [Part<'static>; 2] = [
    ("R2", "100k", R0603, &[("1", "+5V"), ("2", "EN")]),
    ("R3", "100k", R0603, &[("1", "EN"), ("2", "GND")]),
];

/// 5 V less 100 R * (10 mA load + 50 uA `iq_a`).
const ON_WINDOW: (f64, f64) = (3.9945, 3.9955);
/// 5 V less 100 kR * (0.2 uA shutdown current + the open switch's ~5 nA leak).
const SHUTDOWN_WINDOW: (f64, f64) = (4.9780, 4.9805);

/// What the part leaves the VIN node at: its shutdown current when EN holds it
/// off, `iq_a` plus the load when on.
#[test]
fn the_supply_current_follows_the_enable() {
    let card = format!("{EN_HIGH}\ni_shdn_a = 2.0e-7");
    let part = |pin| ("SYNTH-LDO-SD", "en", card.as_str(), pin);
    // The old stamp drew the whole 50 uA with EN low: 5 V across 100 kR,
    // VIN near 0.
    let off = input_current_run(part(Some("GND")), ("100k", &[]), SHUTDOWN_WINDOW);
    assert_input_current(&off, "EN held low draws only the shutdown current");
    assert_eq!(off.dead_rails, ["VOUT"], "the rail is off, not the input");

    // Switched on by a divider the solve reads: the load plus iq_a, shutdown
    // current included in it.
    let on = input_current_run(part(Some("EN")), ("100", &EN_DIVIDER), ON_WINDOW);
    assert_input_current(&on, "EN high draws the load plus iq_a");
    assert!(on.dead_rails.is_empty(), "{:?}", on.dead_rails);
    assert!(
        !on.render_json().contains("shutdown supply current"),
        "a card that states the shutdown current is not caveated"
    );

    // An active-low SHDN pin: tied to its input it is off, to GND it is on.
    let low = "en_on_level = \"low\"\nen_on_v = 0.4\nen_off_v = 1.5\ni_shdn_a = 2.0e-7";
    let part = |pin| ("SYNTH-LDO-SDL", "shdn", low, pin);
    let off = input_current_run(part(Some("VIN")), ("100k", &[]), SHUTDOWN_WINDOW);
    assert_input_current(&off, "SHDN tied to IN draws only the shutdown current");
    let on = input_current_run(part(Some("GND")), ("100", &[]), ON_WINDOW);
    assert_input_current(&on, "SHDN low draws the load plus iq_a");
}

#[test]
fn a_card_with_no_shutdown_current_draws_none_off_and_says_so() {
    let part = |pin| ("SYNTH-LDO-NOSD", "en", EN_HIGH, pin);
    // Nothing to draw: VIN stays at the supply, less a leak of a few nA
    // across 100 kR.
    let off = input_current_run(part(Some("GND")), ("100k", &[]), (4.9985, 5.0001));
    assert_input_current(&off, "no shutdown figure, so IN draws none while off");
    let json = off.render_json();
    assert!(
        json.contains("states no shutdown supply current (`i_shdn_a`)")
            && json.contains("IN is assumed to draw 0 A while its EN pin holds the part off"),
        "the assumption is disclosed:\n{json}"
    );
    // On, the same card draws its full iq_a.
    let on = input_current_run(part(Some("EN")), ("100", &EN_DIVIDER), ON_WINDOW);
    assert_input_current(&on, "EN high draws the load plus iq_a");
}

#[test]
fn a_regulator_with_no_enable_modelled_draws_its_iq_as_before() {
    // The card names no enable pin (pad 3 is `nc`): the old stamp, the same
    // 10.05 mA, and no shutdown disclosure.
    let part = ("SYNTH-LDO-NOEN", "nc", "", None);
    let on = input_current_run(part, ("100", &[]), ON_WINDOW);
    assert_input_current(&on, "iq_a is drawn as always");
    let json = on.render_json();
    assert!(
        !json.contains("shutdown"),
        "no enable, no shutdown caveat:\n{json}"
    );
}

/// The rail is up, and nothing calls it dead.
fn assert_rail_up(result: &CiResult, why: &str) {
    let row = support::row(result, "VOUT regulated");
    support::assert_row_green(row, &format!("{why}\n{}", result.render_human()));
    assert!(
        result.dead_rails.is_empty(),
        "{why}, so no rail is dead: {:?}",
        result.dead_rails
    );
}

/// The rail is off, the regulator is named as holding it off for `cause`, and
/// the check on it fails behind a banner that comes first.
fn assert_rail_held_off(result: &CiResult, cause: &str) {
    assert_eq!(
        result.dead_rails,
        vec!["VOUT".to_string()],
        "VOUT is held off"
    );
    assert!(
        result
            .dead_rail_causes
            .iter()
            .any(|c| c.starts_with("VOUT: U1 is held off") && c.contains(cause)),
        "the cause names U1 and why: {:?}",
        result.dead_rail_causes
    );
    let row = support::row(result, "VOUT regulated");
    // Deliberate negative: a held-off rail is a numeric red.
    assert!(
        !row.passed,
        "a check on a rail the board holds off must not pass: {}",
        row.detail
    );
    let human = result.render_human();
    let banner = human
        .find("UNPOWERED RAIL: VOUT")
        .expect("banner names VOUT");
    let verdict = human.find("[FAIL]").expect("a failed row");
    assert!(banner < verdict, "the banner comes first:\n{human}");
    assert!(human.contains(cause), "the terminal says why:\n{human}");
    assert!(
        !human.contains("names a supply but not a voltage"),
        "the generic advice does not apply to a held-off regulator:\n{human}"
    );
    let json: serde_json::Value = serde_json::from_str(&result.render_json()).expect("json");
    assert!(
        json["dead_rail_causes"][0]
            .as_str()
            .is_some_and(|c| c.contains(cause)),
        "JSON carries the cause: {}",
        json["dead_rail_causes"]
    );
    let junit = result.render_junit();
    let escaped = cause.replace('\'', "&apos;");
    assert!(
        junit.contains(&escaped),
        "JUnit carries the cause:\n{junit}"
    );
    let gh = result.render_github_annotations();
    assert!(gh.contains(cause), "annotations carry the cause:\n{gh}");
}

#[test]
fn en_tied_to_vin_leaves_the_rail_as_it_was() {
    let result = run_ldo("SYNTH-LDO-EN", "en", EN_HIGH, Some("+5V"));
    assert_rail_up(&result, "EN on VIN turns the part on");
}

#[test]
fn en_tied_to_gnd_is_an_unpowered_rail_not_a_silent_pass() {
    let result = run_ldo("SYNTH-LDO-EN", "en", EN_HIGH, Some("GND"));
    assert_rail_held_off(&result, "its EN pin is tied to GND");
}

#[test]
fn an_active_low_shdn_pin_reads_the_other_way_round() {
    let shdn = "en_on_level = \"low\"\nen_on_v = 0.4\nen_off_v = 1.5";
    let on = run_ldo("SYNTH-LDO-SHDN", "shdn", shdn, Some("GND"));
    assert_rail_up(&on, "SHDN on GND is not shut down");
    let off = run_ldo("SYNTH-LDO-SHDN", "shdn", shdn, Some("+5V"));
    assert_rail_held_off(
        &off,
        "its SHDN pin is tied to its input, which turns it off",
    );
}

#[test]
fn an_open_en_pin_follows_the_cards_internal_pull() {
    let down = format!("{EN_HIGH}\nen_pull_down_ohms = 3.0e6");
    let off = run_ldo("SYNTH-LDO-PD", "en", &down, None);
    assert_rail_held_off(
        &off,
        "its EN pin is left open and the card's internal pull-down holds it off",
    );
    // Connected to a net nothing else touches: the same pull, on the board node.
    let off = run_ldo("SYNTH-LDO-PD", "en", &down, Some("EN_NC"));
    assert_rail_held_off(
        &off,
        "its EN pin is left open and the card's internal pull-down holds it off",
    );

    let up = format!("{EN_HIGH}\nen_pull_up_a = 1.0e-6");
    let on = run_ldo("SYNTH-LDO-PU", "en", &up, None);
    assert_rail_up(&on, "an internal pull-up holds an open EN on");
    let on = run_ldo("SYNTH-LDO-PU", "en", &up, Some("EN_NC"));
    assert_rail_up(&on, "the pull-up stamped on the open net holds it on");
}

#[test]
fn an_open_en_pin_with_no_stated_pull_is_assumed_on_and_says_so() {
    let result = run_ldo("SYNTH-LDO-OPEN", "en", EN_HIGH, None);
    assert_rail_up(&result, "with no pull on the card the part is assumed on");
    let json = result.render_json();
    assert!(
        json.contains("its EN pin is left open and the card states no internal pull"),
        "the assumption is disclosed:\n{json}"
    );
}

#[test]
fn an_open_en_pin_the_datasheet_states_off_holds_the_rail_off() {
    let card = format!("{EN_HIGH}\nen_open = \"off\"");
    let cause = "its EN pin is left open and the datasheet states an open EN pin is off";
    // Unconnected, and on a net nothing else touches: both read the same.
    for en in [None, Some("EN_NC")] {
        let result = run_ldo("SYNTH-LDO-OFF", "en", &card, en);
        assert_rail_held_off(&result, cause);
        let json = result.render_json();
        assert!(
            !json.contains("card states no internal pull"),
            "a stated open state is not a disclosed assumption:\n{json}"
        );
    }
    // An on-low SHDN pin the datasheet says is off when open.
    let shdn = "en_on_level = \"low\"\nen_on_v = 0.4\nen_off_v = 1.5\nen_open = \"off\"";
    let result = run_ldo("SYNTH-LDO-SHDN-OFF", "shdn", shdn, None);
    assert_rail_held_off(
        &result,
        "its SHDN pin is left open and the datasheet states an open SHDN pin is off",
    );
}

#[test]
fn an_open_en_pin_the_datasheet_states_on_leaves_the_rail_up_without_a_caveat() {
    let card = format!("{EN_HIGH}\nen_open = \"on\"");
    let result = run_ldo("SYNTH-LDO-ON", "en", &card, None);
    assert_rail_up(&result, "the datasheet says an open pin is on");
    let json = result.render_json();
    assert!(
        !json.contains("card states no internal pull"),
        "a stated open state carries no assumption caveat:\n{json}"
    );
}

#[test]
fn an_open_en_pin_with_a_pull_ignores_a_consistent_en_open() {
    let down = format!("{EN_HIGH}\nen_pull_down_ohms = 3.0e6\nen_open = \"off\"");
    let off = run_ldo("SYNTH-LDO-PD-OFF", "en", &down, None);
    assert_rail_held_off(
        &off,
        "its EN pin is left open and the card's internal pull-down holds it off",
    );
}

/// EN fed from `+5V` through `top` over `bottom` to GND.
fn divider_run(top: &str, bottom: &str) -> CiResult {
    let dir = tempfile::tempdir().expect("tempdir");
    let models = models_dir(dir.path(), "SYNTH-LDO-EN", "en", EN_HIGH);
    let extra: [Part<'_>; 2] = [
        ("R2", top, R0603, &[("1", "+5V"), ("2", "EN")]),
        ("R3", bottom, R0603, &[("1", "EN"), ("2", "GND")]),
    ];
    let board = ldo_board(dir.path(), "SYNTH-LDO-EN", Some("EN"), &extra);
    run(&spec(dir.path(), &board, "duration_ms = 3", ""), &models)
}

#[test]
fn a_divider_on_en_is_read_through_the_solve() {
    // 2.5 V on a 1.5 V guaranteed-on pin: on.
    let on = divider_run("100k", "100k");
    assert_rail_up(&on, "2.5 V on EN is past its 1.5 V on level");
    // 0.31 V on the same pin: below its 0.4 V off level, so the rail stays
    // off. Nothing statically pins EN, so the solve decides, and the rail
    // check fails on the 0 V it reads.
    let off = divider_run("150k", "10k");
    let row = support::row(&off, "VOUT regulated");
    // Deliberate negative: the rail check reads 0 V.
    assert!(
        !row.passed,
        "0.31 V on EN leaves the regulator off: {}",
        row.detail
    );
}

#[test]
fn en_pulled_to_gnd_by_a_resistor_alone_is_held_off() {
    let dir = tempfile::tempdir().expect("tempdir");
    let models = models_dir(dir.path(), "SYNTH-LDO-EN", "en", EN_HIGH);
    let extra: [Part<'_>; 1] = [("R2", "100k", R0603, &[("1", "EN"), ("2", "GND")])];
    let board = ldo_board(dir.path(), "SYNTH-LDO-EN", Some("EN"), &extra);
    let result = run(&spec(dir.path(), &board, "duration_ms = 3", ""), &models);
    assert_rail_held_off(&result, "pulled to GND through resistors");
}

/// EN pulled down by 100k, driven by an `ATmega328P`'s PB5 (DIP-28 pad 19)
/// running the committed demo firmware, which makes PB5 an output (low) at
/// reset and first drives it high one ~100 ms tick loop later.
#[cfg(feature = "avr")]
fn firmware_run(dir: &Path, asserts: &str) -> CiResult {
    let models = models_dir(dir, "SYNTH-LDO-EN", "en", EN_HIGH);
    let extra: [Part<'_>; 2] = [
        ("R2", "100k", R0603, &[("1", "D13"), ("2", "GND")]),
        (
            "U2",
            "ATmega328P",
            "Package_DIP:DIP-28_W7.62mm",
            &[
                ("7", "+5V"),
                ("8", "GND"),
                ("19", "D13"),
                ("20", "+5V"),
                ("22", "GND"),
            ],
        ),
    ];
    let board = ldo_board(dir, "SYNTH-LDO-EN", Some("D13"), &extra);
    let firmware = support::repo("testdata/firmware/demo/demo.hex");
    let head = format!(
        "firmware = {}\nmcu = \"atmega328p\"\nduration_ms = 200",
        support::toml_path(&firmware)
    );
    let path = dir.join("fw.toml");
    std::fs::write(
        &path,
        format!(
            "name = \"firmware enable\"\nboard = {}\n{head}\n\
             [[supply]]\nnet = \"+5V\"\nkind = \"ideal\"\nvolts = 5.0\n{asserts}",
            support::toml_path(&board)
        ),
    )
    .expect("write spec");
    run(&path, &models)
}

#[cfg(feature = "avr")]
#[test]
fn a_firmware_gpio_on_en_brings_the_rail_up_after_it_goes_high() {
    let dir = tempfile::tempdir().expect("tempdir");
    // The first PB5 high lands after the UART banner (about 20 ms at 9600
    // baud) and ten 10 ms loop ticks, at 120 ms on the 1 ms frame grid. The
    // rail must be below 3.0 V for more than 115 ms (it did not come up before
    // the GPIO) and no more than 125 ms (it came up with it), then hold.
    let asserts = "\
        [[assert]]\nname = \"up after the GPIO\"\nkind = \"voltage\"\nnet = \"VOUT\"\n\
        min = 3.2\nmax = 3.4\nafter_ms = 160.0\n\
        [[assert]]\nname = \"off until the GPIO\"\nkind = \"rail_window\"\nnet = \"VOUT\"\n\
        dip_below = 3.0\nfor_max_ms = 115.0\n\
        [[assert]]\nname = \"not off for long\"\nkind = \"rail_window\"\nnet = \"VOUT\"\n\
        dip_below = 3.0\nfor_max_ms = 125.0\n";
    let result = firmware_run(dir.path(), asserts);
    let human = result.render_human();
    assert!(
        result.dead_rails.is_empty(),
        "a GPIO on EN is not a static hold-off: {:?}\n{human}",
        result.dead_rails
    );
    let up = support::row(&result, "up after the GPIO");
    support::assert_row_green(up, &format!("the rail comes up once PB5 is high\n{human}"));
    let long = support::row(&result, "not off for long");
    support::assert_row_green(long, &format!("and it was not off for the whole run\n{human}"));
    let off = support::row(&result, "off until the GPIO");
    // Deliberate negative: the rail is off for longer than the 115 ms window allows.
    assert!(
        !off.passed,
        "the rail stays off until the GPIO goes high, so it dips for more than \
         115 ms: {}\n{human}",
        off.detail
    );
    // simavr counts cycles: the time bounds are judged, not left host-paced.
    for row in [up, long, off] {
        assert!(
            row.host_paced.is_none(),
            "simavr is not host-paced: {}",
            row.label
        );
        assert!(!row.detail.contains("not judged"), "{}", row.detail);
    }
}
