//! Every card that carries a boot-mode table must be internally consistent:
//! each strap the table reads is a pad on the card with a threshold row and a
//! citation, and the ROM-bootloader modes say what answers.
//!
//! The values themselves are checked against the vendor PDFs when the cards are
//! written; this test only guards the shape the resolver depends on.

use hauksbee_ir::boot::BootOutcome;
use hauksbee_models::boot_mode::{PinLevel, RomProtocol};
use hauksbee_models::schema::{DbFile, ModelEntry};

fn cards() -> Vec<ModelEntry> {
    let text = include_str!("../../db/mcu.toml");
    toml::from_str::<DbFile>(text)
        .expect("mcu.toml parses")
        .models
}

fn with_boot() -> Vec<ModelEntry> {
    cards()
        .into_iter()
        .filter(|c| !c.boot_modes.is_empty())
        .collect()
}

#[test]
fn the_families_the_package_covers_carry_a_boot_table() {
    let have: Vec<String> = with_boot().into_iter().map(|c| c.id).collect();
    for id in [
        "stm32f103c8",
        "stm32f072c",
        "stm32f401c",
        "stm32g474c",
        "stm32l0x1k_lqfp32",
    ] {
        assert!(
            have.iter().any(|h| h == id),
            "{id} has no boot table: {have:?}"
        );
    }
}

#[test]
fn every_strap_the_table_reads_is_a_pad_with_thresholds_and_a_citation() {
    for card in with_boot() {
        let pad_roles: Vec<&str> = card.pins.values().map(String::as_str).collect();
        let id = &card.id;
        for row in &card.boot_modes {
            assert!(
                !row.cite.trim().is_empty(),
                "{id}: mode '{}' has no citation",
                row.name
            );
            for role in row.when.keys() {
                assert!(
                    card.boot_inputs.iter().any(|i| &i.role == role),
                    "{id}: mode '{}' reads '{role}' but boot_inputs has no row for it",
                    row.name
                );
            }
        }
        for input in &card.boot_inputs {
            assert!(
                pad_roles.contains(&input.role.as_str()),
                "{id}: boot input '{}' is not a role in [models.pins]",
                input.role
            );
            assert!(
                !input.cite.trim().is_empty(),
                "{id}: '{}' has no citation",
                input.role
            );
        }
        if let Some(reset) = &card.reset_input {
            assert!(
                pad_roles.contains(&reset.role.as_str()),
                "{id}: reset role '{}' is not a pad",
                reset.role
            );
        }
        let latch = card.strap_latch.as_ref();
        assert!(
            latch.is_some_and(|l| !l.cite.trim().is_empty()),
            "{id}: no strap_latch with a citation"
        );
    }
}

#[test]
fn a_rom_bootloader_mode_says_what_answers() {
    for card in with_boot() {
        let runs_rom = card
            .boot_modes
            .iter()
            .any(|m| m.outcome == BootOutcome::RomBootloader);
        if !runs_rom || card.qemu_boot.is_some() {
            // An emulated ROM (QEMU) answers for itself.
            continue;
        }
        let rom = card.rom_bootloader.as_ref().unwrap_or_else(|| {
            panic!(
                "{}: a ROM-bootloader mode but no [models.rom_bootloader]",
                card.id
            )
        });
        assert!(
            !rom.cite.trim().is_empty(),
            "{}: rom_bootloader has no citation",
            card.id
        );
        if rom.protocol == RomProtocol::StUsart {
            assert!(rom.version.is_some(), "{}: no protocol version", card.id);
            assert!(rom.product_id.is_some(), "{}: no product id", card.id);
            for must in [0x00_u8, 0x01, 0x02] {
                assert!(
                    rom.commands.contains(&must),
                    "{}: command list lacks Get/GetVersion/GetID (0x{must:02x})",
                    card.id
                );
            }
        }
    }
}

#[test]
fn the_stm32_f1_table_is_the_vendors_table_9() {
    let f1 = cards().into_iter().find(|c| c.id == "stm32f103c8").unwrap();
    let rows: Vec<(Vec<(String, PinLevel)>, BootOutcome)> = f1
        .boot_modes
        .iter()
        .map(|m| (m.when.iter().map(|(k, v)| (k.clone(), *v)).collect(), m.outcome))
        .collect();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].1, BootOutcome::UserFlash);
    assert_eq!(rows[1].1, BootOutcome::RomBootloader);
    assert_eq!(rows[2].1, BootOutcome::Sram);
    assert_eq!(f1.rom_bootloader.as_ref().unwrap().product_id, Some(0x0410));
}
