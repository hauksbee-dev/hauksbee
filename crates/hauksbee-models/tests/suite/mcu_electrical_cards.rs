//! The pad drive-strength and reset tables the co-simulator reads
//! (`[[models.output_drive]]`, `[models.drive_select]`, `[models.reset]`):
//! what each card must carry, that the module cards repeat their chip's table
//! exactly, and that validation refuses the shapes that would mislead.

use super::*;
use hauksbee_models::validation::validate;
use hauksbee_models::{OutputLevel, OutputPoint};

fn chip(value: &str, footprint: Option<&str>) -> ModelEntry {
    expect_query(Some(value), Some(value), footprint)
}

/// RP2040 datasheet pad drive table (3.3 V): the 12 mA setting is the
/// strongest and the 2 mA setting the weakest, in both directions.
#[test]
fn rp2040_drive_resistance_falls_as_the_setting_rises() {
    let m = chip("RP2040", None);
    assert_eq!(m.id, "rp2040");
    let r = |setting: &str, level| {
        m.output_drive
            .iter()
            .find(|d| d.setting == setting)
            .unwrap_or_else(|| panic!("no {setting} row"))
            .fit(level)
            .unwrap_or_else(|| panic!("{setting} has no {level:?} point"))
    };
    for level in [OutputLevel::High, OutputLevel::Low] {
        let ladder: Vec<f64> = ["2 mA", "4 mA", "8 mA", "12 mA"]
            .iter()
            .map(|s| r(s, level))
            .collect();
        assert!(
            ladder.windows(2).all(|w| w[0] > w[1]),
            "{level:?}: {ladder:?}"
        );
    }
    assert_eq!(
        m.output_drive.iter().filter(|d| d.reset_default).count(),
        1,
        "exactly one reset-default setting"
    );
    let reset = m.output_drive.iter().find(|d| d.reset_default).unwrap();
    assert_eq!(
        reset.selector,
        Some(1),
        "DRIVE resets to 0x1 (4 mA), Table 341"
    );
}

/// The bare chip, its QFN-56 footprint card and the Pico module are the same
/// die with the same pads, so they must state the same table (a copy that
/// drifts would give two boards of one chip different pin strengths).
#[test]
fn rp2040_chip_footprint_and_module_cards_carry_identical_tables() {
    let bare = chip("RP2040", None);
    let qfn = chip(
        "RP2040",
        Some("Package_DFN_QFN:QFN-56-1EP_7x7mm_P0.4mm_EP3.2x3.2mm"),
    );
    let pico = chip("Pico", None);
    assert_eq!(qfn.id, "rp2040_qfn56");
    assert_eq!(pico.id, "rpi_pico");
    assert!(!bare.output_drive.is_empty());
    for other in [&qfn, &pico] {
        assert_eq!(bare.output_drive, other.output_drive, "{}", other.id);
        assert_eq!(bare.drive_select, other.drive_select, "{}", other.id);
    }
    assert!(bare.drive_select.is_some());
}

/// `drive_select` addresses the field the firmware writes: GPIO25 on the
/// RP2040 is `PADS_BANK0` + 0x04 + 4 * 25, bits 5:4.
#[test]
fn rp2040_drive_select_locates_gpio25_pad_register() {
    let m = chip("RP2040", None);
    let sel = m.drive_select.as_ref().expect("drive_select");
    let (addr, shift) = sel.locate('0', 25).expect("GPIO25 located");
    assert_eq!(addr, 0x4001_c068);
    assert_eq!(shift, 4);
    assert_eq!(sel.width, 2);
}

#[test]
fn stm32f103_and_nrf52840_state_a_supply_reset_threshold() {
    let f103 = chip("STM32F103C8T6", None);
    let r = f103.reset.as_ref().expect("F103 reset table");
    let (lo, hi) = (r.assert_below_v.unwrap(), r.release_above_v.unwrap());
    assert!(lo < hi, "hysteresis: falling {lo} below rising {hi}");
    assert!(r.pin_vil_v.unwrap() < r.pin_vih_v.unwrap());
    assert!(r.release_delay_s.unwrap() > 0.0, "the POR temporization");
    let nrf = chip("nRF52840", None);
    let r = nrf.reset.as_ref().expect("nRF reset table");
    assert!(r.assert_below_v.is_some());
}

fn problems(m: &ModelEntry) -> String {
    match validate(m) {
        Ok(()) => String::new(),
        Err(e) => e
            .iter()
            .map(|e| format!("{e:?}"))
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

// The shipped tables are validated when the built-in library loads (`ModelLibrary::builtin`
// panics on a failing entry), so every test above already depends on them passing.

#[test]
fn validation_refuses_misleading_drive_and_reset_tables() {
    // A point whose voltage is outside 0..vdd.
    let mut m = chip("RP2040", None);
    m.output_drive[0].points.push(OutputPoint {
        level: OutputLevel::High,
        current_a: 0.002,
        volts: 99.0,
    });
    assert!(problems(&m).contains("output_drive"), "{}", problems(&m));
    // Two settings that both claim to be the reset default.
    let mut m = chip("RP2040", None);
    for d in &mut m.output_drive {
        d.reset_default = true;
    }
    assert!(problems(&m).contains("reset_default"), "{}", problems(&m));
    // Duplicate selectors.
    let mut m = chip("RP2040", None);
    let s = m.output_drive[0].selector;
    m.output_drive[1].selector = s;
    assert!(
        problems(&m).contains("selectors must be unique"),
        "{}",
        problems(&m)
    );
    // Selectors with no register to read them from.
    let mut m = chip("RP2040", None);
    m.drive_select = None;
    assert!(problems(&m).contains("drive_select"), "{}", problems(&m));
    // A setting with no source.
    let mut m = chip("RP2040", None);
    m.output_drive[0].basis.clear();
    assert!(problems(&m).contains("basis"), "{}", problems(&m));
    // A reset table with a negative threshold.
    let mut m = chip("STM32F103C8T6", None);
    m.reset.as_mut().unwrap().assert_below_v = Some(-1.0);
    assert!(problems(&m).contains("[models.reset]"), "{}", problems(&m));
}
