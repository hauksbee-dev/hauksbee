//! Synthetic AVR firmware images for co-sim tests, written without a toolchain.
//!
//! The suites need firmware of a chosen SIZE (bigger than a small part's flash,
//! smaller than a big one's) and a chosen GPIO activity, and the committed
//! `testdata/firmware` images are fixed on both counts. These helpers
//! hand-assemble a few megaAVR instructions, pad the image with `NOP`s to any
//! length, and wrap it as Intel HEX or as a minimal AVR ELF, so a test states
//! exactly the image it loads.

use std::fmt::Write as _;

/// The program [`port_toggle_image`] assembles, in words. Same I/O addresses on
/// every ATmega48/88/168/328 (`DDRB` 0x04, `PORTB` 0x05, `DDRD` 0x0A, `PORTD`
/// 0x0B), so one image runs on all of them.
///
/// ```text
///         ldi  r16, 0xFF
///         out  DDRB, r16        ; port B all outputs
///         out  DDRD, r16        ; port D all outputs
/// loop:   in   r17, PORTB
///         eor  r17, r16
///         out  PORTB, r17       ; toggle every port B pin
///         in   r17, PORTD
///         eor  r17, r16
///         out  PORTD, r17       ; toggle every port D pin
///         ldi  r18, DELAY
/// spin:   dec  r18
///         brne spin             ; 3 cycles per pass
///         rjmp loop
/// ```
fn toggle_words(delay: u8) -> [u16; 13] {
    let ldi_r18 = 0xE020 | (u16::from(delay & 0xF0) << 4) | u16::from(delay & 0x0F);
    [
        0xEF0F,  // ldi r16, 0xFF
        0xB904,  // out 0x04, r16
        0xB90A,  // out 0x0A, r16
        0xB115,  // in r17, 0x05
        0x2710,  // eor r17, r16
        0xB915,  // out 0x05, r17
        0xB11B,  // in r17, 0x0B
        0x2710,  // eor r17, r16
        0xB91B,  // out 0x0B, r17
        ldi_r18, // ldi r18, delay
        0x952A,  // dec r18
        0xF7F1,  // brne .-4 (back to dec)
        0xCFF6,  // rjmp .-20 (back to loop)
    ]
}

/// Flash cycles one pass of the [`port_toggle_image`] loop takes for `delay`.
///
/// That is the six toggle instructions, the `ldi`, `delay` passes of `dec`+`brne`
/// (the last `brne` falls through in one cycle), and the `rjmp`.
#[must_use]
pub fn port_toggle_period_cycles(delay: u8) -> u64 {
    let d = if delay == 0 { 256 } else { u64::from(delay) };
    6 + 1 + 3 * d - 1 + 2
}

/// A megaAVR image that toggles every `PORTB` and `PORTD` pin.
///
/// One toggle per [`port_toggle_period_cycles`] (`delay` sets the rate),
/// padded with `NOP` words to `pad_to` bytes (never shorter than the program
/// itself).
#[must_use]
pub fn port_toggle_image(delay: u8, pad_to: usize) -> Vec<u8> {
    let mut image: Vec<u8> = toggle_words(delay)
        .iter()
        .flat_map(|w| w.to_le_bytes())
        .collect();
    if image.len() < pad_to {
        image.resize(pad_to, 0x00);
    }
    image
}

/// `ldi Rd, K` for `d` in `16..=31`.
fn ldi(d: u8, k: u8) -> u16 {
    0xE000 | (u16::from(k & 0xF0) << 4) | (u16::from(d - 16) << 4) | u16::from(k & 0x0F)
}

/// `out A, Rr` for an I/O address `a` in `0..=0x3F`.
fn out(a: u8, r: u8) -> u16 {
    0xB800 | (u16::from(a & 0x30) << 5) | (u16::from(r) << 4) | u16::from(a & 0x0F)
}

/// A megaAVR image that reads `PORTD` pins with their pull-ups on, then
/// sleeps in power-down.
///
/// Same I/O addresses on every ATmega48/88/168/328 (`DDRD` 0x0A, `PORTD` 0x0B,
/// `SMCR` 0x33). `pull_up_mask` is the set of `PORTD` pins made inputs with the
/// pull-up on. With `clear_before_sleep` the program then writes `PORTD = 0`,
/// which is the fix for the leak; without it the pull-ups stay on through the
/// sleep, which is the fault. `sei` precedes `sleep` because simavr turns a
/// sleep with interrupts disabled into a finished core.
///
/// ```text
///         ldi  r16, 0x00
///         out  DDRD, r16        ; port D all inputs
///         ldi  r17, MASK
///         out  PORTD, r17       ; pull-ups on
///         ldi  r17, 0x00        ; (only with clear_before_sleep)
///         out  PORTD, r17       ; pull-ups off
///         ldi  r18, 0x05        ; SM2:0 = 010 power-down, SE = 1
///         out  SMCR, r18
///         sei
///         sleep
/// halt:   rjmp halt
/// ```
#[must_use]
pub fn pull_up_sleep_image(pull_up_mask: u8, clear_before_sleep: bool) -> Vec<u8> {
    const DDRD: u8 = 0x0A;
    const PORTD: u8 = 0x0B;
    const SMCR: u8 = 0x33;
    let mut words = vec![
        ldi(16, 0x00),
        out(DDRD, 16),
        ldi(17, pull_up_mask),
        out(PORTD, 17),
    ];
    if clear_before_sleep {
        words.extend([ldi(17, 0x00), out(PORTD, 17)]);
    }
    words.extend([
        ldi(18, 0x05), // SM2:0 = 010 (power-down), SE = 1
        out(SMCR, 18),
        0x9478, // sei
        0x9588, // sleep
        0xCFFF, // rjmp .-2 (to itself)
    ]);
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}

/// `image` as Intel HEX loaded from address 0.
///
/// 16-byte data records, an extended-linear-address record at each 64 KiB
/// boundary, then end-of-file.
///
/// # Panics
///
/// If `image` is 4 GiB or more, which Intel HEX cannot address.
#[must_use]
pub fn intel_hex(image: &[u8]) -> String {
    fn record(out: &mut String, kind: u8, addr: u16, data: &[u8]) {
        let len = u8::try_from(data.len()).expect("a hex record holds at most 255 bytes");
        let [hi, lo] = addr.to_be_bytes();
        let mut sum = len.wrapping_add(hi).wrapping_add(lo).wrapping_add(kind);
        let _ = write!(out, ":{len:02X}{addr:04X}{kind:02X}");
        for b in data {
            sum = sum.wrapping_add(*b);
            let _ = write!(out, "{b:02X}");
        }
        let _ = writeln!(out, "{:02X}", sum.wrapping_neg());
    }
    let mut out = String::new();
    for (i, chunk) in image.chunks(16).enumerate() {
        let at = i * 16;
        if at % 0x1_0000 == 0 && at > 0 {
            let upper = u16::try_from(at >> 16).expect("image below 4 GiB");
            record(&mut out, 0x04, 0, &upper.to_be_bytes());
        }
        let low = u16::try_from(at & 0xFFFF).expect("masked to 16 bits");
        record(&mut out, 0x00, low, chunk);
    }
    record(&mut out, 0x01, 0, &[]);
    out
}

/// `image` as a minimal little-endian ELF32 for `EM_AVR` (83).
///
/// One `PT_LOAD` program header placing the whole image at flash address 0,
/// no sections: what an AVR linker emits for the flash segment, without the
/// symbol table.
///
/// # Panics
///
/// If `image` is 4 GiB or more, which ELF32 cannot address.
#[must_use]
pub fn avr_elf(image: &[u8]) -> Vec<u8> {
    const EHSIZE: u32 = 52;
    const PHENTSIZE: u32 = 32;
    let size = u32::try_from(image.len()).expect("image below 4 GiB");
    let data_off = EHSIZE + PHENTSIZE;
    let mut elf = Vec::with_capacity(data_off as usize + image.len());
    // e_ident: magic, ELFCLASS32, ELFDATA2LSB, EV_CURRENT, padding.
    elf.extend_from_slice(&[0x7F, b'E', b'L', b'F', 1, 1, 1, 0]);
    elf.extend_from_slice(&[0; 8]);
    let half = |v: u16| v.to_le_bytes();
    let word = |v: u32| v.to_le_bytes();
    elf.extend_from_slice(&half(2)); // e_type ET_EXEC
    elf.extend_from_slice(&half(83)); // e_machine EM_AVR
    elf.extend_from_slice(&word(1)); // e_version
    elf.extend_from_slice(&word(0)); // e_entry
    elf.extend_from_slice(&word(EHSIZE)); // e_phoff
    elf.extend_from_slice(&word(0)); // e_shoff
    elf.extend_from_slice(&word(0)); // e_flags
    elf.extend_from_slice(&half(52)); // e_ehsize
    elf.extend_from_slice(&half(32)); // e_phentsize
    elf.extend_from_slice(&half(1)); // e_phnum
    elf.extend_from_slice(&half(40)); // e_shentsize
    elf.extend_from_slice(&half(0)); // e_shnum
    elf.extend_from_slice(&half(0)); // e_shstrndx
                                     // Program header: PT_LOAD, offset, vaddr, paddr, filesz, memsz, R|X, align.
    for v in [1, data_off, 0, 0, size, size, 5, 2] {
        elf.extend_from_slice(&word(v));
    }
    elf.extend_from_slice(image);
    elf
}

#[cfg(test)]
mod tests {
    use super::{
        avr_elf, intel_hex, ldi, out, port_toggle_image, port_toggle_period_cycles,
        pull_up_sleep_image,
    };

    #[test]
    fn the_assembler_helpers_match_the_toggle_program() {
        // The same encodings `toggle_words` spells out by hand.
        assert_eq!(ldi(16, 0xFF), 0xEF0F);
        assert_eq!(out(0x04, 16), 0xB904);
        assert_eq!(out(0x0B, 17), 0xB91B);
        // SMCR sits at I/O 0x33: the high address bits land in the opcode.
        assert_eq!(out(0x33, 18), 0xBF23);
        assert_eq!(ldi(17, 0x0C), 0xE01C);
        assert_eq!(ldi(18, 0x05), 0xE025);
    }

    #[test]
    fn the_sleep_image_clears_the_pull_ups_only_when_asked() {
        let words = |image: &[u8]| -> Vec<u16> {
            image
                .chunks(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect()
        };
        let fault = words(&pull_up_sleep_image(0x0C, false));
        let fixed = words(&pull_up_sleep_image(0x0C, true));
        // ldi r17,0 / out PORTD,r17 sit between the pull-up write and SMCR.
        assert_eq!(fixed.len(), fault.len() + 2);
        assert_eq!(&fixed[4..6], &[0xE010, 0xB91B]);
        assert!(!fault.windows(2).any(|w| w == [0xE010, 0xB91B]));
        for p in [&fault, &fixed] {
            // sei; sleep; rjmp . at the end.
            assert_eq!(&p[p.len() - 3..], &[0x9478, 0x9588, 0xCFFF]);
            assert!(p.contains(&0xBF23), "SMCR is written");
        }
    }

    #[test]
    fn hex_records_checksum_and_cover_the_image() {
        let image = port_toggle_image(10, 40);
        let hex = intel_hex(&image);
        let lines: Vec<&str> = hex.lines().collect();
        assert_eq!(lines.len(), 4, "three data records and EOF: {hex}");
        assert_eq!(lines[3], ":00000001FF");
        for line in &lines {
            let bytes: Vec<u8> = (1..line.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&line[i..i + 2], 16).unwrap())
                .collect();
            assert_eq!(bytes.iter().fold(0u8, |a, b| a.wrapping_add(*b)), 0);
        }
        // ldi r16, 0xFF little-endian first.
        assert!(lines[0].starts_with(":100000000FEF"));
    }

    #[test]
    fn elf_places_the_image_after_one_program_header() {
        let image = port_toggle_image(1, 64);
        let elf = avr_elf(&image);
        assert_eq!(&elf[..4], b"\x7FELF");
        assert_eq!(u16::from_le_bytes([elf[18], elf[19]]), 83);
        assert_eq!(&elf[84..], &image[..]);
    }

    #[test]
    fn the_loop_period_counts_the_delay() {
        assert_eq!(port_toggle_period_cycles(1), 11);
        assert_eq!(port_toggle_period_cycles(255), 773);
    }
}
