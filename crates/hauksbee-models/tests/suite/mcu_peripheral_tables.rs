//! Every shipped MCU card states where each hardware peripheral can go.
//!
//! The pin-mux check (`pin_mux`) and the ADC check abstain on a card with no
//! table, so a card that leaves one out silently loses the finding for that
//! function. The validator refuses a card that NAMES a peripheral without its
//! table; this suite goes further for the shipped cards: each one with a pad map
//! declares every peripheral function (a pad table, an empty table when the
//! package has none, or the matrix declaration), each with a basis that cites a
//! page, and a few tables are pinned against the datasheet.

use hauksbee_models::schema::DbFile;
use hauksbee_models::validation::validate;
use hauksbee_models::{ComponentKind, ModelEntry, NamedPeripheral};

/// The shipped MCU cards that carry a pad map.
fn shipped_mcu_cards() -> Vec<ModelEntry> {
    let db = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("db");
    let mut cards = Vec::new();
    for path in std::fs::read_dir(db)
        .expect("db dir")
        .flatten()
        .map(|f| f.path())
    {
        let text = std::fs::read_to_string(&path).expect("db file");
        let Ok(file) = toml::from_str::<DbFile>(&text) else {
            continue;
        };
        cards.extend(
            file.models
                .into_iter()
                .filter(|m| m.kind == ComponentKind::Mcu && !m.pins.is_empty()),
        );
    }
    cards
}

fn card(id: &str) -> ModelEntry {
    shipped_mcu_cards()
        .into_iter()
        .find(|m| m.id == id)
        .unwrap_or_else(|| panic!("no shipped MCU card '{id}'"))
}

/// A basis cites a page when it carries a page marker followed by a number.
fn cites_a_page(basis: &str) -> bool {
    let lower = basis.to_ascii_lowercase();
    ["p.", "pp.", "page ", "pdf p"].iter().any(|marker| {
        lower.match_indices(marker).any(|(at, _)| {
            lower[at + marker.len()..]
                .trim_start()
                .starts_with(|c: char| c.is_ascii_digit())
        })
    })
}

#[test]
fn every_shipped_mcu_card_declares_every_peripheral() {
    let cards = shipped_mcu_cards();
    assert!(cards.len() >= 60, "only {} MCU cards seen", cards.len());
    let mut missing = Vec::new();
    for m in &cards {
        for peripheral in NamedPeripheral::all() {
            if !m.declares(peripheral) {
                missing.push(format!("{}: {}", m.id, peripheral.key()));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "MCU cards with no table for a peripheral (read the datasheet's alternate-function \
         table; an empty table says the package has none):\n  {}",
        missing.join("\n  ")
    );
}

/// No shipped card has a peripheral-table problem the validator can see: a bad
/// basis, a missing table for a named peripheral, a malformed routed declaration.
///
/// Only the problems about `pin_capability` count here. Two cards carry older
/// validation problems unrelated to the tables (`rp2040` reset supply role,
/// `esp8266ex_qfn32` pad 13 label); those are not this suite's to judge.
#[test]
fn every_shipped_mcu_card_has_valid_peripheral_tables() {
    let failures: Vec<String> = shipped_mcu_cards()
        .iter()
        .filter_map(|m| {
            let errors = validate(m).err()?;
            let ours: Vec<String> = errors
                .iter()
                .map(ToString::to_string)
                .filter(|e| e.contains("pin_capability"))
                .collect();
            (!ours.is_empty()).then(|| format!("{}: {}", m.id, ours.join("; ")))
        })
        .collect();
    assert!(
        failures.is_empty(),
        "shipped MCU cards whose peripheral tables fail validation:\n  {}",
        failures.join("\n  ")
    );
}

#[test]
fn every_peripheral_table_cites_a_page() {
    let mut uncited = Vec::new();
    for m in shipped_mcu_cards() {
        let cap = &m.pin_capability;
        let bases = hauksbee_ir::pin_functions::PeripheralFunction::ALL
            .iter()
            .filter_map(|f| cap.table(*f).map(|t| (f.key(), t.basis.as_str())))
            .chain(cap.routed.as_ref().map(|r| ("routed", r.basis.as_str())));
        for (key, basis) in bases {
            if !cites_a_page(basis) {
                uncited.push(format!("{}: {key}", m.id));
            }
        }
    }
    assert!(
        uncited.is_empty(),
        "tables whose basis names no page:\n  {}",
        uncited.join("\n  ")
    );
}

#[test]
fn atmega328p_peripherals_sit_on_their_fixed_pads() {
    let m = card("atmega328p");
    let cap = &m.pin_capability;
    // 28-pin DIP: SCK PB5 = 19, MOSI PB3 = 17, MISO PB4 = 18, SDA PC4 = 27,
    // SCL PC5 = 28, TXD PD1 = 3, RXD PD0 = 2, ICP1 PB0 = 14.
    let pads = |t: &Option<hauksbee_models::schema::FunctionPads>| {
        t.as_ref()
            .map(|t| t.pads.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default()
    };
    assert!(pads(&cap.spi_sck).contains(&"19".to_owned()));
    assert!(pads(&cap.spi_mosi).contains(&"17".to_owned()));
    assert!(pads(&cap.spi_miso).contains(&"18".to_owned()));
    assert_eq!(pads(&cap.i2c_sda), vec!["27".to_owned()]);
    assert_eq!(pads(&cap.i2c_scl), vec!["28".to_owned()]);
    assert_eq!(pads(&cap.timer_capture), vec!["14".to_owned()]);
}

#[test]
fn matrix_parts_declare_routing_and_a_part_without_a_function_declares_it_empty() {
    // The ESP32 routes UART, I2C, SPI and LEDC through its GPIO matrix.
    let esp = card("esp32_wroom");
    for key in ["uart_tx", "i2c_sda", "spi_sck", "pwm"] {
        assert!(
            esp.pin_capability
                .routed
                .as_ref()
                .is_some_and(|r| r.functions.contains(key)),
            "esp32_wroom should route {key}"
        );
    }
    // The ESP32-C3 has no MCPWM or capture unit: an empty table, not silence.
    let c3 = card("esp32c3_mini1");
    let capture = c3.pin_capability.timer_capture.as_ref().expect("declared");
    assert!(capture.pads.is_empty());
    // The RP2040 has no timer input-capture channel either.
    let rp = card("rp2040");
    assert!(rp
        .pin_capability
        .timer_capture
        .as_ref()
        .is_some_and(|t| t.pads.is_empty()));
}

/// The firmware-intent check finds an AVR's capture pad by the word `ICP1` in
/// the `timer_capture` table's pad description, so every simulated AVR card with
/// a capture unit must name it there.
///
/// A dev-board card whose header does not bring ICP1 out (the processor's ICP1
/// pin reaches nothing but the processor) names no ICP1 pad; its basis must then
/// say so in capitals ("ICP1 IS NOT ON THE ... HEADER") so the gap is deliberate.
#[test]
fn every_simulated_avr_card_names_its_capture_pad_icp1() {
    let mut seen = 0;
    for m in shipped_mcu_cards() {
        let Some(backend) = m.params.get_str("backend") else {
            continue;
        };
        if !backend.starts_with("simavr:atmega") {
            continue;
        }
        seen += 1;
        let table = m
            .pin_capability
            .timer_capture
            .as_ref()
            .unwrap_or_else(|| panic!("{}: no timer_capture table", m.id));
        let header_lacks_icp1 = m.params.get_bool("module").unwrap_or(false)
            || m.params.get_f64("module_clock_hz").is_some();
        if header_lacks_icp1 && table.basis.contains("ICP1 IS NOT ON THE") {
            assert!(
                !table.pads.values().any(|what| what.contains("ICP1")),
                "{}: says ICP1 is not on the header but names an ICP1 pad",
                m.id
            );
            continue;
        }
        assert!(
            table.pads.values().any(|what| what
                .split(|c: char| !c.is_ascii_alphanumeric())
                .any(|w| w.eq_ignore_ascii_case("ICP1"))),
            "{}: no capture pad names ICP1: {:?}",
            m.id,
            table.pads
        );
    }
    assert!(seen >= 4, "only {seen} simulated AVR cards seen");
}
