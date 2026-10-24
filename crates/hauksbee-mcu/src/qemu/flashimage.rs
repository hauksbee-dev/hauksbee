//! Build a bootable merged ESP32 flash image from a bare application ELF.
//!
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
//! `merge_bin`" is a terrible first-run answer).
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

/// Size of the flash file handed to QEMU. Its flash model accepts 2/4/8/16 MB
/// files, but an image whose file is only as large as its header declares
/// (4 or 8 MB) fails IDF flash init and boot-loops; the same image padded with
/// erased 0xFF to 16 MB boots (dev-set firmware sweep, 2026-10-01). The image
/// header keeps declaring 4 MB, which is what the partition table is sized for.
const FLASH_SIZE_BYTES: usize = 16 * 1024 * 1024;

/// Offsets of the esptool `merge_bin` layout: partition table and factory app.
const PARTITION_TABLE_OFFSET: usize = 0x8000;
const APP_OFFSET: usize = 0x1_0000;

/// The espflash target chip for a QEMU `-machine` name, `None` when the
/// machine is not one this converter knows how to lay an image out for.
pub fn chip_for_machine(machine: &str) -> Option<Chip> {
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
pub fn bootloader_offset(machine: &str) -> u64 {
    match machine {
        "esp32" | "esp32s2" => 0x1000,
        _ => 0x0,
    }
}

thread_local! {
    static QUIET_PANICS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Run `f`, catching a panic without letting the default hook print the
/// "thread panicked at espflash/..." banner. The refusal the caller builds is
/// the user-facing message; the library's internal `unreachable!` is noise.
/// The wrapping hook is installed once and consults a thread-local flag, so
/// panics on other threads (a Tokio worker serving a different request) are
/// still reported normally.
fn quietly<T>(f: impl FnOnce() -> T) -> std::thread::Result<T> {
    static INSTALL: std::sync::Once = std::sync::Once::new();
    INSTALL.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if !QUIET_PANICS.with(std::cell::Cell::get) {
                previous(info);
            }
        }));
    });
    QUIET_PANICS.with(|q| q.set(true));
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    QUIET_PANICS.with(|q| q.set(false));
    out
}

/// Build a bootable, QEMU-sized merged flash image from the app ELF at
/// `elf_path`, returning the temp file holding it. The caller must keep the
/// returned handle alive as long as QEMU runs from it.
pub fn merged_image_from_elf(elf_path: &Path, machine: &str) -> Result<tempfile::NamedTempFile> {
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
    let image = match quietly(|| {
        IdfBootloaderFormat::new(&elf_data, &flash_data, None, None, None, None)
    }) {
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
                 16 MB flash the emulator models",
                seg.addr,
                seg.data.len()
            );
        };
        flash[start..end].copy_from_slice(&seg.data);
    }

    write_temp(&flash)
}

fn write_temp(flash: &[u8]) -> Result<tempfile::NamedTempFile> {
    let mut tmp = tempfile::Builder::new()
        .prefix("hauksbee-esp-flash-")
        .suffix(".bin")
        .tempfile()
        .context("creating temp file for the merged flash image")?;
    tmp.write_all(flash)
        .and_then(|()| tmp.flush())
        .context("writing the merged flash image")?;
    Ok(tmp)
}

/// Make a raw `.bin` bootable: an app-only image (esp image magic at 0, no
/// partition table at 0x8000) is merged with the build's own bootloader and
/// partition table found beside it, in the esptool `merge_bin` layout; a
/// merged image smaller than the flash file QEMU needs is padded with 0xFF.
/// `None` means the file is already a full-size image and boots as is.
///
/// # Errors
///
/// When the file cannot be read, is larger than the emulated flash, or is an
/// app-only image with no bootloader / partition table beside it.
pub fn prepare_raw_image(image: &Path, machine: &str) -> Result<Option<tempfile::NamedTempFile>> {
    let bytes =
        std::fs::read(image).with_context(|| format!("reading flash image {}", image.display()))?;
    if bytes.len() > FLASH_SIZE_BYTES {
        bail!(
            "flash image {} is {} bytes, larger than the 16 MB flash the emulator models",
            image.display(),
            bytes.len()
        );
    }
    let has_table =
        bytes.get(PARTITION_TABLE_OFFSET..PARTITION_TABLE_OFFSET + 2) == Some(&[0xAA, 0x50]);
    // An app image's first segment opens with `esp_app_desc_t`, whose magic
    // word 0xABCD5432 sits at 0x20 (24-byte image header + 8-byte segment
    // header). A bootloader at offset 0 (S2/S3/C3) also starts with 0xE9, so
    // the descriptor, not the image magic, is what marks an app-only file.
    let app_desc = bytes.get(0x20..0x24) == Some(&0xABCD_5432u32.to_le_bytes());
    if !has_table && bytes.first() == Some(&0xE9) && app_desc {
        let dir = image.parent().unwrap_or_else(|| Path::new("."));
        let find = |names: &[&str]| names.iter().map(|n| dir.join(n)).find(|p| p.is_file());
        let boot = find(&["bootloader.bin", "bootloader/bootloader.bin"]);
        let table = find(&[
            "partitions.bin",
            "partition-table.bin",
            "partition_table/partition-table.bin",
        ]);
        let (Some(boot), Some(table)) = (boot, table) else {
            bail!(
                "{} is an ESP app image without a bootloader or partition table, and \
                 no bootloader.bin + partitions.bin (or partition-table.bin) sits beside \
                 it to merge with. Pass the app .elf instead (hauksbee builds the full \
                 image from it), or a merged image (esptool merge_bin).",
                image.display()
            );
        };
        let mut flash = vec![0xFFu8; FLASH_SIZE_BYTES];
        let boot_at = usize::try_from(bootloader_offset(machine)).unwrap_or(0);
        for (at, path) in [
            (boot_at, boot.as_path()),
            (PARTITION_TABLE_OFFSET, table.as_path()),
        ] {
            let part =
                std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
            let limit = if at == PARTITION_TABLE_OFFSET {
                APP_OFFSET
            } else {
                PARTITION_TABLE_OFFSET
            };
            if at + part.len() > limit {
                bail!(
                    "{} overruns the next region of the flash layout",
                    path.display()
                );
            }
            flash[at..at + part.len()].copy_from_slice(&part);
        }
        if APP_OFFSET + bytes.len() > FLASH_SIZE_BYTES {
            bail!("app image {} does not fit the 16 MB flash", image.display());
        }
        flash[APP_OFFSET..APP_OFFSET + bytes.len()].copy_from_slice(&bytes);
        return write_temp(&flash).map(Some);
    }
    if bytes.len() == FLASH_SIZE_BYTES {
        return Ok(None);
    }
    let mut flash = bytes;
    flash.resize(FLASH_SIZE_BYTES, 0xFF);
    write_temp(&flash).map(Some)
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
        hauksbee_testkit::skip_if!(!elf.exists(), "fixture ELF not present");
        let tmp = merged_image_from_elf(&elf, "esp32").expect("merge");
        let img = std::fs::read(tmp.path()).unwrap();
        assert_eq!(img.len(), FLASH_SIZE_BYTES);
        assert_eq!(
            img[0x1000], 0xE9,
            "bootloader magic at the esp32 ROM offset"
        );
        assert_eq!(img[0x10000], 0xE9, "app image magic at the app offset");
    }

    #[test]
    fn short_merged_images_are_padded_and_full_ones_pass_through() {
        let dir = tempfile::TempDir::new().unwrap();
        let merged = dir.path().join("merged.bin");
        let mut img = vec![0xFFu8; 0x2_0000];
        img[0x1000] = 0xE9;
        img[PARTITION_TABLE_OFFSET..PARTITION_TABLE_OFFSET + 2].copy_from_slice(&[0xAA, 0x50]);
        std::fs::write(&merged, &img).unwrap();
        let padded = prepare_raw_image(&merged, "esp32")
            .unwrap()
            .expect("padded copy");
        let out = std::fs::read(padded.path()).unwrap();
        assert_eq!(out.len(), FLASH_SIZE_BYTES);
        assert_eq!(&out[..img.len()], &img[..]);
        assert!(out[img.len()..].iter().all(|&b| b == 0xFF));
        // Images sized to the flash their header declares (4 and 8 MB, the
        // sweep's boot-loop case) are padded the same way.
        for mb in [4usize, 8] {
            let mut sized = img.clone();
            sized.resize(mb * 1024 * 1024, 0xFF);
            std::fs::write(&merged, &sized).unwrap();
            let padded = prepare_raw_image(&merged, "esp32")
                .unwrap()
                .expect("padded");
            let out = std::fs::read(padded.path()).unwrap();
            assert_eq!(out.len(), FLASH_SIZE_BYTES, "{mb} MB");
            assert_eq!(&out[..sized.len()], &sized[..], "{mb} MB");
        }
        // Legal control: already 16 MB, used in place.
        std::fs::write(&merged, &out).unwrap();
        assert!(prepare_raw_image(&merged, "esp32").unwrap().is_none());
        // Larger than the flash: refused.
        let big = dir.path().join("big.bin");
        std::fs::write(&big, vec![0u8; FLASH_SIZE_BYTES + 1]).unwrap();
        assert!(prepare_raw_image(&big, "esp32").is_err());
    }

    #[test]
    fn app_only_bin_merges_with_its_build_siblings_or_refuses() {
        let dir = tempfile::TempDir::new().unwrap();
        let app = dir.path().join("firmware.bin");
        let mut app_bytes = vec![0xE9, 0x03, 0x02, 0x20];
        app_bytes.resize(0x20, 0);
        app_bytes.extend_from_slice(&0xABCD_5432u32.to_le_bytes());
        std::fs::write(&app, &app_bytes).unwrap();
        // Control: the same bytes without the app descriptor are a merged
        // image whose bootloader is at 0 (S3), padded rather than merged.
        let boot_only = dir.path().join("s3-merged.bin");
        std::fs::write(&boot_only, &app_bytes[..0x20]).unwrap();
        assert!(prepare_raw_image(&boot_only, "esp32s3").unwrap().is_some());
        let err = prepare_raw_image(&app, "esp32s3").unwrap_err().to_string();
        assert!(err.contains("bootloader.bin"), "{err}");
        std::fs::write(dir.path().join("bootloader.bin"), [0xE9, 0x01]).unwrap();
        std::fs::write(dir.path().join("partitions.bin"), [0xAA, 0x50, 0x01]).unwrap();
        for (machine, boot_at) in [("esp32s3", 0x0), ("esp32", 0x1000)] {
            let merged = prepare_raw_image(&app, machine).unwrap().expect("merged");
            let img = std::fs::read(merged.path()).unwrap();
            assert_eq!(img.len(), FLASH_SIZE_BYTES);
            assert_eq!(&img[boot_at..boot_at + 2], &[0xE9, 0x01]);
            assert_eq!(
                &img[PARTITION_TABLE_OFFSET..PARTITION_TABLE_OFFSET + 3],
                &[0xAA, 0x50, 0x01]
            );
            assert_eq!(
                &img[APP_OFFSET..APP_OFFSET + app_bytes.len()],
                &app_bytes[..]
            );
        }
    }
}
