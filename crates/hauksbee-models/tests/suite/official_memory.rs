//! Official-parts batch memory: resolution and behaviour tests for the cards added in that batch
//! (I2C EEPROMs in `memory.toml`, SPI NOR flash and SPI SRAM in `memory_spi.toml`).
//!
//! Every card is resolved by a real ordering code, its complete pad map, supply range, typed
//! peripheral and datasheet sha are asserted, and sibling part numbers (the other density, the
//! other voltage family, the other generation) are asserted NOT to resolve to it.

use super::{assert_pin_map, expect_query, has_supply_range, pins, query_id};
use hauksbee_models::{ModelEntry, PeripheralSpec};

const SOIC8: &str = "Package_SO:SOIC-8_3.9x4.9mm_P1.27mm";
const SOIC8_208: &str = "Package_SO:SOIC-8_5.23x5.23mm_P1.27mm";
const SOT23_5: &str = "Package_TO_SOT_SMD:SOT-23-5";
const MSOP8: &str = "Package_SO:MSOP-8_3x3mm_P0.65mm";
const SOIC16: &str = "Package_SO:SOIC-16W_7.5x10.3mm_P1.27mm";

/// Resolve the way a layout-only board does: the value doubles as the MPN.
fn bind(value: &str, footprint: Option<&str>) -> ModelEntry {
    expect_query(Some(value), Some(value), footprint)
}

/// `value` must not bind the card `id` (it may bind another card, or nothing).
fn assert_not(card: &str, value: &str, footprint: Option<&str>) {
    let got = query_id(Some(value), Some(value), footprint);
    assert_ne!(
        got.as_deref(),
        Some(card),
        "{value} ({footprint:?}) must not resolve to {card}"
    );
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-12
}

fn assert_has_sha(value: &str, footprint: Option<&str>, sha: &str) {
    let res = super::resolve_query(Some(value), Some(value), footprint);
    assert!(
        res.references
            .iter()
            .any(|r| r.sha256.as_deref() == Some(sha)
                && !r.url.is_empty()
                && !r.locator.is_empty()),
        "{value}: no reference carrying sha {sha}"
    );
}

struct Eeprom {
    value: &'static str,
    footprint: Option<&'static str>,
    id: &'static str,
    pins: &'static str,
    supply: (f64, f64, f64),
    cites: &'static str,
    size: usize,
    page: usize,
    word: u8,
    power: (f64, f64, f64, f64),
    sha: &'static str,
}

fn check_eeprom(c: &Eeprom) {
    let m = bind(c.value, c.footprint);
    assert_pin_map(&m, c.id, &pins(c.pins));
    assert!(
        has_supply_range(
            &m,
            "vcc",
            (c.supply.0, c.supply.1),
            Some(c.supply.2),
            c.cites
        ),
        "{}: supply range {:?} citing {}",
        c.id,
        c.supply,
        c.cites
    );
    assert_eq!(
        m.peripheral,
        Some(PeripheralSpec::I2cEeprom {
            address: 80,
            size_bytes: c.size,
            page_size: c.page,
            word_address_bytes: c.word,
        }),
        "{}: typed i2c_eeprom peripheral",
        c.id
    );
    let p = m.peripheral_power.as_ref().expect("bus-coupled power");
    assert_eq!(
        (p.supply_role.as_str(), p.return_role.as_str()),
        ("vcc", "gnd")
    );
    assert!(
        close(p.power_on_threshold_v, c.power.0),
        "{} threshold",
        c.id
    );
    assert!(close(p.idle_a, c.power.1), "{} idle", c.id);
    assert!(close(p.read_a, c.power.2), "{} read", c.id);
    assert!(close(p.write_a, c.power.3), "{} write", c.id);
    assert_has_sha(c.value, c.footprint, c.sha);
}

const AT24C02D_SHA: &str = "397ff061aa7a4ae36864592813831e9386e9467f10049064d7f139f26be6475e";
const AT24C32D_SHA: &str = "5d626a1bec7f0c525323f9251abaa890c37fe009036ffcef27218880c8251ca4";
const AT24C256C_SHA: &str = "60d5cadc56737978e0f90127a95fb32bd347ed2415ef101cb23526126daf986e";
const MCHP256_SHA: &str = "48ba5ace395dbe7b070914acf982e366cbe1a2e44ec036d63903471646b26ec7";

const EIGHT_PIN_EEPROM: &str = "1=a0 2=a1 3=a2 4=gnd 5=sda 6=scl 7=wp 8=vcc";
const SOT23_5_EEPROM: &str = "1=scl 2=gnd 3=sda 4=vcc 5=wp";
const MSOP_EEPROM: &str = "1=nc 2=nc 3=a2 4=gnd 5=sda 6=scl 7=wp 8=vcc";
const CSP_EEPROM: &str = "1=vcc 2=a1 3=a0 4=wp 5=a2 6=sda 7=scl 8=gnd";

#[test]
fn at24c02d_cards_bind_by_ordering_code_and_package() {
    for (value, footprint, id, pin_spec) in [
        (
            "AT24C02D-SSHM-T",
            Some(SOIC8),
            "at24c02d_8pin",
            EIGHT_PIN_EEPROM,
        ),
        ("AT24C02D-XHM-B", None, "at24c02d_8pin", EIGHT_PIN_EEPROM),
        ("AT24C02D", None, "at24c02d_8pin", EIGHT_PIN_EEPROM),
        (
            "AT24C02D-STUM-T",
            Some(SOT23_5),
            "at24c02d_sot23_5",
            SOT23_5_EEPROM,
        ),
    ] {
        check_eeprom(&Eeprom {
            value,
            footprint,
            id,
            pins: pin_spec,
            supply: (1.7, 3.6, 4.1),
            cites: "Atmel-8871F",
            size: 256,
            page: 8,
            word: 1,
            power: (1.7, 0.8e-6, 0.5e-3, 1.0e-3),
            sha: AT24C02D_SHA,
        });
    }
    for sibling in [
        "AT24C02C",
        "AT24C02C-SSHM-T",
        "AT24C04D",
        "AT24C01D",
        "AT24C32D",
        "AT24CS02",
        "AT24C02",
    ] {
        for card in ["at24c02d_8pin", "at24c02d_sot23_5"] {
            assert_not(card, sibling, None);
        }
    }
}

#[test]
fn at24c32d_cards_bind_by_ordering_code_and_package() {
    for (value, footprint, id, pin_spec) in [
        (
            "AT24C32D-SSHM-T",
            Some(SOIC8),
            "at24c32d_8pin",
            EIGHT_PIN_EEPROM,
        ),
        ("AT24C32D-MAHM-T", None, "at24c32d_8pin", EIGHT_PIN_EEPROM),
        ("AT24C32D", None, "at24c32d_8pin", EIGHT_PIN_EEPROM),
        (
            "AT24C32D-STUM-T",
            Some(SOT23_5),
            "at24c32d_sot23_5",
            SOT23_5_EEPROM,
        ),
    ] {
        check_eeprom(&Eeprom {
            value,
            footprint,
            id,
            pins: pin_spec,
            supply: (1.7, 5.5, 6.25),
            cites: "AT24C32D",
            size: 4096,
            page: 32,
            word: 2,
            power: (1.7, 6.0e-6, 1.0e-3, 3.0e-3),
            sha: AT24C32D_SHA,
        });
    }
    for sibling in [
        "AT24C32E",
        "AT24C32E-SSHM-T",
        "AT24C32C",
        "AT24C64D",
        "AT24C16D",
        "AT24C32",
    ] {
        for card in ["at24c32d_8pin", "at24c32d_sot23_5"] {
            assert_not(card, sibling, None);
        }
    }
}

#[test]
fn at24c256c_binds_by_ordering_code_and_siblings_do_not() {
    for value in [
        "AT24C256C-SSHL-T",
        "AT24C256C-XHL-B",
        "AT24C256C-MAHL-T",
        "AT24C256C",
    ] {
        check_eeprom(&Eeprom {
            value,
            footprint: Some(SOIC8),
            id: "at24c256c_8pin",
            pins: EIGHT_PIN_EEPROM,
            supply: (1.7, 5.5, 6.25),
            cites: "Atmel-8568F",
            size: 32768,
            page: 64,
            word: 2,
            power: (1.7, 6.0e-6, 2.0e-3, 3.0e-3),
            sha: AT24C256C_SHA,
        });
    }
    for sibling in [
        "AT24C256",
        "AT24C256B",
        "AT24C512C",
        "AT24C128C",
        "AT24C64D",
        "AT24CM01",
        "BL24C256A",
        "CAT24M01",
        "24LC256",
        "24AA256",
    ] {
        assert_not("at24c256c_8pin", sibling, None);
    }
}

#[test]
fn the_24xx256_family_splits_by_voltage_grade_and_package() {
    // 24AA256 and 24FC256 share a card (both 1.7 V to 5.5 V, same pin map and currents).
    for (value, footprint, id, pin_spec) in [
        (
            "24AA256-I/SN",
            Some(SOIC8),
            "24aa256_8pin",
            EIGHT_PIN_EEPROM,
        ),
        ("24AA256T-I/ST", None, "24aa256_8pin", EIGHT_PIN_EEPROM),
        ("24FC256-I/P", None, "24aa256_8pin", EIGHT_PIN_EEPROM),
        ("24AA256", None, "24aa256_8pin", EIGHT_PIN_EEPROM),
        ("24AA256-I/MS", Some(MSOP8), "24aa256_msop8", MSOP_EEPROM),
        ("24FC256T-E/MS", Some(MSOP8), "24aa256_msop8", MSOP_EEPROM),
        ("24AA256T-I/CS16K", None, "24aa256_csp8", CSP_EEPROM),
        ("24AA256T-CS16K", None, "24aa256_csp8", CSP_EEPROM),
    ] {
        check_eeprom(&Eeprom {
            value,
            footprint,
            id,
            pins: pin_spec,
            supply: (1.7, 5.5, 6.5),
            cites: "DS20001203W",
            size: 32768,
            page: 64,
            word: 2,
            power: (1.7, 5.0e-6, 400e-6, 3.0e-3),
            sha: MCHP256_SHA,
        });
    }
    // 24LC256 is the 2.5 V part and must not collapse into the 1.7 V card.
    for (value, footprint, id, pin_spec) in [
        (
            "24LC256-I/SN",
            Some(SOIC8),
            "24lc256_8pin",
            EIGHT_PIN_EEPROM,
        ),
        ("24LC256T-E/ST", None, "24lc256_8pin", EIGHT_PIN_EEPROM),
        ("24LC256-I/P", None, "24lc256_8pin", EIGHT_PIN_EEPROM),
        ("24LC256", None, "24lc256_8pin", EIGHT_PIN_EEPROM),
        ("24LC256-I/MS", Some(MSOP8), "24lc256_msop8", MSOP_EEPROM),
    ] {
        check_eeprom(&Eeprom {
            value,
            footprint,
            id,
            pins: pin_spec,
            supply: (2.5, 5.5, 6.5),
            cites: "DS20001203W",
            size: 32768,
            page: 64,
            word: 2,
            power: (2.5, 5.0e-6, 400e-6, 3.0e-3),
            sha: MCHP256_SHA,
        });
    }
    for card in ["24aa256_8pin", "24aa256_msop8", "24aa256_csp8"] {
        assert_not(card, "24LC256", None);
        assert_not(card, "24LC256-I/SN", Some(SOIC8));
    }
    for card in ["24lc256_8pin", "24lc256_msop8"] {
        assert_not(card, "24AA256", None);
        assert_not(card, "24FC256-I/SN", Some(SOIC8));
    }
    for sibling in [
        "24LC128",
        "24LC512",
        "24AA512",
        "24LC64",
        "24AA025",
        "24AA025UID",
        "24LC025",
        "24LC32A",
        "25LC256",
        "24LC256SC",
    ] {
        for card in [
            "24aa256_8pin",
            "24aa256_msop8",
            "24aa256_csp8",
            "24lc256_8pin",
            "24lc256_msop8",
        ] {
            assert_not(card, sibling, None);
        }
    }
    // A CSP-only code never lands on a leaded pad map, and the 24LC256 has no CSP card.
    assert_not("24aa256_8pin", "24AA256T-I/CS16K", None);
    assert_not("24aa256_csp8", "24LC256T-I/CS16K", None);
}

struct Flash {
    value: &'static str,
    footprint: Option<&'static str>,
    id: &'static str,
    pins: &'static str,
    supply: (f64, f64, f64),
    cites: &'static str,
    size: usize,
    jedec: [u8; 3],
    /// idle, read, write, low-power
    power: (f64, f64, f64, f64),
    sha: &'static str,
}

fn check_flash(c: &Flash) {
    let m = bind(c.value, c.footprint);
    assert_pin_map(&m, c.id, &pins(c.pins));
    assert!(
        has_supply_range(
            &m,
            "vcc",
            (c.supply.0, c.supply.1),
            Some(c.supply.2),
            c.cites
        ),
        "{}: supply range {:?} citing {}",
        c.id,
        c.supply,
        c.cites
    );
    match &m.peripheral {
        Some(PeripheralSpec::SpiNorFlash {
            size_bytes,
            page_size,
            sector_size,
            jedec_id,
            spi_mode,
            cs_role,
            clk_role,
            mosi_role,
            miso_role,
        }) => {
            assert_eq!(*size_bytes, c.size, "{} size", c.id);
            assert_eq!(
                (*page_size, *sector_size, *spi_mode),
                (256, 4096, 0),
                "{}",
                c.id
            );
            assert_eq!(jedec_id.as_slice(), c.jedec.as_slice(), "{} JEDEC ID", c.id);
            assert_eq!(
                (
                    cs_role.as_str(),
                    clk_role.as_str(),
                    mosi_role.as_str(),
                    miso_role.as_str()
                ),
                ("cs", "sck", "mosi", "miso"),
                "{}",
                c.id
            );
            // The role names the peripheral drives must exist as pad roles of the card.
            for role in [cs_role, clk_role, mosi_role, miso_role] {
                assert!(
                    m.pins.values().any(|r| r == role),
                    "{}: no pad with role {role}",
                    c.id
                );
            }
        }
        other => panic!("{}: expected spi_nor_flash, got {other:?}", c.id),
    }
    let p = m.peripheral_power.as_ref().expect("bus-coupled power");
    assert_eq!(
        (p.supply_role.as_str(), p.return_role.as_str()),
        ("vcc", "gnd")
    );
    assert!(
        close(p.power_on_threshold_v, c.supply.0),
        "{} threshold",
        c.id
    );
    assert!(close(p.idle_a, c.power.0), "{} idle", c.id);
    assert!(close(p.read_a, c.power.1), "{} read", c.id);
    assert!(close(p.write_a, c.power.2), "{} write", c.id);
    assert_eq!(
        p.low_power_a.map(|v| (v - c.power.3).abs() < 1e-12),
        Some(true),
        "{} low power",
        c.id
    );
    assert_has_sha(c.value, c.footprint, c.sha);
}

const PINS8: &str = "1=cs 2=miso 3=wp_n 4=gnd 5=mosi 6=sck 7=hold_n 8=vcc";
const PINS16: &str = "1=hold_n 2=vcc 3=nc 4=nc 5=nc 6=nc 7=cs 8=miso 9=wp_n 10=gnd 11=nc 12=nc 13=nc 14=nc 15=mosi 16=sck";
/// W25Q64JV 16-pin SOIC: pad 3 is /RESET (Rev J 3.5, PDF p.6).
const PINS16_W25: &str = "1=hold_n 2=vcc 3=reset_n 4=nc 5=nc 6=nc 7=cs 8=miso 9=wp_n 10=gnd 11=nc 12=nc 13=nc 14=nc 15=mosi 16=sck";

#[test]
fn winbond_jv_cards_bind_their_density_and_package_family() {
    let w64 = "452d906c3be0da077fd751a2b8aa062629caf6081f8dd1eda04880854ce7c8f9";
    let w16 = "6d23cdf57fd63fc2c2de517707e293beaf70f3f47b424bf58510666294c04d82";
    for (value, footprint) in [
        ("W25Q64JVSSIQ", Some(SOIC8_208)),
        ("W25Q64JVSSIQ-TR", None),
        ("W25Q64JVZPIQ", None),
        ("W25Q64JVXGIQ", None),
        ("W25Q64JVSSJQ", None),
        ("W25Q64JV", None),
    ] {
        check_flash(&Flash {
            value,
            footprint,
            id: "w25q64jv_soic8",
            pins: PINS8,
            supply: (2.7, 3.6, 4.6),
            cites: "W25Q64JV",
            size: 8_388_608,
            jedec: [0xEF, 0x40, 0x17],
            power: (50e-6, 20e-3, 25e-3, 15e-6),
            sha: w64,
        });
    }
    check_flash(&Flash {
        value: "W25Q64JVSFIQ",
        footprint: Some(SOIC16),
        id: "w25q64jv_soic16",
        pins: PINS16_W25,
        supply: (2.7, 3.6, 4.6),
        cites: "W25Q64JV",
        size: 8_388_608,
        jedec: [0xEF, 0x40, 0x17],
        power: (50e-6, 20e-3, 25e-3, 15e-6),
        sha: w64,
    });
    for value in [
        "W25Q16JVSNIQ",
        "W25Q16JVSSIQ",
        "W25Q16JVZPIQ",
        "W25Q16JVUXIQ",
        "W25Q16JVUUIQ",
        "W25Q16JVXGIQ",
        "W25Q16JVSNJQ",
        "W25Q16JV",
    ] {
        check_flash(&Flash {
            value,
            footprint: None,
            id: "w25q16jv_soic8",
            pins: PINS8,
            supply: (2.7, 3.6, 4.6),
            cites: "W25Q16JV",
            size: 2_097_152,
            jedec: [0xEF, 0x40, 0x15],
            power: (50e-6, 20e-3, 25e-3, 15e-6),
            sha: w16,
        });
    }
    // The -IM/JM grade answers another JEDEC ID, other densities and generations are other cards.
    for sibling in [
        "W25Q64JVSSIM",
        "W25Q64FVSSIG",
        "W25Q64FV",
        "W25Q64JWSSIQ",
        "W25Q64DW",
        "W25Q32JVSSIQ",
        "W25Q128JVSIQ",
        "W25Q16JVSSIM",
        "W25Q16JWSNIQ",
        "W25Q16DVSNIG",
        "W25Q16FWSVIG",
        "W25Q80DV",
    ] {
        for card in ["w25q64jv_soic8", "w25q64jv_soic16", "w25q16jv_soic8"] {
            assert_not(card, sibling, None);
        }
    }
    assert_not("w25q64jv_soic8", "W25Q64JVSFIQ", None);
    assert_not("w25q64jv_soic16", "W25Q64JVSSIQ", None);
    assert_not("w25q16jv_soic8", "W25Q64JVSSIQ", None);
}

#[test]
fn gigadevice_cards_bind_the_generation_that_was_read() {
    let g32 = "ce541db9d2d3efc21d9fb5e958ae2df481d5a9692fe42abf8f3a46ec8d627c38";
    let g64 = "5a1a318a367d3a2d1106054c7583551a12028b04ff1eaccd69cb2e8bd25ad18d";
    let g128 = "eccb484055d9936b157ad7bf7eebce0a0e30e5ffd9385a197f0fa6fcb2b3c223";
    for value in [
        "GD25Q32CTIGR",
        "GD25Q32CSIG",
        "GD25Q32CWIGR",
        "GD25Q32CHIG",
        "GD25Q32C",
        "GD25Q32",
    ] {
        check_flash(&Flash {
            value,
            footprint: None,
            id: "gd25q32c_8pin",
            pins: PINS8,
            supply: (2.7, 3.6, 4.2),
            cites: "GD25Q32C",
            size: 4_194_304,
            jedec: [0xC8, 0x40, 0x16],
            power: (5e-6, 20e-3, 25e-3, 5e-6),
            sha: g32,
        });
    }
    for value in [
        "GD25Q64CSIG",
        "GD25Q64CWIGR",
        "GD25Q64CQIG",
        "GD25Q64CSIGR",
        "GD25Q64C",
        "GD25Q64",
    ] {
        check_flash(&Flash {
            value,
            footprint: None,
            id: "gd25q64c_8pin",
            pins: PINS8,
            supply: (2.7, 3.6, 4.2),
            cites: "GD25Q64C",
            size: 8_388_608,
            jedec: [0xC8, 0x40, 0x17],
            power: (5e-6, 20e-3, 20e-3, 5e-6),
            sha: g64,
        });
    }
    check_flash(&Flash {
        value: "GD25Q64CFIG",
        footprint: Some(SOIC16),
        id: "gd25q64c_sop16",
        pins: PINS16,
        supply: (2.7, 3.6, 4.2),
        cites: "GD25Q64C",
        size: 8_388_608,
        jedec: [0xC8, 0x40, 0x17],
        power: (5e-6, 20e-3, 20e-3, 5e-6),
        sha: g64,
    });
    for value in [
        "GD25Q128ESIG",
        "GD25Q128EWIGR",
        "GD25Q128EQIG",
        "GD25Q128E",
        "GD25Q128",
    ] {
        check_flash(&Flash {
            value,
            footprint: None,
            id: "gd25q128e_8pin",
            pins: PINS8,
            supply: (2.7, 3.6, 4.2),
            cites: "GD25Q128E",
            size: 16_777_216,
            jedec: [0xC8, 0x40, 0x18],
            power: (50e-6, 20e-3, 25e-3, 8e-6),
            sha: g128,
        });
    }
    check_flash(&Flash {
        value: "GD25Q128EFIR",
        footprint: Some(SOIC16),
        id: "gd25q128e_sop16",
        pins: "1=hold_n 2=vcc 3=reset_n 4=nc 5=nc 6=nc 7=cs 8=miso 9=wp_n 10=gnd 11=nc 12=nc 13=nc 14=nc 15=mosi 16=sck",
        supply: (2.7, 3.6, 4.2),
        cites: "GD25Q128E",
        size: 16_777_216,
        jedec: [0xC8, 0x40, 0x18],
        power: (50e-6, 20e-3, 25e-3, 8e-6),
        sha: g128,
    });
}

#[test]
fn gigadevice_cards_reject_other_generations_and_option_codes() {
    let all = [
        "gd25q32c_8pin",
        "gd25q64c_8pin",
        "gd25q64c_sop16",
        "gd25q128e_8pin",
        "gd25q128e_sop16",
    ];
    for sibling in [
        "GD25Q16",
        "GD25Q16CSIG",
        "GD25Q32E",
        "GD25Q32ESIG",
        "GD25Q32B",
        "GD25Q64E",
        "GD25Q64ESIG",
        "GD25Q64B",
        "GD25Q128C",
        "GD25Q128CSIG",
        "GD25Q256D",
        "GD25Q64CSJG",
        "GD25Q64CSEG",
        "GD25Q128ESJG",
        "GD25LQ64C",
        "GD25B64C",
    ] {
        for card in all {
            assert_not(card, sibling, None);
        }
    }
    // The G/S-option SOP16 carries DNU on pad 3 (not RESET#), so only R/K codes bind the RESET# card.
    assert_not("gd25q128e_sop16", "GD25Q128EFIG", None);
    assert_not("gd25q128e_8pin", "GD25Q128EFIR", None);
    assert_not("gd25q64c_8pin", "GD25Q64CFIG", None);
}

#[test]
fn macronix_cards_split_density_and_pin_one_function() {
    let m64 = "f9841f0fdc919d257c602f62fe3a2fa9254c2bb89bd6585599fbd73c5367d30d";
    let m32 = "c34badd3067e5f29ccc790fbf9d0e37ccc4b29d7f9384cc830bf478b96f2822d";
    for value in [
        "MX25L6433FM2I-08G",
        "MX25L6433FZNI-08Q",
        "MX25L6433FZ2I-08G",
        "MX25L6433F",
    ] {
        check_flash(&Flash {
            value,
            footprint: None,
            id: "mx25l6433f_8pin",
            pins: PINS8,
            supply: (2.65, 3.6, 4.6),
            cites: "MX25L6433F",
            size: 8_388_608,
            jedec: [0xC2, 0x20, 0x17],
            power: (50e-6, 17e-3, 15e-3, 20e-6),
            sha: m64,
        });
    }
    check_flash(&Flash {
        value: "MX25L6433FMI-08G",
        footprint: Some(SOIC16),
        id: "mx25l6433f_sop16",
        pins: PINS16,
        supply: (2.65, 3.6, 4.6),
        cites: "MX25L6433F",
        size: 8_388_608,
        jedec: [0xC2, 0x20, 0x17],
        power: (50e-6, 17e-3, 15e-3, 20e-6),
        sha: m64,
    });
    for value in [
        "MX25L3233FM1I-08G",
        "MX25L3233FM2I-08Q",
        "MX25L3233FZBI-08G",
        "MX25L3233FZNI-08G",
        "MX25L3233FZCI-08G",
        "MX25L3233F",
    ] {
        check_flash(&Flash {
            value,
            footprint: None,
            id: "mx25l3233f_8pin",
            pins: PINS8,
            supply: (2.65, 3.6, 4.6),
            cites: "MX25L3233F",
            size: 4_194_304,
            jedec: [0xC2, 0x20, 0x16],
            power: (50e-6, 17e-3, 15e-3, 20e-6),
            sha: m32,
        });
    }
    check_flash(&Flash {
        value: "MX25L3233FMI-08G",
        footprint: Some(SOIC16),
        id: "mx25l3233f_sop16",
        pins: PINS16,
        supply: (2.65, 3.6, 4.6),
        cites: "MX25L3233F",
        size: 4_194_304,
        jedec: [0xC2, 0x20, 0x16],
        power: (50e-6, 17e-3, 15e-3, 20e-6),
        sha: m32,
    });
}

#[test]
fn macronix_mx25l12835f_pad_7_is_reset_and_sop16_pad_1_is_sio3() {
    let m128 = "66e38de394d725c668b469b65a9fb17604ce71b2356af516ed6cd923f3814744";
    // MX25L12835F: pad 7 is RESET#/SIO3 (not HOLD#) and the 16-pin pad 1 is DNU/SIO3.
    for value in [
        "MX25L12835FM2I-10G",
        "MX25L12835FZNI-10G",
        "MX25L12835FZ2I-10G",
        "MX25L12835F",
    ] {
        check_flash(&Flash {
            value,
            footprint: None,
            id: "mx25l12835f_8pin",
            pins: "1=cs 2=miso 3=wp_n 4=gnd 5=mosi 6=sck 7=reset_n 8=vcc",
            supply: (2.7, 3.6, 4.0),
            cites: "MX25L12835F",
            size: 16_777_216,
            jedec: [0xC2, 0x20, 0x18],
            power: (50e-6, 25e-3, 25e-3, 20e-6),
            sha: m128,
        });
    }
    check_flash(&Flash {
        value: "MX25L12835FMI-10G",
        footprint: Some(SOIC16),
        id: "mx25l12835f_sop16",
        pins: "1=sio3 2=vcc 3=reset_n 4=nc 5=nc 6=nc 7=cs 8=miso 9=wp_n 10=gnd 11=nc 12=nc 13=nc 14=nc 15=mosi 16=sck",
        supply: (2.7, 3.6, 4.0),
        cites: "MX25L12835F",
        size: 16_777_216,
        jedec: [0xC2, 0x20, 0x18],
        power: (50e-6, 25e-3, 25e-3, 20e-6),
        sha: m128,
    });
}

#[test]
fn macronix_cards_reject_other_densities_and_families() {
    let all = [
        "mx25l6433f_8pin",
        "mx25l6433f_sop16",
        "mx25l3233f_8pin",
        "mx25l3233f_sop16",
        "mx25l12835f_8pin",
        "mx25l12835f_sop16",
    ];
    for sibling in [
        "MX25L6406E",
        "MX25L6406EM2I-12G",
        "MX25L6435E",
        "MX25L6473F",
        "MX25L3206E",
        "MX25L3273F",
        "MX25L12805D",
        "MX25L12873F",
        "MX25L1606E",
        "MX25L25645G",
        "MX25R6435F",
        "MX25U6433F",
    ] {
        for card in all {
            assert_not(card, sibling, None);
        }
    }
    // Each density's ordering codes stay on that density's card.
    assert_not("mx25l6433f_8pin", "MX25L3233FM1I-08G", None);
    assert_not("mx25l3233f_8pin", "MX25L6433FM2I-08G", None);
    assert_not("mx25l12835f_8pin", "MX25L6433FM2I-08G", None);
    assert_not("mx25l6433f_8pin", "MX25L6433FMI-08G", None);
    assert_not("mx25l6433f_sop16", "MX25L6433FM2I-08G", None);
}

struct Sram {
    value: &'static str,
    id: &'static str,
    pins: &'static str,
    supply: (f64, f64, f64),
    cites: &'static str,
    sha: &'static str,
}

#[test]
fn spi_sram_cards_carry_pins_levels_and_ratings_only() {
    let s1024 = "21929095a86c4d296060a8ceb8363baab17bdb5633ab8492d8fe0259f2595d22";
    let s512 = "a268829a2ccd77a37c80938dc0439959f7ad5f58a4a8718b60943ff83a73d864";
    let s256 = "b35c23fd09f38f12f033d809ced3caca1a7c5e40813c52cabe7a7307e178584f";
    for c in [
        Sram {
            value: "23LC1024-I/SN",
            id: "23lc1024_8pin",
            pins: "1=cs 2=miso 3=sio2 4=gnd 5=mosi 6=sck 7=hold_n 8=vcc",
            supply: (2.5, 5.5, 6.5),
            cites: "DS20005142C",
            sha: s1024,
        },
        Sram {
            value: "23LC1024T-E/ST",
            id: "23lc1024_8pin",
            pins: "1=cs 2=miso 3=sio2 4=gnd 5=mosi 6=sck 7=hold_n 8=vcc",
            supply: (2.5, 5.5, 6.5),
            cites: "DS20005142C",
            sha: s1024,
        },
        Sram {
            value: "23LC512-I/P",
            id: "23lc512_8pin",
            pins: "1=cs 2=miso 3=sio2 4=gnd 5=mosi 6=sck 7=hold_n 8=vcc",
            supply: (2.5, 5.5, 6.5),
            cites: "DS20005155C",
            sha: s512,
        },
        Sram {
            value: "23LC512T-I/SN",
            id: "23lc512_8pin",
            pins: "1=cs 2=miso 3=sio2 4=gnd 5=mosi 6=sck 7=hold_n 8=vcc",
            supply: (2.5, 5.5, 6.5),
            cites: "DS20005155C",
            sha: s512,
        },
        Sram {
            value: "23K256-I/SN",
            id: "23k256_8pin",
            pins: "1=cs 2=miso 3=nc 4=gnd 5=mosi 6=sck 7=hold_n 8=vcc",
            supply: (2.7, 3.6, 4.5),
            cites: "DS22100F",
            sha: s256,
        },
        Sram {
            value: "23K256T-E/P",
            id: "23k256_8pin",
            pins: "1=cs 2=miso 3=nc 4=gnd 5=mosi 6=sck 7=hold_n 8=vcc",
            supply: (2.7, 3.6, 4.5),
            cites: "DS22100F",
            sha: s256,
        },
    ] {
        let m = bind(c.value, None);
        assert_pin_map(&m, c.id, &pins(c.pins));
        assert!(
            has_supply_range(
                &m,
                "vcc",
                (c.supply.0, c.supply.1),
                Some(c.supply.2),
                c.cites
            ),
            "{}: supply range",
            c.id
        );
        // No honest SRAM peripheral kind exists: the card must not pretend to be an executable memory.
        assert!(
            m.peripheral.is_none(),
            "{}: SRAM must carry no peripheral",
            c.id
        );
        assert!(
            m.peripheral_power.is_none(),
            "{}: SRAM must carry no peripheral power",
            c.id
        );
        assert_has_sha(c.value, None, c.sha);
    }
    let all = ["23lc1024_8pin", "23lc512_8pin", "23k256_8pin"];
    for sibling in [
        "23A1024",
        "23A512",
        "23A256",
        "23LC256",
        "23K640",
        "23K640-I/SN",
        "23LCV1024",
        "23LCV512",
        "23A640",
    ] {
        for card in all {
            assert_not(card, sibling, None);
        }
    }
    assert_not("23lc1024_8pin", "23LC512-I/SN", None);
    assert_not("23lc512_8pin", "23LC1024-I/SN", None);
    assert_not("23k256_8pin", "23LC512-I/SN", None);
}
