//! Tests for [`super`]: the bus and peripheral limit tables carried on the
//! built-in cards, read back from the shipped TOML exactly as the binder sees
//! them, and the arithmetic built on them (baud error against a clock, the
//! fastest rate a peripheral accepts) so a wrong row fails here first.

use super::*;
use crate::schema::{ComponentKind, DbFile, ModelEntry, ParamValue};
use hauksbee_ir::peripheral_limits::{baud_error_pct, BUS_LIMIT_PARAMS};

fn builtin_entries() -> Vec<ModelEntry> {
    crate::BUILTIN_TOML_FILES
        .iter()
        .flat_map(|(name, src)| {
            toml::from_str::<DbFile>(src)
                .unwrap_or_else(|e| panic!("{name}: {e}"))
                .models
        })
        .collect()
}

fn card(id: &str) -> ModelEntry {
    builtin_entries()
        .into_iter()
        .find(|e| e.id == id)
        .unwrap_or_else(|| panic!("no card {id}"))
}

fn uart(id: &str) -> UartLimits {
    card(id)
        .peripherals
        .uart
        .unwrap_or_else(|| panic!("{id}: no uart table"))
}

fn adc(id: &str) -> AdcLimits {
    card(id)
        .peripherals
        .adc
        .unwrap_or_else(|| panic!("{id}: no adc table"))
}

/// What `validate` says about a card, as messages.
fn validator_messages(entry: &ModelEntry) -> Vec<String> {
    crate::validation::validate(entry)
        .err()
        .unwrap_or_default()
        .into_iter()
        .map(|e| e.message)
        .collect()
}

/// The MCU cards that carry limit tables.
const FILLED: [&str; 15] = [
    "atmega1284",
    "atmega1284p",
    "atmega32u4",
    "pro_micro",
    "arduino_nano",
    "attiny85",
    "attiny85_8pin",
    "attiny85_qfn20",
    "atmega328p",
    "atmega328p_32pad",
    "atmega328p_28mlf",
    "stm32f103c8",
    "rp2040",
    "rp2040_qfn56",
    "rpi_pico",
];

/// Every `[models.peripherals]` block on a built-in card names the https
/// document it was read from and a basis with a table, section or page for
/// every value group; its numbers are finite and positive. The builtin loader
/// does not run the validator, so this runs it on every card.
#[test]
fn every_builtin_peripheral_block_has_a_source_a_basis_and_sound_numbers() {
    let mut seen = Vec::new();
    for entry in builtin_entries() {
        if entry.peripherals.is_empty() {
            continue;
        }
        assert_eq!(entry.kind, ComponentKind::Mcu, "{}", entry.id);
        let problems = entry.peripherals.problems();
        assert!(problems.is_empty(), "{}: {problems:#?}", entry.id);
        for block_source in entry
            .peripherals
            .uart
            .iter()
            .map(|u| (&u.source, &u.source_sha256))
            .chain(
                entry
                    .peripherals
                    .adc
                    .iter()
                    .map(|a| (&a.source, &a.source_sha256)),
            )
        {
            assert_eq!(
                block_source.1.len(),
                64,
                "{}: every block records its document hash",
                entry.id
            );
        }
        seen.push(entry.id);
    }
    for id in FILLED {
        assert!(seen.iter().any(|s| s == id), "{id} lost its limit tables");
    }
}

/// The same check through the card validator, so a user card with a block
/// missing its source is refused the way a builtin one would be.
#[test]
fn the_validator_refuses_a_block_without_a_source_or_basis() {
    let mut entry = card("atmega328p");
    let bad = entry.peripherals.uart.as_mut().unwrap();
    bad.source = "http://example.invalid/ds.pdf".into();
    bad.basis = "from memory".into();
    bad.baud.basis.clear();
    let messages = validator_messages(&entry);
    assert!(
        messages.iter().any(|m| m.contains("https URL")),
        "{messages:?}"
    );
    assert!(messages.iter().any(|m| m.contains("basis")), "{messages:?}");
}

#[test]
fn a_block_on_a_non_mcu_card_is_refused() {
    let mut entry = card("bme280");
    entry.peripherals = card("atmega328p").peripherals;
    let messages = validator_messages(&entry);
    assert!(
        messages
            .iter()
            .any(|m| m.contains("belongs on an mcu card")),
        "{messages:?}"
    );
}

#[test]
fn a_conversion_states_its_duration_exactly_one_way() {
    let mut a = adc("stm32f103c8");
    a.conversions[0].total_cycles = Some(14.0);
    assert!(
        a.problems().iter().any(|p| p.contains("not both")),
        "{:?}",
        a.problems()
    );
    let mut a = adc("rp2040");
    a.conversions[0].total_cycles = None;
    assert!(!a.problems().is_empty());
}

/// Every card with limits survives a TOML round trip unchanged.
#[test]
fn limit_tables_round_trip_through_toml() {
    for entry in builtin_entries()
        .into_iter()
        .filter(|e| !e.peripherals.is_empty())
    {
        let text = toml::to_string(&entry).unwrap_or_else(|e| panic!("{}: {e}", entry.id));
        let back: ModelEntry =
            toml::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", entry.id));
        assert_eq!(back.peripherals, entry.peripherals, "{}", entry.id);
    }
}

/// A table key the schema does not know is an error, not a silently ignored
/// typo (`deny_unknown_fields`).
#[test]
fn an_unknown_key_in_a_limit_table_is_refused() {
    let text = r#"
        source = "https://example.com/ds.pdf"
        basis = "Table 1 p.2"
        resolution_bits = [10]
        samplerate = 5
        [clock]
        source = "x"
        basis = "Table 1 p.2"
    "#;
    assert!(toml::from_str::<AdcLimits>(text).is_err());
}

/// The vocabulary is spelled once, in `hauksbee-ir`. No other source file of
/// the workspace spells a bus-limit param or an ADC mode word as a string
/// literal; they use the constants and the enum.
#[test]
fn the_peripheral_limit_vocabulary_is_defined_once_in_hauksbee_ir() {
    fn rust_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                rust_files(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }
    let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let home = crates
        .join("hauksbee-ir/src/peripheral_limits.rs")
        .canonicalize()
        .unwrap();
    let needles: Vec<String> = BUS_LIMIT_PARAMS
        .iter()
        .copied()
        .chain(AdcConversionMode::ALL.map(AdcConversionMode::as_str))
        // "normal" is an everyday word other crates use for unrelated enums.
        .filter(|w| *w != "normal")
        .map(|w| format!("\"{w}\""))
        .collect();
    let mut files = Vec::new();
    for c in std::fs::read_dir(&crates).unwrap().flatten() {
        let src = c.path().join("src");
        if src.is_dir() {
            rust_files(&src, &mut files);
        }
    }
    assert!(files.len() > 100, "found only {} source files", files.len());
    let mut strays = Vec::new();
    for f in files {
        if f.canonicalize().unwrap() == home {
            continue;
        }
        let text = std::fs::read_to_string(&f).unwrap();
        for n in &needles {
            if text.contains(n.as_str()) {
                strays.push(format!("{} spells {n}", f.display()));
            }
        }
    }
    assert!(
        strays.is_empty(),
        "spell these through hauksbee_ir::peripheral_limits: {strays:#?}"
    );
}

/// The bus-limit names are in the digital and ADC vocabularies, so a card
/// carrying them lints clean, and every card that has one states it as a
/// printed row in hertz with conditions and a basis.
#[test]
fn bus_limit_params_are_known_and_each_one_is_a_sourced_row() {
    for kind in [ComponentKind::Digital, ComponentKind::Adc] {
        for name in BUS_LIMIT_PARAMS {
            assert!(
                crate::param_names::known_param_names(kind).contains(&name),
                "{kind:?} {name}"
            );
        }
    }
    let mut carriers = Vec::new();
    for entry in builtin_entries() {
        for name in BUS_LIMIT_PARAMS {
            let Some(value) = entry.params.0.get(name) else {
                continue;
            };
            let ParamValue::Row(row) = value else {
                panic!(
                    "{}: {name} must be a printed row with a basis, not a bare number",
                    entry.id
                );
            };
            carriers.push(entry.id.clone());
            assert_eq!(row.unit, "Hz", "{} {name}", entry.id);
            assert!(row.max.is_some_and(|m| m > 0.0), "{} {name}", entry.id);
            assert!(!row.conditions.trim().is_empty(), "{} {name}", entry.id);
            assert!(
                cites_location(&row.basis),
                "{} {name}: basis {:?}",
                entry.id,
                row.basis
            );
            assert_eq!(entry.params.get_f64(name), row.max, "{} {name}", entry.id);
        }
        let unknown = crate::param_names::unknown_params(&entry);
        assert!(
            unknown
                .iter()
                .all(|u| !BUS_LIMIT_PARAMS.contains(&u.name.as_str())),
            "{}: {unknown:?}",
            entry.id
        );
    }
    for id in [
        "bme280",
        "ina228",
        "ina219_soic",
        "ads122c04_pw",
        "ads122c04_rte",
        "at24c64d_8pin",
        "w25q32jv_soic8",
    ] {
        assert!(carriers.iter().any(|c| c == id), "{id} lost its bus limit");
    }
}

// ── the arithmetic, from the cards' own data ──────────────────────────────

/// The AVR baud error for UBRR = 8 at 16 MHz, from the card: the datasheet
/// prints -3.5 % for this setting (DS40002061B Table 20-7).
#[test]
fn avr_ubrr_8_at_16_mhz_is_minus_3_5_percent_from_the_card() {
    for id in ["atmega328p", "atmega328p_32pad", "atmega328p_28mlf"] {
        let u = uart(id);
        let baud = u.baud_hz(16e6, 16, 8).unwrap();
        let err = baud_error_pct(baud, 115_200.0).unwrap();
        assert!((err - -3.5).abs() < 0.1, "{id}: {err}");
        // Double speed UBRR = 16 is +2.1 % in the same table.
        let err2 = baud_error_pct(u.baud_hz(16e6, 8, 16).unwrap(), 115_200.0).unwrap();
        assert!((err2 - 2.1).abs() < 0.1, "{id}: {err2}");
        // A divisor outside the 12-bit register, or an oversampling the part
        // does not have, is not a baud rate.
        assert_eq!(u.baud_hz(16e6, 16, 4096), None);
        assert_eq!(u.baud_hz(16e6, 4, 8), None);
    }
}

/// That setting is inside the datasheet's total error but outside what it
/// recommends the receiver carry: the pair a fidelity check needs to word a
/// finding ("legal on paper, marginal on the bench").
#[test]
fn the_avr_error_sits_between_the_recommended_and_the_total_limit() {
    let u = uart("atmega328p");
    let err = baud_error_pct(u.baud_hz(16e6, 16, 8).unwrap(), 115_200.0).unwrap();
    let t = u
        .tolerance(10, 16, None, None)
        .expect("8N1 normal speed row");
    assert!((t.total_error_min_pct - -4.54).abs() < 1e-9);
    assert!((t.total_error_max_pct - 4.58).abs() < 1e-9);
    assert!(err.abs() > t.recommended_receiver_error_pct.unwrap());
    assert!(err > t.total_error_min_pct);
}

#[test]
fn stm32_brr_follows_table_192_of_the_reference_manual() {
    let u = uart("stm32f103c8");
    // 72 MHz, BRR 0x271 (USARTDIV 39.0625): 115.2 kbps exactly.
    assert!((u.baud_hz(72e6, 16, 0x271).unwrap() - 115_200.0).abs() < 1e-6);
    // 36 MHz, BRR 0x138 (USARTDIV 19.5): 115.384 kbps, 0.15 %.
    let b = u.baud_hz(36e6, 16, 0x138).unwrap();
    assert!(
        (baud_error_pct(b, 115_200.0).unwrap() - 0.16).abs() < 0.02,
        "{b}"
    );
    // 72 MHz, USARTDIV 1: the 4.5 Mbit/s the datasheet allows USART1.
    assert!((u.baud_hz(72e6, 16, 0x10).unwrap() - 4.5e6).abs() < 1.0);
    assert!(u
        .max_baud
        .iter()
        .any(|m| (m.baud_hz - 4.5e6).abs() < 1.0 && m.instances == ["USART1"]));
    // M = 0, integer divisor, NF an error: 3.75 %.
    let t = u.tolerance(10, 16, Some(false), Some(true)).unwrap();
    assert!((t.total_error_max_pct - 3.75).abs() < 1e-9);
    let t = u.tolerance(11, 16, Some(true), Some(false)).unwrap();
    assert!((t.total_error_max_pct - 3.53).abs() < 1e-9);
}

#[test]
fn rp2040_divisor_is_ibrd_and_fbrd_as_one_22_bit_number() {
    let u = uart("rp2040");
    // pico-sdk: 125 MHz, 115200 -> IBRD 67, FBRD 52.
    let reg = (67 << 6) | 52;
    let b = u.baud_hz(125e6, 16, reg).unwrap();
    assert!(baud_error_pct(b, 115_200.0).unwrap().abs() < 0.05, "{b}");
    assert_eq!(u.baud_hz(125e6, 16, 1 << 22), None);
    assert!(u
        .max_baud
        .iter()
        .any(|m| (m.baud_hz - 125e6 / 16.0).abs() < 0.05e6));
}

/// The card's conversion timing reproduces the datasheets' own headline
/// rates, which is what makes the cycle counts checkable.
#[test]
fn adc_timing_reproduces_the_datasheet_headline_rates() {
    use AdcConversionMode::Normal;
    let rate = |id: &str, bits, f, s| adc(id).conversion_rate_hz(Normal, bits, f, s).unwrap();
    // ATmega328P: 200 kHz / 13 cycles ~ 15 kSPS; 1 MHz / 13 ~ 76.9 kSPS.
    let a = adc("atmega328p");
    assert!((rate("atmega328p", 10, 200e3, 1.5) - a.max_rate_hz(10).unwrap()).abs() / 15e3 < 0.03);
    assert!((rate("atmega328p", 10, 1e6, 1.5) - a.max_sample_rate[1].max_hz).abs() < 100.0);
    // STM32F103: 14 MHz / (1.5 + 12.5) = 1 MHz = fS max (Table 47).
    let s = adc("stm32f103c8");
    assert!((rate("stm32f103c8", 12, 14e6, 1.5) - s.max_rate_hz(12).unwrap()).abs() < 1.0);
    // RP2040: 48 MHz / 96 = 500 kSPS.
    assert!((rate("rp2040", 12, 48e6, 0.0) - 500e3).abs() < 1.0);
    // First conversion after enabling the AVR ADC takes 25 cycles.
    let first = a
        .conversion_rate_hz(AdcConversionMode::FirstAfterEnable, 10, 125e3, 13.5)
        .unwrap();
    assert!((first - 125e3 / 25.0).abs() < 1e-6);
}

/// The STM32 sampling data (8 pF, 1 kOhm) reproduce the datasheet's own
/// Table 48 from its Equation 1, so the capacitor and switch figures and the
/// printed table agree.
#[test]
fn stm32_source_resistance_table_matches_equation_1() {
    let a = adc("stm32f103c8");
    let rows = &a.input.as_ref().unwrap().max_source_by_sample_time;
    assert_eq!(rows.len(), 8);
    for r in rows {
        let calc = a.settled_source_ohms(r.sample_cycles, r.adc_clock_hz);
        match r.max_ohms {
            // The table is rounded to 0.1 kOhm.
            Some(printed) if printed < 50e3 => {
                let calc = calc.unwrap();
                assert!(
                    (calc - printed).abs() < 150.0,
                    "{} cycles: {calc} vs {printed}",
                    r.sample_cycles
                );
            }
            // 55.5 cycles prints 50 kOhm, the RAIN cap; the equation allows a bit more.
            Some(printed) => assert!(calc.unwrap() >= printed - 150.0),
            None => assert!(calc.is_some_and(|c| c > 50e3), "{} cycles", r.sample_cycles),
        }
    }
}

#[test]
fn the_atmega_adc_prescalers_and_clock_window_are_the_datasheets() {
    let a = adc("atmega328p");
    assert_eq!(a.clock.prescalers, [2, 4, 8, 16, 32, 64, 128]);
    assert_eq!((a.clock.min_hz, a.clock.max_hz), (Some(50e3), Some(1e6)));
    assert_eq!(a.clock.full_resolution_max_hz, Some(200e3));
    // 16 MHz / 128 = 125 kHz sits in the full-resolution window; /16 = 1 MHz
    // is the top of the window and above the full-resolution one.
    let f = |p: u32| 16e6 / f64::from(p);
    assert!(f(128) >= a.clock.full_resolution_min_hz.unwrap());
    assert!(f(128) <= a.clock.full_resolution_max_hz.unwrap());
    assert!(f(16) > a.clock.full_resolution_max_hz.unwrap());
}

/// The other AVR families are each read from their own datasheet, not copied
/// from the `ATmega328P`: the first conversion after enabling the ADC samples at
/// 14.5 cycles on the 1284 and 32U4 and 13.5 on the 328P and `ATtiny85`, the
/// `ATtiny85` has no UART, and every AVR accuracy figure says which column it is.
#[test]
fn avr_families_keep_their_own_datasheet_values() {
    let first = |id: &str| {
        adc(id)
            .conversions
            .iter()
            .find(|c| c.mode == Some(AdcConversionMode::FirstAfterEnable))
            .and_then(|c| c.sample_cycles.first().copied())
            .unwrap_or_else(|| panic!("{id}: no first conversion"))
    };
    assert!((first("atmega328p") - 13.5).abs() < 1e-9);
    assert!((first("atmega1284p") - 14.5).abs() < 1e-9);
    assert!((first("atmega32u4") - 14.5).abs() < 1e-9);
    assert!((first("attiny85") - 13.5).abs() < 1e-9);
    assert!(card("attiny85").peripherals.uart.is_none());
    // The 1284 and 32U4 UARTs are the same asynchronous model as the 328P.
    let err = |id: &str| baud_error_pct(uart(id).baud_hz(16e6, 16, 8).unwrap(), 115_200.0).unwrap();
    for id in ["atmega1284p", "atmega32u4", "atmega328p"] {
        assert!((err(id) + 3.55).abs() < 0.1, "{id}: {}", err(id));
    }
    for id in FILLED {
        let Some(adc) = card(id).peripherals.adc else {
            continue;
        };
        for a in adc.accuracy.iter().filter(|a| a.abs_error_lsb.is_some()) {
            assert!(
                matches!(a.column.as_deref(), Some("typ" | "max")),
                "{id}: accuracy row without a column"
            );
        }
    }
}
