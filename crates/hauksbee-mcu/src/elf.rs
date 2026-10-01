//! Firmware-architecture gate: read an ELF image's `e_machine` and refuse to
//! load it onto a backend whose MCU is a different ISA.
//!
//! # Why this exists
//!
//! Handing an image straight to the emulator with no architecture check fails
//! silently rather than loudly. Loading an Xtensa ESP32 image onto a
//! RISC-V ESP32-C3 board (`qemu-system-riscv32`) does not error: the RISC-V core
//! tries to execute Xtensa instructions, the SPI-flash second-stage bootloader
//! header parses as `0xffffffff`, and the run produces ~136 MB of UART garbage
//! with no diagnostic.
//!
//! An ELF carries its target ISA in the `e_machine` half-word at file offset
//! 0x12. This module reads it and compares it against the architecture the
//! selected backend/MCU actually executes, refusing the load *before* anything
//! is handed to the emulator (or QEMU is spawned) when they disagree.
//!
//! # Scope
//!
//! The check fires only for genuine ELF images (the `0x7F 'E' 'L' 'F'` magic).
//! Raw `.bin` images (e.g. an `esptool merge_bin` flash image) carry no header
//! and therefore no recoverable architecture, so the check is *skipped* for
//! them rather than guessed, see [`read_e_machine`] returning `None` and the
//! `bin`-handling notes in each backend.

use anyhow::{bail, Result};
use std::path::Path;

/// The four-byte ELF magic: `0x7F 'E' 'L' 'F'`.
const ELF_MAGIC: [u8; 4] = [0x7F, b'E', b'L', b'F'];

/// File offset of the little-/big-endian `e_machine` half-word in the ELF
/// header. Same offset for ELF32 and ELF64 (`e_ident[16]` + `e_type[2]`).
const E_MACHINE_OFFSET: usize = 0x12;

// ── e_machine constants (subset relevant to the supported backends) ──────────
// Values are the canonical `EM_*` numbers from the System V ABI / the ELF spec.

/// `EM_ARM`, 32-bit ARM (Cortex-M: STM32, nRF52 under Renode).
pub const EM_ARM: u16 = 0x28; // 40
/// `EM_AVR`, Atmel AVR 8-bit (atmega* under simavr).
pub const EM_AVR: u16 = 0x53; // 83
/// `EM_XTENSA`, Tensilica Xtensa (ESP32 / ESP32-S3 under qemu-system-xtensa).
pub const EM_XTENSA: u16 = 0x5E; // 94
/// `EM_RISCV`, RISC-V (ESP32-C3 under qemu-system-riscv32; `SiFive` FE310 under
/// Renode).
pub const EM_RISCV: u16 = 0xF3; // 243

/// Map a canonical `EM_*` name (as written in a `*.soc.toml` descriptor's
/// `expected_e_machine` field) to its `e_machine` number.
///
/// This is the reviewable-string side of the numeric [`EM_ARM`]/[`EM_RISCV`]/…
/// constants: an `SoC` descriptor says `expected_e_machine = "EM_ARM"` (the
/// example shape) rather than a raw `40`, and the loader resolves it here.
/// Returns `None` for an unrecognised name so the loader raises a named
/// "unknown `e_machine`" error instead of silently accepting garbage.
#[must_use]
pub fn e_machine_from_name(name: &str) -> Option<u16> {
    match name {
        "EM_ARM" => Some(EM_ARM),
        "EM_AVR" => Some(EM_AVR),
        "EM_XTENSA" => Some(EM_XTENSA),
        "EM_RISCV" => Some(EM_RISCV),
        _ => None,
    }
}

/// Human-readable name for an `e_machine` value, for error messages.
#[must_use]
pub const fn machine_name(e_machine: u16) -> &'static str {
    match e_machine {
        EM_ARM => "ARM",
        EM_AVR => "AVR",
        EM_XTENSA => "Xtensa",
        EM_RISCV => "RISC-V",
        _ => "unknown",
    }
}

/// Read the `e_machine` field of the ELF at `path`.
///
/// Returns:
///   - `Ok(Some(e_machine))` when the file is a valid ELF (correct magic and
///     long enough to contain the field),
///   - `Ok(None)` when the file is NOT an ELF (no magic), e.g. a raw `.bin`
///     flash image, which carries no architecture to check, so the caller
///     should skip the gate rather than error,
///   - `Err(..)` only on a real I/O failure reading the file.
///
/// The endianness of the half-word follows `e_ident[EI_DATA]` (offset 5):
/// `1` = little-endian, `2` = big-endian. All supported targets are
/// little-endian, but honouring the flag keeps the reader correct for any ELF.
///
/// # Errors
///
/// When the file cannot be opened or read.
pub fn read_e_machine(path: &Path) -> Result<Option<u16>> {
    let Some(header) = read_header(path)? else {
        return Ok(None);
    };
    let little_endian = header[5] != 2; // EI_DATA: 2 = big-endian, else little
    let lo = header[E_MACHINE_OFFSET];
    let hi = header[E_MACHINE_OFFSET + 1];
    let val = if little_endian {
        u16::from_le_bytes([lo, hi])
    } else {
        u16::from_be_bytes([lo, hi])
    };
    Ok(Some(val))
}

/// Read enough of `path` to cover the `e_machine` field and confirm the ELF
/// magic. Returns `None` (not an error) if the file is too short to be an ELF
/// or lacks the magic.
fn read_header(path: &Path) -> Result<Option<[u8; E_MACHINE_OFFSET + 2]>> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)
        .map_err(|e| anyhow::anyhow!("cannot open firmware '{}': {e}", path.display()))?;
    let mut buf = [0u8; E_MACHINE_OFFSET + 2];
    let mut read = 0usize;
    // Read until the buffer is full or EOF; a short file simply is not an ELF.
    loop {
        match f.read(&mut buf[read..]) {
            Ok(0) => break,
            Ok(n) => {
                read += n;
                if read == buf.len() {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => bail!("reading firmware '{}': {e}", path.display()),
        }
    }
    if read < buf.len() || buf[..4] != ELF_MAGIC {
        return Ok(None);
    }
    Ok(Some(buf))
}

/// Validate that the firmware ELF at `path` targets `expected`.
///
/// `expected` is the ISA the selected backend/MCU executes; a mismatch is
/// refused with a clear, two-sided error. `mcu_label` names the board/MCU in
/// the message (e.g. `"ESP32-C3"`).
///
/// Raw `.bin` images (no ELF header) cannot be checked, so this is a no-op for
/// them; it returns `Ok(())` without guessing.
///
/// # Errors
///
/// When the file cannot be read, or its `e_machine` is not `expected`.
pub fn validate_arch(path: &Path, expected: u16, mcu_label: &str) -> Result<()> {
    let Some(found) = read_e_machine(path)? else {
        // Not an ELF (raw .bin / .hex without a recoverable arch): skip.
        return Ok(());
    };
    if found == expected {
        return Ok(());
    }
    bail!(
        "firmware '{}' is {} (e_machine=0x{:X}) but board MCU {} needs {} (0x{:X}); \
         this is an architecture mismatch. Use the {} build of the firmware.",
        path.display(),
        machine_name(found),
        found,
        mcu_label,
        machine_name(expected),
        expected,
        machine_name(expected),
    );
}

/// Look up a symbol's value in a little-endian ELF32 image's `.symtab`.
///
/// Used for execution evidence: the backend hooks (or compares its program
/// counter against) the firmware's entry symbol to say whether `main` was
/// reached. Returns `None`, not an error, for anything that is not a
/// little-endian ELF32 with a symbol table carrying `name` (a stripped image,
/// a raw `.bin`, an ELF64): the evidence then reads "unknown".
#[must_use]
pub fn symbol_address(path: &Path, name: &str) -> Option<u32> {
    symbol_in_image(&std::fs::read(path).ok()?, name)
}

fn symbol_in_image(image: &[u8], name: &str) -> Option<u32> {
    let u16_at = |at: usize| Some(u16::from_le_bytes(image.get(at..at + 2)?.try_into().ok()?));
    let u32_at = |at: usize| Some(u32::from_le_bytes(image.get(at..at + 4)?.try_into().ok()?));
    let usize_at = |at: usize| u32_at(at).and_then(|v| usize::try_from(v).ok());
    // ELFCLASS32 + ELFDATA2LSB only.
    if image.get(..4)? != ELF_MAGIC || image.get(4) != Some(&1) || image.get(5) != Some(&1) {
        return None;
    }
    let shoff = usize_at(0x20)?;
    let shentsize = usize::from(u16_at(0x2E)?);
    let shnum = usize::from(u16_at(0x30)?);
    let section = |i: usize| shoff.checked_add(i.checked_mul(shentsize)?);
    for i in 0..shnum {
        let sh = section(i)?;
        if u32_at(sh + 4)? != 2 {
            continue; // SHT_SYMTAB only
        }
        let (sym_off, sym_size, link) =
            (usize_at(sh + 16)?, usize_at(sh + 20)?, usize_at(sh + 24)?);
        let str_off = usize_at(section(link)? + 16)?;
        for sym in (sym_off..sym_off.checked_add(sym_size)?).step_by(16) {
            let name_at = str_off.checked_add(usize_at(sym)?)?;
            let rest = image.get(name_at..)?;
            let end = rest.iter().position(|&b| b == 0)?;
            if &rest[..end] == name.as_bytes() {
                return u32_at(sym + 4);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn symbol_lookup_finds_main_and_refuses_non_elf() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/firmware");
        let arm = root.join("clock_truth/stm32g4_fault.elf");
        if arm.exists() {
            // Thumb function symbols carry the low bit.
            assert_eq!(symbol_address(&arm, "main"), Some(0x0800_007D));
            assert_eq!(symbol_address(&arm, "no_such_symbol"), None);
        }
        assert_eq!(symbol_in_image(b"not an elf at all", "main"), None);
        assert_eq!(symbol_in_image(&[], "main"), None);
    }

    /// Write a minimal fake ELF header with the given `e_machine` (little-endian)
    /// and return its temp path. The filename carries a per-process counter as
    /// well as the pid: several tests fabricate the SAME `e_machine` and the test
    /// harness runs them in parallel threads, so a pid+machine name alone let
    /// one test remove/rewrite a file another was mid-way through reading (a
    /// rare but real `reads_known_machines` flake).
    fn fake_elf(e_machine: u16) -> std::path::PathBuf {
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let mut buf = vec![0u8; E_MACHINE_OFFSET + 2];
        buf[..4].copy_from_slice(&ELF_MAGIC);
        buf[5] = 1; // EI_DATA = little-endian
        buf[E_MACHINE_OFFSET..].copy_from_slice(&e_machine.to_le_bytes());
        let path = std::env::temp_dir().join(format!(
            "hauksbee-fake-elf-{}-{}-{:04x}.elf",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            e_machine
        ));
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(&buf).unwrap();
        path
    }

    fn raw_bin(first: &[u8]) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "hauksbee-fake-bin-{}-{}.bin",
            std::process::id(),
            first.len()
        ));
        std::fs::write(&path, first).unwrap();
        path
    }

    #[test]
    fn reads_known_machines() {
        for em in [EM_ARM, EM_AVR, EM_XTENSA, EM_RISCV] {
            let p = fake_elf(em);
            assert_eq!(read_e_machine(&p).unwrap(), Some(em));
            std::fs::remove_file(&p).ok();
        }
    }

    #[test]
    fn raw_bin_is_not_an_elf() {
        // The real esp32 merged flash image starts with 0xffffffff.
        let p = raw_bin(&[0xff, 0xff, 0xff, 0xff, 0x00, 0x01]);
        assert_eq!(read_e_machine(&p).unwrap(), None);
        // And validation is a no-op (skip) rather than an error.
        assert!(validate_arch(&p, EM_RISCV, "ESP32-C3").is_ok());
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn too_short_is_not_an_elf() {
        let p = raw_bin(&[0x7f, b'E']); // has start of magic but too short
        assert_eq!(read_e_machine(&p).unwrap(), None);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn matched_arch_is_accepted() {
        let p = fake_elf(EM_RISCV);
        assert!(validate_arch(&p, EM_RISCV, "ESP32-C3").is_ok());
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn mismatched_arch_is_refused_with_clear_message() {
        // The persona bug: an Xtensa image aimed at a RISC-V ESP32-C3.
        let p = fake_elf(EM_XTENSA);
        let err = validate_arch(&p, EM_RISCV, "ESP32-C3").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("Xtensa"), "msg: {msg}");
        assert!(msg.contains("0x5E"), "msg: {msg}");
        assert!(msg.contains("ESP32-C3"), "msg: {msg}");
        assert!(msg.contains("RISC-V"), "msg: {msg}");
        assert!(msg.contains("0xF3"), "msg: {msg}");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn machine_names_are_correct() {
        assert_eq!(machine_name(EM_ARM), "ARM");
        assert_eq!(machine_name(EM_AVR), "AVR");
        assert_eq!(machine_name(EM_XTENSA), "Xtensa");
        assert_eq!(machine_name(EM_RISCV), "RISC-V");
        assert_eq!(machine_name(0x1234), "unknown");
    }
}
