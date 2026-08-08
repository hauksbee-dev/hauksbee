//! The `cs` pin role on the SPI-slave entries, pinned.
//!
//! These two pad maps are load-bearing in a way most are not. The co-sim frames
//! SPI transactions from the chip-select edge stream when it can find the CS net,
//! and falls back to treating the co-sim chunk boundary as a CS deassert when it
//! cannot. That fallback is documented actively wrong: it merges two transactions
//! that share a chunk and truncates one that spans a boundary. The route off it
//! that needs no hand-written `cs_net` in the run spec reads the `cs` role off
//! the bound model, so `"10" = "cs"` on the MCP3008 and `"1" = "cs"` on the 25xx
//! are the difference between exact framing and a guess.
//!
//! Two ways that breaks quietly, both covered below:
//!
//!   - the role is renamed or dropped, and every board using the part silently
//!     drops back to the heuristic. Nothing errors; a report just says
//!     `heuristic` where it said `exact`.
//!   - the role stays but moves to the wrong pad, which is worse. The co-sim then
//!     frames confidently off a pin that is not chip-select, and the framing tier
//!     claims `exact` while the boundaries are fiction.
//!
//! So each test asserts the COMPLETE map against the datasheet pinout, not just
//! the presence of `cs`.

use hauksbee_models::{ComponentQuery, ModelLibrary};

/// Resolve `value` and assert the entry id plus the exact, complete pad->role
/// map (no extra pads, no missing pads, no swaps).
///
/// Sets `mpn` as well as `value`, because both entries carry an `mpn_re` and the
/// matcher ANDs its rules: a value-only query matches neither, while the engine's
/// real query path falls `mpn` back to the value field. Querying with both is
/// what the engine actually does.
fn assert_pin_map(value: &str, id: &str, expected: &[(&str, &str)]) {
    let lib = ModelLibrary::builtin();
    let q = ComponentQuery {
        value: Some(value.into()),
        mpn: Some(value.into()),
        ..Default::default()
    };
    let m = lib
        .resolve(&q)
        .model
        .unwrap_or_else(|| panic!("{value} did not resolve to any model"));
    assert_eq!(m.id, id, "{value} resolved to the wrong model id");
    for (pad, role) in expected {
        assert_eq!(
            m.pins.get(*pad).map(String::as_str),
            Some(*role),
            "{id}: pad {pad} must be role {role:?} (datasheet pinout), got {:?}",
            m.pins.get(*pad),
        );
    }
    assert_eq!(
        m.pins.len(),
        expected.len(),
        "{id}: pad map has {} entries, the datasheet package has {}",
        m.pins.len(),
        expected.len(),
    );
}

/// The pad carrying the `cs` role, or `None` when the entry declares no such
/// role. This is exactly the lookup `binder::model_role_cs_net` performs.
fn cs_pad(value: &str) -> Option<String> {
    let lib = ModelLibrary::builtin();
    let q = ComponentQuery {
        value: Some(value.into()),
        mpn: Some(value.into()),
        ..Default::default()
    };
    let m = lib.resolve(&q).model?;
    m.pins
        .iter()
        .find(|(_, role)| *role == "cs")
        .map(|(pad, _)| pad.clone())
}

// Microchip MCP3004/3008 datasheet DS21295D Section 6.1, PDIP/SOIC/TSSOP-16.
#[test]
fn the_mcp3008_map_is_the_ds21295d_pinout() {
    assert_pin_map(
        "MCP3008",
        "mcp3008",
        &[
            ("1", "ch0"),
            ("2", "ch1"),
            ("3", "ch2"),
            ("4", "ch3"),
            ("5", "ch4"),
            ("6", "ch5"),
            ("7", "ch6"),
            ("8", "ch7"),
            ("9", "dgnd"),
            ("10", "cs"),
            ("11", "mosi"),
            ("12", "miso"),
            ("13", "sck"),
            ("14", "agnd"),
            ("15", "vref"),
            ("16", "vdd"),
        ],
    );
}

// Microchip 25LC256 datasheet DS22065 Section 2.0 / Figure 2-1; the same 8-pin
// arrangement across the 25AA/25LC families.
#[test]
fn the_25xx_eeprom_map_is_the_ds22065_pinout() {
    assert_pin_map(
        "25LC256",
        "eeprom_25xx_spi",
        &[
            ("1", "cs"),
            ("2", "miso"),
            ("3", "wp_n"),
            ("4", "vss"),
            ("5", "mosi"),
            ("6", "sck"),
            ("7", "hold_n"),
            ("8", "vcc"),
        ],
    );
}

/// The role must survive on the pad the co-sim will trace, under the suffixed
/// part numbers a real BOM carries rather than the bare ones above.
#[test]
fn every_spi_slave_entry_offers_a_cs_pad_under_real_part_numbers() {
    for (value, expected_pad) in [
        ("MCP3008", "10"),
        ("MCP3008-I/P", "10"),
        ("MCP3008-I/SL", "10"),
        ("25LC256", "1"),
        ("25LC256-I/SN", "1"),
        ("25AA010A-I/OT", "1"),
        ("25LC1024-I/SM", "1"),
    ] {
        assert_eq!(
            cs_pad(value).as_deref(),
            Some(expected_pad),
            "{value} must expose a `cs` role on pad {expected_pad}; without it the co-sim \
             drops to the chunk-boundary framing heuristic with no error"
        );
    }
}

/// The match rules must not reach past the SPI parts. `24LC256` is the I2C
/// sibling and has no chip-select at all; `2512` is a resistor package code that
/// starts with the same two digits; a bare family prefix names no part.
#[test]
fn the_match_rules_do_not_swallow_neighbouring_part_numbers() {
    for value in ["24LC256", "24AA256", "2512", "25LC", "25"] {
        let lib = ModelLibrary::builtin();
        let q = ComponentQuery {
            value: Some(value.into()),
            mpn: Some(value.into()),
            ..Default::default()
        };
        let id = lib.resolve(&q).model.map(|m| m.id.clone());
        assert_ne!(
            id.as_deref(),
            Some("eeprom_25xx_spi"),
            "{value} must not resolve to the 25xx SPI EEPROM: binding it would put a `cs` \
             role on a pad that is not chip-select, and the co-sim would then report exact \
             framing off a fictional edge stream"
        );
    }
}
