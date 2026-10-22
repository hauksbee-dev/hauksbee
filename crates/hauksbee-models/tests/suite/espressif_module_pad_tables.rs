//! Espressif module cards carry the MODULE's pad map, not the chip's: a module
//! footprint numbers its pads (65 on the -MINI-1 of the S3 and S2, 29 on the
//! C6-WROOM-1) differently from the bare chip and from the sibling module, so a
//! board built on one of them bound no card (or the wrong one) and a footprint
//! pad swap went unseen. The expectations live in
//! `module_pad_tables/*.tsv`, transcribed from each datasheet's pin-definition
//! table (kind, printed name, GPIO per pad); the cards in `db/mcu.toml` are
//! checked against them pad by pad so neither can drift alone.

use super::{expect_query, query_id, resolve_query};

/// (card id, a value that must resolve to it, the table's file text)
const TABLES: &[(&str, &str, &str)] = &[
    (
        "esp32s3_mini1",
        "ESP32-S3-MINI-1",
        include_str!("module_pad_tables/esp32_s3_mini_1.tsv"),
    ),
    (
        "esp32s2_mini1",
        "ESP32-S2-MINI-1",
        include_str!("module_pad_tables/esp32_s2_mini_1.tsv"),
    ),
    (
        "esp32c6_wroom1",
        "ESP32-C6-WROOM-1",
        include_str!("module_pad_tables/esp32_c6_wroom_1.tsv"),
    ),
];

/// The role family a table row's kind demands of the card.
fn role_matches(kind: &str, gpio: &str, role: &str) -> bool {
    match kind {
        "gnd" => role == "gnd" || role.strip_prefix("gnd").is_some_and(digits),
        "nc" => role == "nc" || role.strip_prefix("nc").is_some_and(digits),
        "vdd" => role == "vdd",
        "en" => role == "en",
        // A GPIO pad is `gpioN`, or `gpioN_<uart signal>` where the datasheet
        // also names the pad TXD0/RXD0.
        "io" => {
            let own = format!("gpio{gpio}");
            role == own
                || role
                    .strip_prefix(&format!("{own}_"))
                    .is_some_and(|rest| matches!(rest, "txd0" | "rxd0"))
        }
        other => panic!("unknown kind {other}"),
    }
}

fn digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

#[test]
fn every_module_card_matches_its_datasheet_pad_table_pad_by_pad() {
    for (id, value, table) in TABLES {
        let model = expect_query(Some(value), None, None);
        assert_eq!(model.id, *id, "{value}");
        let mut seen = 0;
        for line in table.lines().filter(|l| !l.starts_with('#')) {
            let [pad, printed, kind, gpio] = line.split('\t').collect::<Vec<_>>()[..] else {
                panic!("{id}: malformed row {line:?}");
            };
            let role = model
                .pins
                .get(pad)
                .unwrap_or_else(|| panic!("{id}: pad {pad} ({printed}) has no role"));
            assert!(
                role_matches(kind, gpio, role),
                "{id}: pad {pad} is printed {printed} ({kind}) in the datasheet but the card says {role}"
            );
            seen += 1;
        }
        assert_eq!(
            model.pins.len(),
            seen,
            "{id}: the card maps pads the datasheet table does not list"
        );
    }
}

#[test]
fn module_cards_are_sourced_and_do_not_claim_a_core_they_lack() {
    for (id, value, _) in TABLES {
        let resolution = resolve_query(Some(value), None, None);
        assert_eq!(resolution.references.len(), 1, "{id}");
        let reference = &resolution.references[0];
        assert!(
            reference.url.starts_with("https://www.espressif.com/"),
            "{id}"
        );
        assert_eq!(reference.sha256.as_deref().map(str::len), Some(64), "{id}");
        assert!(reference.locator.contains("Pin Definitions"), "{id}");
    }
    // Only the S3 has a QEMU core; the S2 has none, the C6 has no descriptor.
    let backend = |v: &str| {
        expect_query(Some(v), None, None)
            .params
            .get_str("backend")
            .map(str::to_string)
    };
    assert_eq!(backend("ESP32-S3-MINI-1").as_deref(), Some("qemu:esp32s3"));
    assert_eq!(backend("ESP32-S2-MINI-1"), None);
    assert_eq!(backend("ESP32-C6-WROOM-1").as_deref(), Some("none:esp32c6"));
}

#[test]
fn the_module_straps_sit_on_the_modules_own_pads() {
    // S3-MINI-1 Table 3-1: IO0 pad 4, IO45 pad 41, IO46 pad 44, EN pad 45. The
    // WROOM-1 has IO0 on pad 27 and the MINI-1 has IO47 there, so a map copied
    // from the sibling module would put the BOOT strap on the wrong pad.
    let s3 = expect_query(Some("ESP32-S3-MINI-1"), None, None);
    for (pad, role) in [
        ("4", "gpio0"),
        ("41", "gpio45"),
        ("44", "gpio46"),
        ("45", "en"),
        ("27", "gpio47"),
    ] {
        assert_eq!(
            s3.pins.get(pad).map(String::as_str),
            Some(role),
            "S3 pad {pad}"
        );
    }
    let s3_straps: Vec<&str> = s3.straps.iter().map(|s| s.role.as_str()).collect();
    for role in ["en", "gpio0", "gpio45", "gpio46"] {
        assert!(s3_straps.contains(&role), "S3 strap {role}");
    }
    // C6-WROOM-1 Table 3-1: EN pad 3, IO9 is the BOOT strap.
    let c6 = expect_query(Some("ESP32-C6-WROOM-1"), None, None);
    assert_eq!(c6.pins.get("3").map(String::as_str), Some("en"));
    let boot = c6
        .straps
        .iter()
        .find(|s| s.role == "gpio9")
        .expect("GPIO9 strap");
    assert!(boot.boot_select);
    let pad = c6
        .pins
        .iter()
        .find(|(_, role)| role.as_str() == "gpio9")
        .map(|(pad, _)| pad.as_str());
    assert!(pad.is_some(), "the card maps GPIO9");
}

#[test]
fn module_ordering_codes_resolve_and_neighbouring_modules_do_not_bleed_in() {
    for (value, id) in [
        ("ESP32-S3-MINI-1U", "esp32s3_mini1"),
        ("ESP32-S3-MINI-1-N8", "esp32s3_mini1"),
        ("ESP32-S3-MINI-1U-N4R2", "esp32s3_mini1"),
        ("ESP32-S3-MINI-1-H4R2", "esp32s3_mini1"),
        ("ESP32-S2-MINI-1U", "esp32s2_mini1"),
        ("ESP32-S2-MINI-1-N4R2", "esp32s2_mini1"),
        ("ESP32-C6-WROOM-1U", "esp32c6_wroom1"),
        ("ESP32-C6-WROOM-1-N8", "esp32c6_wroom1"),
        // The classic module's WROVER spellings share the WROOM-32 pad map
        // (WROVER-E Datasheet Table 3-1 and WROOM-32E Table 3-1 list the same
        // pads); the "32" of the name is optional there.
        ("ESP32-WROVER-E", "esp32_wroom"),
        ("ESP32-WROVER-IE", "esp32_wroom"),
        ("ESP32-WROVER-B", "esp32_wroom"),
        ("ESP32-WROOM-32E", "esp32_wroom"),
    ] {
        assert_eq!(
            query_id(Some(value), None, None).as_deref(),
            Some(id),
            "{value}"
        );
    }
    // Other modules keep their own cards: the MINI-1 map is not the WROOM-1's.
    assert_eq!(
        query_id(
            Some("ESP32-S3-WROOM-1"),
            None,
            Some("RF_Module:ESP32-S3-WROOM-1")
        )
        .as_deref(),
        Some("esp32s3_wroom")
    );
    assert_eq!(
        query_id(Some("ESP32-C6-MINI-1"), None, None).as_deref(),
        Some("esp32c6_mini1")
    );
    // A different suffix is not a variant of the module.
    assert_ne!(
        query_id(Some("ESP32-S3-MINI-2"), None, None).as_deref(),
        Some("esp32s3_mini1")
    );
}
