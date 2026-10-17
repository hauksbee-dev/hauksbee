//! An indicator's meaning is intent: an active-low `READY_N` through a
//! non-inverting buffer lights its LED when the board is NOT ready, and every
//! voltage on the path is in range. Only a stated outcome can tell the two
//! apart, so a spec that says "lit when `READY_N` is asserted" must pass the
//! inverter and fail the buffer on the same footprint, and the scaffold must
//! offer that outcome for the user to confirm.

use crate::support::{self, comp, pad};

/// `READY_N` -> U1 (`gate`, SOT-23-5: 1 NC, 2 A, 3 GND, 4 Y, 5 VCC) -> 1 kOhm
/// -> red LED D1 to ground.
fn board(gate: &str) -> String {
    let u1: String = ["NC", "READY_N", "GND", "LED_DRV", "VCC"]
        .iter()
        .zip(1..)
        .map(|(n, i)| pad(i, n))
        .collect();
    let sot = "Package_TO_SOT_SMD:SOT-23-5";
    let r1 = comp(
        "R1",
        "Resistor_SMD:R_0805_2012Metric",
        "1k",
        30,
        pad(1, "LED_DRV") + &pad(2, "LED_A"),
    );
    // LED_0805 pad 1 is the cathode, pad 2 the anode.
    let d1 = comp(
        "D1",
        "LED_SMD:LED_0805_2012Metric",
        "LED_RED",
        40,
        pad(1, "GND") + &pad(2, "LED_A"),
    );
    let parts = comp("U1", sot, gate, 10, u1) + &r1 + &d1;
    support::board_text(&["VCC", "GND", "READY_N", "LED_DRV", "LED_A", "NC"], &parts)
}

fn run(gate: &str) -> hauksbee_ci::CiResult {
    let path = support::write_tmp("indicator.kicad_pcb", &board(gate));
    support::run_body(
        &format!("indicator_{gate}.toml"),
        &format!(
            "name = \"indicator {gate}\"\nboard = {}\nduration_ms = 2\n\
             [[net_drive]]\nnet = \"VCC\"\nvolts = 3.3\n\
             [search]\n[[search.sweep]]\nnet = \"READY_N\"\nvolts = [0.0, 3.3]\n\
             [[assert]]\nname = \"lit when ready\"\nkind = \"decreases\"\n\
             net = \"LED_A\"\nalong = \"READY_N\"\n",
            support::toml_path(&path),
        ),
    )
}

#[test]
fn a_stated_polarity_passes_the_inverter_and_fails_the_buffer() {
    let inverter = run("74LVC1G04");
    let row = support::row(&inverter, "lit when ready");
    support::assert_row_green(row, &format!("inverter: {}", inverter.render_human()));

    let buffer = run("74LVC1G17");
    let row = support::row(&buffer, "lit when ready");
    // Deliberate negative: the buffer has the wrong polarity.
    assert!(!row.passed, "buffer: {}", buffer.render_human());
    assert!(row.detail.contains("READY_N"), "{}", row.detail);
}

#[test]
fn init_scaffolds_one_indicator_outcome_with_the_polarity_to_confirm() {
    let path = support::write_tmp("indicator_init.kicad_pcb", &board("74LVC1G17"));
    let spec = hauksbee_ci::init::render_spec_at(&path, path.parent().unwrap()).unwrap();
    assert!(
        spec.contains(
            "# LED D1 should be lit when READY_N is asserted; confirm whether READY_N is active-low."
        ),
        "{spec}"
    );
    assert!(spec.contains("via U1 buffer"), "{spec}");
    assert!(spec.contains("# kind = \"decreases\""), "{spec}");
    assert!(spec.contains("# net = \"LED_A\""), "{spec}");
    assert!(spec.contains("# along = \"READY_N\""), "{spec}");
    assert_eq!(spec.matches("# [[search.sweep]]").count(), 1, "{spec}");

    // Uncommented as instructed, the block is valid TOML of the spec's shape.
    let start = spec.find("# Indicator outcomes").expect("indicator block");
    let live: String = spec[start..]
        .lines()
        .filter_map(|l| l.strip_prefix("# ").filter(|rest| is_toml_line(rest)))
        .fold(String::new(), |acc, l| acc + l + "\n");
    let toml: toml::Value = toml::from_str(&live).expect("uncommented block parses");
    assert_eq!(
        toml["search"]["sweep"][0]["net"].as_str(),
        Some("READY_N"),
        "{live}"
    );
    assert_eq!(
        toml["assert"][0]["kind"].as_str(),
        Some("decreases"),
        "{live}"
    );
}

/// A commented scaffold line that is TOML (a header or a `key = value`), not prose.
fn is_toml_line(rest: &str) -> bool {
    rest.starts_with('[')
        || rest.split_once(" = ").is_some_and(|(k, _)| {
            !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
}
