//! AVR fuse bytes and the clock they select.
//!
//! avr-libc's `FUSES = { .low = ..., .high = ..., .extended = ... };` places the
//! fuse bytes in the ELF's `.fuse` section (low byte first), and that is what a
//! programmer burns. The low fuse picks the core clock: `CKSEL[3:0]` chooses the
//! source (crystal on XTAL1/XTAL2, internal RC, external clock) and `CKDIV8`
//! (bit 7, active when programmed = 0) divides it by eight.
//!
//! Two facts follow, and both are board-checkable:
//!
//! * a crystal source on a board with no crystal across the XTAL pins never
//!   starts (the part sits in reset-oscillator start-up forever);
//! * an internal-RC source on a board that fits a crystal ignores it, so every
//!   firmware timing derived from the crystal's `F_CPU` is wrong by the ratio.
//!
//! And the co-sim must run the core at the FUSED frequency, not a fixed one.
//!
//! Only the megaAVR families whose `CKSEL` map is the one decoded below are
//! recognised (48/88/168/328, 164/324/644/1284, 640/1280/1281/2560/2561).
//! Others (ATmega8/16/32 have no `CKDIV8` and a different RC map; `ATtiny` maps
//! vary per part) decode to `None`: no check, no frequency change.
//!
//! Pure std; no simavr link, so the check runs in every build shape.

use std::io::Read;
use std::path::Path;

/// The fuse bytes as stored in the ELF `.fuse` section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AvrFuses {
    pub low: u8,
    pub high: Option<u8>,
    pub extended: Option<u8>,
}

/// The clock source a low fuse's `CKSEL[3:0]` selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AvrClockSource {
    /// External clock driven into XTAL1 (`CKSEL = 0000`).
    ExternalClock,
    /// Calibrated internal RC oscillator at `hz` (8 MHz or 128 kHz).
    InternalRc { hz: u64 },
    /// A crystal/resonator across XTAL1/XTAL2 (low-frequency, full-swing or
    /// low-power oscillator).
    Crystal,
    /// A `CKSEL` code the datasheet marks reserved.
    Reserved,
}

/// The decoded clock selection: source plus the `CKDIV8` prescaler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AvrClockSelect {
    pub source: AvrClockSource,
    pub ckdiv8: bool,
}

/// True for the megaAVR cores whose `CKSEL`/`CKDIV8` layout [`decode_clock`]
/// implements. `part` is the simavr core name (`atmega328p`, ...).
#[must_use]
pub fn part_has_known_clock_fuses(part: &str) -> bool {
    let p = part.to_ascii_lowercase();
    let Some(rest) = p.strip_prefix("atmega") else {
        return false;
    };
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    matches!(
        digits.as_str(),
        "48" | "88"
            | "168"
            | "328"
            | "164"
            | "324"
            | "644"
            | "1284"
            | "640"
            | "1280"
            | "1281"
            | "2560"
            | "2561"
    )
}

/// Decode the low fuse for `part`. `None` when the part's fuse layout is not
/// one this module knows.
#[must_use]
pub fn decode_clock(part: &str, low: u8) -> Option<AvrClockSelect> {
    if !part_has_known_clock_fuses(part) {
        return None;
    }
    let source = match low & 0x0F {
        0b0000 => AvrClockSource::ExternalClock,
        0b0010 => AvrClockSource::InternalRc { hz: 8_000_000 },
        0b0011 => AvrClockSource::InternalRc { hz: 128_000 },
        0b0001 => AvrClockSource::Reserved,
        _ => AvrClockSource::Crystal,
    };
    Some(AvrClockSelect {
        source,
        // CKDIV8 is bit 7, PROGRAMMED (active) when 0.
        ckdiv8: low & 0x80 == 0,
    })
}

impl AvrClockSelect {
    /// The core clock the fuses produce, given the board's crystal frequency
    /// (when one is fitted and its value parses). `None` when the source's
    /// frequency is not knowable from the board (no crystal for a crystal
    /// source, an external clock, a reserved code).
    #[must_use]
    pub fn core_hz(&self, board_crystal_hz: Option<u64>) -> Option<u64> {
        let base = match self.source {
            AvrClockSource::InternalRc { hz } => hz,
            AvrClockSource::Crystal => board_crystal_hz?,
            AvrClockSource::ExternalClock | AvrClockSource::Reserved => return None,
        };
        Some(if self.ckdiv8 { base / 8 } else { base })
    }

    /// The fuse/board inconsistency, if any, as a sentence. `crystal_fitted` is
    /// whether an assembled crystal bridges the part's XTAL1/XTAL2 nets.
    #[must_use]
    pub fn board_mismatch(&self, crystal_fitted: bool) -> Option<String> {
        match (self.source, crystal_fitted) {
            (AvrClockSource::Crystal, false) => Some(
                "the fuses select a crystal oscillator on XTAL1/XTAL2, but no crystal \
                 is fitted across those pins: the oscillator never starts and the \
                 part does not run"
                    .to_string(),
            ),
            (AvrClockSource::InternalRc { hz }, true) => Some(format!(
                "a crystal is fitted across XTAL1/XTAL2, but the fuses select the \
                 internal {} RC oscillator: the crystal is unused, and firmware timed \
                 for the crystal's F_CPU runs at the RC frequency instead",
                fmt_hz(hz)
            )),
            (AvrClockSource::Reserved, _) => Some(
                "the fuses select a CKSEL code the datasheet marks reserved; the \
                 clock source is undefined"
                    .to_string(),
            ),
            _ => None,
        }
    }
}

/// A frequency as `8 MHz` / `128 kHz` / `1234 Hz`, for the sentences that
/// report a clock.
#[must_use]
pub fn fmt_hz(hz: u64) -> String {
    if hz >= 1_000_000 && hz.is_multiple_of(1_000_000) {
        format!("{} MHz", hz / 1_000_000)
    } else if hz.is_multiple_of(1_000) {
        format!("{} kHz", hz / 1_000)
    } else {
        format!("{hz} Hz")
    }
}

/// Read the `.fuse` section of an AVR ELF. `Ok(None)` when the file is not a
/// 32-bit little-endian ELF or carries no `.fuse` section (a `.hex`/`.bin`
/// image, or firmware built without `FUSES`).
///
/// # Errors
/// Only on an I/O error reading the file.
pub fn read_elf_fuses(path: &Path) -> std::io::Result<Option<AvrFuses>> {
    let mut buf = Vec::new();
    std::fs::File::open(path)?.read_to_end(&mut buf)?;
    Ok(parse_elf_fuses(&buf))
}

fn u16_at(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(off..off + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?))
}

/// Parse the `.fuse` section out of an in-memory ELF32-LE image.
#[must_use]
pub fn parse_elf_fuses(b: &[u8]) -> Option<AvrFuses> {
    // ELF32 (class 1), little-endian (data 1): every AVR ELF.
    if b.get(0..4)? != b"\x7fELF" || *b.get(4)? != 1 || *b.get(5)? != 1 {
        return None;
    }
    let shoff = u32_at(b, 0x20)? as usize;
    let shentsize = u16_at(b, 0x2E)? as usize;
    let shnum = u16_at(b, 0x30)? as usize;
    let shstrndx = u16_at(b, 0x32)? as usize;
    if shentsize < 40 || shstrndx >= shnum {
        return None;
    }
    let sh = |i: usize| shoff.checked_add(i.checked_mul(shentsize)?);
    let strtab_hdr = sh(shstrndx)?;
    let strtab_off = u32_at(b, strtab_hdr + 0x10)? as usize;
    for i in 0..shnum {
        let h = sh(i)?;
        let name_off = u32_at(b, h)? as usize;
        let name_start = strtab_off.checked_add(name_off)?;
        let name = b.get(name_start..)?;
        let end = name.iter().position(|&c| c == 0)?;
        if &name[..end] != b".fuse" {
            continue;
        }
        let off = u32_at(b, h + 0x10)? as usize;
        let size = u32_at(b, h + 0x14)? as usize;
        let data = b.get(off..off.checked_add(size)?)?;
        let low = *data.first()?;
        return Some(AvrFuses {
            low,
            high: data.get(1).copied(),
            extended: data.get(2).copied(),
        });
    }
    None
}

/// A minimal ELF32-LE (EM_AVR) whose only content is a `.fuse` section
/// holding `fuses`: a fixture for tests of fuse-reading callers, which
/// otherwise need an AVR toolchain to produce one.
#[doc(hidden)]
#[must_use]
pub fn fuse_only_elf(fuses: &[u8]) -> Vec<u8> {
    let shstr = b"\0.fuse\0.shstrtab\0";
    let mut b = vec![0u8; 52];
    b[0..4].copy_from_slice(b"\x7fELF");
    b[4] = 1;
    b[5] = 1;
    b[6] = 1;
    b[0x12..0x14].copy_from_slice(&0x53u16.to_le_bytes()); // EM_AVR
    let fuse_off = b.len();
    b.extend_from_slice(fuses);
    let str_off = b.len();
    b.extend_from_slice(shstr);
    let shoff = b.len();
    b[0x20..0x24].copy_from_slice(&u32::try_from(shoff).expect("ELF32 offset").to_le_bytes());
    b[0x2E..0x30].copy_from_slice(&40u16.to_le_bytes());
    b[0x30..0x32].copy_from_slice(&3u16.to_le_bytes());
    b[0x32..0x34].copy_from_slice(&2u16.to_le_bytes());
    let mut hdr = |name: u32, off: usize, size: usize| {
        let mut h = [0u8; 40];
        h[0..4].copy_from_slice(&name.to_le_bytes());
        h[0x10..0x14].copy_from_slice(&u32::try_from(off).expect("ELF32 offset").to_le_bytes());
        h[0x14..0x18].copy_from_slice(&u32::try_from(size).expect("ELF32 size").to_le_bytes());
        b.extend_from_slice(&h);
    };
    hdr(0, 0, 0);
    hdr(1, fuse_off, fuses.len());
    hdr(7, str_off, shstr.len());
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_fuse_section_low_byte_first() {
        let elf = fuse_only_elf(&[0xFF, 0xDE, 0xFD]);
        assert_eq!(
            parse_elf_fuses(&elf),
            Some(AvrFuses {
                low: 0xFF,
                high: Some(0xDE),
                extended: Some(0xFD)
            })
        );
    }

    #[test]
    fn an_elf_without_a_fuse_section_has_no_fuses() {
        let mut elf = fuse_only_elf(&[0x62]);
        // Rename `.fuse` to `.fusf`.
        let pos = elf.windows(5).position(|w| w == b".fuse").unwrap();
        elf[pos + 4] = b'f';
        assert_eq!(parse_elf_fuses(&elf), None);
        assert_eq!(parse_elf_fuses(b":100000000C9434000C94"), None);
    }

    #[test]
    fn decodes_the_megaavr_clock_select() {
        // Arduino Uno: 0xFF = low-power crystal, CKDIV8 unprogrammed.
        let uno = decode_clock("atmega328p", 0xFF).unwrap();
        assert_eq!(uno.source, AvrClockSource::Crystal);
        assert!(!uno.ckdiv8);
        assert_eq!(uno.core_hz(Some(16_000_000)), Some(16_000_000));
        // Factory default: 0x62 = internal 8 MHz RC with CKDIV8 = 1 MHz.
        let factory = decode_clock("atmega328p", 0x62).unwrap();
        assert_eq!(factory.source, AvrClockSource::InternalRc { hz: 8_000_000 });
        assert!(factory.ckdiv8);
        assert_eq!(factory.core_hz(Some(16_000_000)), Some(1_000_000));
        // Crystal + CKDIV8.
        assert_eq!(
            decode_clock("atmega2560", 0x7F)
                .unwrap()
                .core_hz(Some(16_000_000)),
            Some(2_000_000)
        );
        // A layout this module does not know decodes to nothing.
        assert_eq!(decode_clock("atmega8", 0xFF), None);
        assert_eq!(decode_clock("attiny85", 0x62), None);
    }

    #[test]
    fn board_mismatch_is_reported_both_ways_and_only_then() {
        let xtal = decode_clock("atmega328p", 0xFF).unwrap();
        let rc = decode_clock("atmega328p", 0xE2).unwrap();
        assert!(xtal.board_mismatch(false).unwrap().contains("no crystal"));
        assert!(rc
            .board_mismatch(true)
            .unwrap()
            .contains("crystal is unused"));
        // The legal controls: consistent fuses and board.
        assert_eq!(xtal.board_mismatch(true), None);
        assert_eq!(rc.board_mismatch(false), None);
        let ext = decode_clock("atmega328p", 0xE0).unwrap();
        assert_eq!(ext.board_mismatch(false), None);
    }
}
