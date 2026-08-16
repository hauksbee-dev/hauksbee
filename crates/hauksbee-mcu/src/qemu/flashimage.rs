//! Build a bootable merged ESP32 flash image from a bare application ELF.
//!
//! Long-form how-and-why: docs/how-and-why/hauksbee-mcu/qemu.md.
//!
//! # Why this exists
//!
//! Espressif QEMU boots from a merged SPI-flash image (2nd-stage bootloader +
//! partition table + app, at their per-chip offsets, padded to a power-of-two
//! size), never from an ELF: handing it an ELF as the mtd drive fails with
//! "only 2, 4, 8, 16 MB flash images are supported", or, if the size happens
//! to fit, boot-loops the ROM on an invalid image header. But the artifact a
//! user actually has in hand is the app ELF their build produced; the merged
//! image is an esptool/idf.py build-machine byproduct a cold install cannot
//! ask for (there is no esptool on the machine, and "go re-run your build with
//! merge_bin" is a terrible first-run answer).
//!
//! So the backend does the merge itself, in-process, with the espflash
//! library: the same ELF-to-app-image conversion, default per-chip bootloader
//! binaries and default partition table the esp-rs flashing tooling uses on
//! real hardware. The result is written to a temp file whose lifetime the
//! caller ties to the QEMU process.

use anyhow::{bail, Context, Result};
use espflash::flasher::{FlashData, FlashSettings, FlashSize};
use espflash::image_format::idf::IdfBootloaderFormat;
use espflash::target::{Chip, XtalFrequency};
use std::io::Write;
use std::path::Path;

/// Merged image size. QEMU's ESP32-family flash model accepts only 2/4/8/16 MB
/// images; 4 MB matches the repo's fixture images and every devkit default.
const FLASH_SIZE_BYTES: usize = 4 * 1024 * 1024;

/// The espflash target chip for a QEMU `-machine` name, `None` when the
/// machine is not one this converter knows how to lay an image out for.
pub(crate) fn chip_for_machine(machine: &str) -> Option<Chip> {
    match machine {
        "esp32" => Some(Chip::Esp32),
        "esp32s2" => Some(Chip::Esp32s2),
        "esp32s3" => Some(Chip::Esp32s3),
        "esp32c3" => Some(Chip::Esp32c3),
        _ => None,
    }
}

/// Flash offset where this machine's ROM expects the 2nd-stage bootloader
/// image (magic byte 0xE9). The original ESP32 and the S2 read it at 0x1000;
/// the S3 / C3 (and every later part) read it at 0x0. Used both to place the
/// bootloader when building an image and to sanity-check a user-supplied
/// merged image BEFORE booting it (a wrong-chip image otherwise boot-loops
/// the ROM forever with "invalid header: 0xffffffff" and no useful error).
pub(crate) fn bootloader_offset(machine: &str) -> u64 {
    match machine {
        "esp32" | "esp32s2" => 0x1000,
        _ => 0x0,
    }
}

/// Build a bootable, QEMU-sized merged flash image from the app ELF at
/// `elf_path`, returning the temp file holding it. The caller must keep the
/// returned handle alive as long as QEMU runs from it.
pub(crate) fn merged_image_from_elf(
    elf_path: &Path,
    machine: &str,
) -> Result<tempfile::NamedTempFile> {
    let Some(chip) = chip_for_machine(machine) else {
        bail!(
            "don't know the flash layout for QEMU machine '{machine}'; supply a \
             merged flash image (esptool merge_bin) instead of an ELF"
        );
    };
    let elf_data = std::fs::read(elf_path)
        .with_context(|| format!("reading firmware ELF {}", elf_path.display()))?;

    // Defaults throughout: espflash's per-chip default bootloader binary, its
    // default single-app partition table sized to the 4 MB flash, and the
    // 40 MHz crystal every supported devkit (and the QEMU machine model) uses.
    let flash_data = FlashData::new(
        FlashSettings::new(None, Some(FlashSize::_4Mb), None),
        0,
        None,
        chip,
        XtalFrequency::_40Mhz,
    );
    // espflash 4.5 has an internal `unreachable!` when an ELF advertises a
    // `.flash.appdesc` section that is not part of any flash segment. Real
    // build trees can contain exactly that intermediate ELF. A library panic
    // must not strand a browser launch request or tear down its Tokio worker;
    // translate it into the same actionable refusal as any other bad input.
    let image = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        IdfBootloaderFormat::new(&elf_data, &flash_data, None, None, None, None)
    })) {
        Ok(result) => result.with_context(|| {
            format!(
                "converting {} into an ESP32 app image (is it the app ELF your \
                 esp-idf/Arduino build produced, for this exact chip?)",
                elf_path.display()
            )
        })?,
        Err(_) => {
            bail!(
                "ESP32 app ELF '{}' has an app-descriptor/flash-segment layout \
                 the bundled converter cannot safely merge. Use the build's \
                 merged flash.bin (or produce one with esptool merge_bin) instead; \
                 no emulator was started.",
                elf_path.display()
            )
        }
    };

    // Lay the segments (bootloader, partition table, app) into a 0xFF-filled
    // flash-sized buffer: byte-identical to what
    // `esptool merge_bin --fill-flash-size 4MB` produces from the same parts.
    let mut flash = vec![0xFFu8; FLASH_SIZE_BYTES];
    for seg in image.flash_segments() {
        let start = seg.addr as usize;
        let end = start
            .checked_add(seg.data.len())
            .filter(|&e| e <= FLASH_SIZE_BYTES);
        let Some(end) = end else {
            bail!(
                "firmware image segment at {:#x} ({} bytes) does not fit the \
                 4 MB flash the emulator models",
                seg.addr,
                seg.data.len()
            );
        };
        flash[start..end].copy_from_slice(&seg.data);
    }

    let mut tmp = tempfile::Builder::new()
        .prefix("hauksbee-esp-flash-")
        .suffix(".bin")
        .tempfile()
        .context("creating temp file for the merged flash image")?;
    tmp.write_all(&flash)
        .and_then(|_| tmp.flush())
        .context("writing the merged flash image")?;
    Ok(tmp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn machine_to_chip_covers_supported_parts() {
        assert!(chip_for_machine("esp32").is_some());
        assert!(chip_for_machine("esp32s3").is_some());
        assert!(chip_for_machine("esp32c3").is_some());
        assert!(chip_for_machine("lx60").is_none());
    }

    #[test]
    fn unsupported_idf_intermediate_elf_refuses_instead_of_unwinding() {
        let elf = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/firmware/watchy_display_init/watchy_display_init.elf");
        if !elf.is_file() {
            eprintln!("SKIP: {} is not present", elf.display());
            return;
        }
        // This ESP32 image is deliberately offered to the S3 converter: the
        // current espflash release reaches its internal appdesc unreachable
        // instead of returning an ordinary error for this layout/chip pairing.
        let call = std::panic::catch_unwind(|| merged_image_from_elf(&elf, "esp32s3"));
        assert!(call.is_ok(), "bad user firmware must not escape as a panic");
        let message = call.unwrap().unwrap_err().to_string();
        assert!(message.contains("merged flash.bin"), "{message}");
        assert!(message.contains("no emulator was started"), "{message}");
    }

    #[test]
    fn bootloader_offsets_match_the_rom() {
        // Classic ESP32 ROM loads the 2nd stage from 0x1000; S3/C3 from 0x0.
        assert_eq!(bootloader_offset("esp32"), 0x1000);
        assert_eq!(bootloader_offset("esp32s3"), 0x0);
        assert_eq!(bootloader_offset("esp32c3"), 0x0);
    }

    /// End-to-end against the repo's real fixture ELF: the merged image is
    /// flash-sized and carries the app-image magic at the chip's bootloader
    /// offset, i.e. the ROM would actually boot it.
    #[test]
    fn merges_the_fixture_elf_into_a_bootable_image() {
        let elf = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/firmware/esp32_blinky/esp32_blinky.elf");
        if !elf.exists() {
            eprintln!("SKIP: fixture ELF not present");
            return;
        }
        let tmp = merged_image_from_elf(&elf, "esp32").expect("merge");
        let img = std::fs::read(tmp.path()).unwrap();
        assert_eq!(img.len(), FLASH_SIZE_BYTES);
        assert_eq!(
            img[0x1000], 0xE9,
            "bootloader magic at the esp32 ROM offset"
        );
        assert_eq!(img[0x10000], 0xE9, "app image magic at the app offset");
    }
}
