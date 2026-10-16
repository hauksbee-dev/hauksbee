//! A clean report must not imply an indicator's meaning was checked.
//!
//! `READY_N` through a non-inverting buffer lights its LED when the board is
//! not ready, and nothing on the path is out of range, so no check fires.
//! Without a stated outcome the plain, JSON and web surfaces must each say the
//! indicator's polarity versus intent was not checked, naming the LED and the
//! source net traced back through the buffer, and none of them may fail over
//! it.

use crate::support::{run_board, stdout};

const LINE: &str = "indicator meaning and signal polarity versus intent: not checked";

/// `READY_N` -> U1 (74LVC1G17 buffer) -> 1 kOhm -> red LED D1 to ground.
fn board() -> std::path::PathBuf {
    let pad = |n: &str, x: u32, net: &str| {
        format!("pad \"{n}\" smd rect at {x} 0 size 2 1.5 layers [F.Cu F.Mask] net \"{net}\"\n")
    };
    let comp = |r: &str, lib: &str, val: &str, x: u32, pads: String| {
        format!(
            "comp {r} lib \"{lib}\" val \"{val}\" layer \"F.Cu\" at {x} 20 rot 0 {{\n{pads}}}\n"
        )
    };
    let u1: String = ["NC", "READY_N", "GND", "LED_DRV", "VCC"]
        .iter()
        .zip(1..)
        .map(|(net, i)| pad(&i.to_string(), i, net))
        .collect();
    let parts = comp("U1", "Package_TO_SOT_SMD:SOT-23-5", "74LVC1G17", 10, u1)
        + &comp(
            "R1",
            "Resistor_SMD:R_0805_2012Metric",
            "1k",
            30,
            pad("1", 1, "LED_DRV") + &pad("2", 2, "LED_A"),
        )
        + &comp(
            "D1",
            "LED_SMD:LED_0805_2012Metric",
            "LED_RED",
            40,
            pad("K", 1, "GND") + &pad("A", 3, "LED_A"),
        );
    let nets = ["VCC", "GND", "READY_N", "LED_DRV", "LED_A", "NC"]
        .map(|n| format!("net \"{n}\"\n"))
        .concat();
    let dir = std::env::temp_dir().join(format!("hauksbee_indicator_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("indicator.board");
    std::fs::write(
        &path,
        format!("board version 20171130\nfn main {{\n{nets}{parts}}}\n"),
    )
    .unwrap();
    path
}

#[test]
fn plain_and_json_say_the_indicator_polarity_was_not_checked() {
    let path = board();
    let plain = run_board(&path, &["--check", "--plain"]);
    let text = stdout(&plain);
    let not_covered = text
        .lines()
        .find(|l| l.starts_with("NOT COVERED:"))
        .unwrap_or_else(|| panic!("a NOT COVERED line:\n{text}"));
    assert!(not_covered.contains(LINE), "{not_covered}");
    assert!(
        not_covered.contains("D1 (from READY_N, name suggests active-low, via U1 buffer)"),
        "{not_covered}"
    );

    let json = run_board(&path, &["--check", "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&json.stdout).expect("json report");
    let note = v["triage"]["not_covered"].as_str().unwrap_or_default();
    assert!(note.contains(LINE), "{note}");
    // Not judged is not a failure: nothing about it reaches the order buckets.
    for bucket in ["do_not_order", "inspect"] {
        let entries = v["triage"][bucket].to_string();
        assert!(!entries.contains("polarity"), "{bucket}: {entries}");
    }
    assert_eq!(plain.status.code(), json.status.code());
}

#[test]
fn the_web_report_carries_the_same_note_without_failing() {
    let bytes = std::fs::read(board()).unwrap();
    let report = hauksbee_engine::frontdoor::analyze("indicator.board", &bytes);
    assert!(report.ok, "{:?}", report.error);
    let triage = report.triage.as_ref().expect("triage");
    assert!(triage.not_covered.contains(LINE), "{}", triage.not_covered);
    assert!(
        report.notes.iter().any(|n| n.message.contains(LINE)),
        "{:?}",
        report.notes
    );
    // The unrouted test board has findings of its own; none is the indicator.
    let findings: Vec<&str> = report
        .sections
        .iter()
        .flat_map(|s| s.findings.iter().map(|f| f.what.as_str()))
        .collect();
    assert!(
        findings.iter().all(|f| !f.contains("polarity")),
        "{findings:?}"
    );
}

const CONDITION: &str = "Indicator polarity versus intent was not checked for D1";

/// A 74HCT245 channel sinking a green LED from `CARRIER_DETECT`: electrically
/// clean as a netlist, and which level lights the LED is fixed by the wiring,
/// not by firmware. `placeholder_cap` adds a decoupling capacitor whose value
/// is still the placeholder `C`: a gating warning that is not serious.
fn transceiver_netlist(name: &str, placeholder_cap: bool) -> std::path::PathBuf {
    let (cap, cap_5v, cap_gnd) = if placeholder_cap {
        (
            r#"
    (comp (ref "C1") (value "C") (footprint "Capacitor_SMD:C_0805_2012Metric"))"#,
            r#" (node (ref "C1") (pin "1"))"#,
            r#" (node (ref "C1") (pin "2"))"#,
        )
    } else {
        ("", "", "")
    };
    let text = format!(
        r#"(export (version "E")
  (design (source "{name}.kicad_sch") (tool "Eeschema"))
  (components
    (comp (ref "J1") (value "Conn_01x03") (footprint "Connector_PinHeader_2.54mm:PinHeader_1x03_P2.54mm_Vertical"))
    (comp (ref "U1") (value "74HCT245") (footprint "Package_SO:SOIC-20W_7.5x12.8mm_P1.27mm"))
    (comp (ref "R1") (value "330") (footprint "Resistor_SMD:R_0805_2012Metric"))
    (comp (ref "D1") (value "LED_GREEN") (footprint "LED_SMD:LED_0805_2012Metric")){cap})
  (nets
    (net (code "1") (name "+5V") (node (ref "J1") (pin "1")) (node (ref "U1") (pin "20")) (node (ref "U1") (pin "1")) (node (ref "D1") (pin "2") (pinfunction "A")){cap_5v})
    (net (code "2") (name "GND") (node (ref "J1") (pin "2")) (node (ref "U1") (pin "10")) (node (ref "U1") (pin "19")){cap_gnd}
      (node (ref "U1") (pin "3")) (node (ref "U1") (pin "4")) (node (ref "U1") (pin "5")) (node (ref "U1") (pin "6"))
      (node (ref "U1") (pin "7")) (node (ref "U1") (pin "8")) (node (ref "U1") (pin "9")))
    (net (code "3") (name "CARRIER_DETECT") (node (ref "J1") (pin "3")) (node (ref "U1") (pin "2")))
    (net (code "4") (name "LED_K") (node (ref "U1") (pin "18")) (node (ref "R1") (pin "1")))
    (net (code "5") (name "LED_R") (node (ref "R1") (pin "2")) (node (ref "D1") (pin "1") (pinfunction "K")))))
"#
    );
    let dir = std::env::temp_dir().join(format!("hauksbee_indicator_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{name}.net"));
    std::fs::write(&path, text).unwrap();
    path
}

/// The pass rests on what the tool cannot know, so every surface says it is
/// conditional on it, in the same words: `--json` (verdict still `pass`), the
/// plain triage headline, the closing VERDICT line and the web headline.
#[test]
fn a_hardware_fixed_indicator_makes_the_pass_conditional_on_every_surface() {
    let path = transceiver_netlist("indicator_condition", false);

    let json = run_board(&path, &["--check", "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&json.stdout).expect("json report");
    assert_eq!(v["verdict"], "pass", "{v:#}");
    let conditions: Vec<&str> = v["verdict_conditions"]
        .as_array()
        .unwrap_or_else(|| panic!("conditions: {v:#}"))
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect();
    // One condition for the area; the only other is that the fixture's
    // connector-fed rail is assumed supplied from off the board.
    assert!(conditions[0].starts_with(CONDITION), "{conditions:?}");
    assert!(
        conditions[1..]
            .iter()
            .all(|c| c.starts_with("Rails assumed supplied from off the board")),
        "one condition for the area: {conditions:?}"
    );

    let text = stdout(&run_board(&path, &["--check", "--plain"]));
    let headline = text.lines().nth(1).unwrap_or_default();
    assert!(
        headline.starts_with("OK TO ORDER, conditionally: ") && headline.contains(conditions[0]),
        "{text}"
    );
    let verdict = text
        .lines()
        .rfind(|l| l.starts_with("VERDICT:"))
        .unwrap_or_default();
    assert!(
        verdict.starts_with("VERDICT: clean, conditionally: ") && verdict.contains(conditions[0]),
        "{verdict}"
    );
    // The detail stays where it was: the traced path is in NOT COVERED.
    assert!(text.contains("NOT COVERED: Indicators: "), "{text}");

    let bytes = std::fs::read(&path).unwrap();
    let web = hauksbee_engine::frontdoor::analyze("indicator_condition.net", &bytes);
    assert!(web.ok, "{:?}", web.error);
    assert_eq!(web.verdict, "pass");
    assert_eq!(web.verdict_conditions, conditions);
    assert!(
        web.headline.starts_with("OK to order, conditionally: ")
            && !web.headline.contains('`')
            && web
                .headline
                .contains(&format!("this rests on: {}", conditions[0])),
        "{}",
        web.headline
    );
}

/// A finding that fails the `--check` gate without being serious (a
/// placeholder capacitor value) fails the run; the web headline must not read
/// as a pass beside it.
#[test]
fn a_gating_warning_fails_the_web_headline_as_it_fails_the_json() {
    let path = transceiver_netlist("indicator_gating_warning", true);
    let json = run_board(&path, &["--check", "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&json.stdout).expect("json report");
    assert_eq!(v["verdict"], "fail", "{v:#}");
    assert_eq!(v["serious_count"], 0, "{v:#}");
    assert!(v.get("verdict_conditions").is_none(), "{v:#}");

    let bytes = std::fs::read(&path).unwrap();
    let web = hauksbee_engine::frontdoor::analyze("indicator_gating_warning.net", &bytes);
    assert!(web.ok, "{:?}", web.error);
    assert_eq!(web.verdict, "fail");
    assert_eq!(web.serious, 0);
    assert_eq!(web.verdict_reason.as_deref(), v["verdict_reason"].as_str());
    assert!(web.verdict_conditions.is_empty());
    assert!(
        web.headline.starts_with("Not OK to order: "),
        "{}",
        web.headline
    );
}

/// An LED a microcontroller pin drives directly is lit by whatever level the
/// firmware writes, so ordering the board does not fix its meaning: it stays
/// NOT COVERED but adds no condition (blinky's only conditions are that its
/// copper was spaced against default rules, having no project file beside it,
/// and how its rails are taken to be supplied).
#[test]
fn a_firmware_driven_indicator_adds_no_condition() {
    let path = crate::support::board("../hauksbee-ci/examples/boards/blinky.kicad_pcb");
    let json = run_board(&path, &["--check", "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&json.stdout).expect("json report");
    let note = v["triage"]["not_covered"].as_str().unwrap_or_default();
    assert!(note.contains(LINE), "{note}");
    assert_eq!(v["verdict"], "pass", "{v:#}");
    let conditions = v["verdict_conditions"].as_array().expect("conditions");
    assert!(
        conditions.iter().all(|c| c.as_str().is_some_and(|c| c
            .starts_with("copper spacing was judged against a DEFAULT")
            || c.starts_with("Rails "))),
        "{v:#}"
    );
}
