//! AVR firmware images, read in Rust before any native loader sees them.
//!
//! simavr's loaders trust the image: its ELF loader clips a segment that is too
//! big for the part's flash but then zero-fills the clipped remainder PAST the
//! buffer (`do_chunk` in `sim_elf.c`), and its Intel HEX reader dereferences a
//! null chunk on a file with no data records. Either one takes the whole
//! process down (a heap-corruption abort in a later `free`/`realloc`, or a
//! segfault). A board whose alternate footprint binds a smaller-flash variant
//! of the MCU reaches exactly that: firmware sized for the bigger part loaded
//! into the smaller one.
//!
//! So every image is parsed here first, every segment is placed in its memory
//! region (flash, data, EEPROM, fuses, lock bits, by the AVR toolchain's
//! address map), and an image that does not fit the part is refused with both
//! sizes named, before a byte of it reaches the emulator.

use anyhow::{bail, Context, Result};
use std::path::Path;

/// The AVR toolchain's address map for the memories an image can carry
/// (`AVR_SEGMENT_OFFSET_*` in simavr's `sim_elf.h`, the same offsets avr-gcc's
/// linker scripts use).
const DATA_BASE: u32 = 0x0080_0000;
const EEPROM_BASE: u32 = 0x0081_0000;
const FUSES_BASE: u32 = 0x0082_0000;
const LOCK_BASE: u32 = 0x0083_0000;
const REGIONS_END: u32 = 0x0084_0000;

/// One of the memories an AVR image loads into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Region {
    /// Program memory.
    Flash,
    /// SRAM, above the I/O registers.
    Data,
    /// Data EEPROM.
    Eeprom,
    /// Fuse bytes.
    Fuses,
    /// Lock bits.
    Lock,
}

impl Region {
    /// The region an image address falls in, and the address relative to the
    /// region's start. `None` past the last region (an ELF `.mmcu` section,
    /// which simavr reads as metadata and never loads).
    #[must_use]
    pub const fn of(address: u32) -> Option<(Self, u32)> {
        if address < DATA_BASE {
            Some((Self::Flash, address))
        } else if address < EEPROM_BASE {
            Some((Self::Data, address - DATA_BASE))
        } else if address < FUSES_BASE {
            Some((Self::Eeprom, address - EEPROM_BASE))
        } else if address < LOCK_BASE {
            Some((Self::Fuses, address - FUSES_BASE))
        } else if address < REGIONS_END {
            Some((Self::Lock, address - LOCK_BASE))
        } else {
            None
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Flash => "flash",
            Self::Data => "SRAM",
            Self::Eeprom => "EEPROM",
            Self::Fuses => "fuse",
            Self::Lock => "lock-bit",
        }
    }
}

/// One loadable piece of an image: where it lands and how many bytes it
/// occupies there (for an ELF segment, the larger of its file and memory sizes,
/// since the loader zero-fills up to the memory size).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    /// The memory it loads into.
    pub region: Region,
    /// Start address within that memory.
    pub start: u32,
    /// Bytes it occupies from `start`.
    pub len: u64,
}

impl Segment {
    /// One past the last byte this segment occupies.
    #[must_use]
    pub fn end(&self) -> u64 {
        u64::from(self.start) + self.len
    }
}

/// The bytes of each memory a part has, as its loader allocates them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capacity {
    /// Program memory bytes.
    pub flash: u64,
    /// SRAM bytes above the I/O registers.
    pub data: u64,
    /// EEPROM bytes.
    pub eeprom: u64,
    /// Fuse bytes the core keeps.
    pub fuses: u64,
    /// Lock-bit bytes the core keeps.
    pub lock: u64,
}

impl Capacity {
    const fn of(&self, region: Region) -> u64 {
        match region {
            Region::Flash => self.flash,
            Region::Data => self.data,
            Region::Eeprom => self.eeprom,
            Region::Fuses => self.fuses,
            Region::Lock => self.lock,
        }
    }
}

/// An AVR firmware image's layout, read from a `.hex` or `.elf` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvrImage {
    /// Every loadable piece, in file order.
    pub segments: Vec<Segment>,
    /// For an Intel HEX file, the contiguous data runs (absolute address, bytes)
    /// in file order, which is what the HEX loader copies into flash. Empty for
    /// an ELF, whose bytes simavr's own loader reads once the layout checks out.
    pub hex_runs: Vec<(u32, Vec<u8>)>,
    /// The first [`FLASH_HEAD`] bytes of flash as the image fills them (bytes it
    /// does not carry read 0xFF, an erased flash). The interrupt vector table
    /// sits here; see [`crate::avr_vectors`]. Shorter when the image ends sooner.
    pub flash_head: Vec<u8>,
}

/// How many leading flash bytes [`AvrImage::flash_head`] keeps: room for the
/// largest AVR vector table (the `ATmega2560`'s 57 vectors of 4 bytes).
pub const FLASH_HEAD: usize = 256;

/// Lay `data`, which starts at flash address `at`, over `head`.
fn fill_head(head: &mut Vec<u8>, at: u32, data: &[u8]) {
    let at = at as usize;
    if at >= FLASH_HEAD {
        return;
    }
    let end = (at + data.len()).min(FLASH_HEAD);
    if head.len() < end {
        head.resize(end, 0xFF);
    }
    head[at..end].copy_from_slice(&data[..end - at]);
}

impl AvrImage {
    /// Read the layout of the image at `path`, choosing the format by extension
    /// (`.hex` is Intel HEX; anything else must be an ELF).
    ///
    /// # Errors
    ///
    /// When the file cannot be read, or is not a well-formed image of its
    /// format (bad record checksum, no data, a program header pointing outside
    /// the file, not 32-bit little-endian).
    pub fn read(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path)
            .with_context(|| format!("reading firmware '{}'", path.display()))?;
        let is_hex = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("hex"));
        let parsed = if is_hex {
            parse_ihex(&bytes)
        } else {
            parse_elf(&bytes)
        };
        // One flat message: callers that print only the outermost error still
        // say what is wrong with the file.
        parsed.map_err(|e| anyhow::anyhow!("firmware '{}': {e:#}", path.display()))
    }

    /// One past the highest flash byte the image occupies (0 when it carries
    /// no flash at all): the flash a part needs to hold it.
    #[must_use]
    pub fn flash_bytes(&self) -> u64 {
        (self.segments.iter())
            .filter(|s| s.region == Region::Flash)
            .map(Segment::end)
            .max()
            .unwrap_or(0)
    }

    /// Refuse an image that does not fit `capacity`, naming the region and both
    /// sizes. `part` names the core in the message.
    ///
    /// # Errors
    ///
    /// When any segment ends past its region's capacity.
    pub fn check_fits(&self, capacity: &Capacity, part: &str, path: &Path) -> Result<()> {
        for region in [
            Region::Flash,
            Region::Data,
            Region::Eeprom,
            Region::Fuses,
            Region::Lock,
        ] {
            let need = (self.segments.iter())
                .filter(|s| s.region == region)
                .map(Segment::end)
                .max()
                .unwrap_or(0);
            let have = capacity.of(region);
            if need > have {
                let advice = if region == Region::Flash {
                    "it was built for a larger part. Build the firmware for this part, or, \
                     if the board carries the larger part, bind that one"
                } else {
                    "it was built for a different part. Build the firmware for this part"
                };
                bail!(
                    "firmware '{}' does not fit this MCU's {}: the image needs {need} bytes \
                     but the {part} has {have}; {advice}. Nothing was loaded.",
                    path.display(),
                    region.name(),
                );
            }
        }
        Ok(())
    }
}

/// Parse Intel HEX: data records (type 00) placed through extended segment
/// (02) and extended linear (04) addresses; end-of-file (01) resets the base,
/// as a multi-part file needs; start-address records (03, 05) carry no data.
fn parse_ihex(bytes: &[u8]) -> Result<AvrImage> {
    let text = std::str::from_utf8(bytes).context("an Intel HEX file must be ASCII text")?;
    let mut base: u32 = 0;
    let mut runs: Vec<(u32, Vec<u8>)> = Vec::new();
    for (n, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let line_no = n + 1;
        let Some(body) = line.strip_prefix(':') else {
            bail!("line {line_no} is not an Intel HEX record (no leading ':')");
        };
        if body.len() % 2 != 0 || body.len() < 10 {
            bail!("line {line_no} is not a whole Intel HEX record");
        }
        let rec: Vec<u8> = (0..body.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&body[i..i + 2], 16))
            .collect::<Result<_, _>>()
            .with_context(|| format!("line {line_no} holds a non-hex digit"))?;
        let len = usize::from(rec[0]);
        if rec.len() != len + 5 {
            bail!(
                "line {line_no} declares {len} data bytes but carries {}",
                rec.len().saturating_sub(5)
            );
        }
        if rec.iter().fold(0u8, |a, b| a.wrapping_add(*b)) != 0 {
            bail!("line {line_no} fails its checksum");
        }
        let offset = u32::from(u16::from_be_bytes([rec[1], rec[2]]));
        let data = &rec[4..4 + len];
        match rec[3] {
            0x00 => {
                if data.is_empty() {
                    continue;
                }
                let at = base.wrapping_add(offset);
                match runs.last_mut() {
                    Some((start, run)) if u64::from(*start) + run.len() as u64 == u64::from(at) => {
                        run.extend_from_slice(data);
                    }
                    _ => runs.push((at, data.to_vec())),
                }
            }
            0x01 => base = 0,
            0x02 if len == 2 => base = u32::from(u16::from_be_bytes([data[0], data[1]])) << 4,
            0x04 if len == 2 => base = u32::from(u16::from_be_bytes([data[0], data[1]])) << 16,
            0x03 | 0x05 => {}
            kind => bail!("line {line_no} has an unsupported record type {kind:02X}"),
        }
    }
    if runs.is_empty() {
        bail!("the Intel HEX file carries no data records, so there is no program to run");
    }
    let mut segments = Vec::with_capacity(runs.len());
    for (start, run) in &runs {
        let Some((region, rel)) = Region::of(*start) else {
            bail!("data at 0x{start:08X} lies outside every AVR memory");
        };
        segments.push(Segment {
            region,
            start: rel,
            len: run.len() as u64,
        });
    }
    let mut flash_head = Vec::new();
    for (start, run) in &runs {
        if Region::of(*start).is_some_and(|(region, _)| region == Region::Flash) {
            fill_head(&mut flash_head, *start, run);
        }
    }
    Ok(AvrImage {
        segments,
        hex_runs: runs,
        flash_head,
    })
}

/// Parse the program headers of a little-endian ELF32, keeping the segments
/// simavr's ELF loader loads (`PT_LOAD` with file bytes, below the last AVR
/// region) and refusing headers that point outside the file (simavr's reader
/// calls `exit(1)` on a short read).
fn parse_elf(bytes: &[u8]) -> Result<AvrImage> {
    const PT_LOAD: u32 = 1;
    let u16_at = |at: usize| -> Result<u16> {
        let b = bytes
            .get(at..at + 2)
            .context("the ELF header is truncated")?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    };
    let u32_at = |at: usize| -> Result<u32> {
        let b = bytes
            .get(at..at + 4)
            .context("the ELF header is truncated")?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    if bytes.get(..4) != Some(&[0x7F, b'E', b'L', b'F'][..]) {
        bail!("not an ELF file (no ELF magic); AVR firmware must be .hex or .elf");
    }
    if bytes.get(4) != Some(&1) || bytes.get(5) != Some(&1) {
        bail!("not a 32-bit little-endian ELF, which every AVR image is");
    }
    let phoff = usize::try_from(u32_at(0x1C)?)?;
    let phentsize = usize::from(u16_at(0x2A)?);
    let phnum = usize::from(u16_at(0x2C)?);
    if phnum == 0 {
        bail!("the ELF has no program headers, so there is nothing to load");
    }
    if phentsize < 32 {
        bail!("the ELF's program headers are {phentsize} bytes, short of ELF32's 32");
    }
    let mut segments = Vec::new();
    let mut flash_head = Vec::new();
    for i in 0..phnum {
        let ph = phoff
            .checked_add(
                i.checked_mul(phentsize)
                    .context("program header table overflows")?,
            )
            .context("program header table overflows")?;
        if bytes.len() < ph.saturating_add(32) {
            bail!("program header {i} lies past the end of the file");
        }
        let (kind, offset, paddr) = (u32_at(ph)?, u32_at(ph + 4)?, u32_at(ph + 12)?);
        let (filesz, memsz) = (u32_at(ph + 16)?, u32_at(ph + 20)?);
        if kind != PT_LOAD || filesz == 0 {
            continue;
        }
        let Some((region, start)) = Region::of(paddr) else {
            continue;
        };
        if u64::from(offset) + u64::from(filesz) > bytes.len() as u64 {
            bail!(
                "program header {i} places {filesz} bytes at file offset {offset}, past the \
                 end of the {}-byte file",
                bytes.len()
            );
        }
        if region == Region::Flash {
            let from = offset as usize;
            if let Some(data) = bytes.get(from..from + filesz as usize) {
                fill_head(&mut flash_head, start, data);
            }
        }
        // EEPROM is copied, never zero-filled; the other regions fill to memsz.
        let len = if region == Region::Eeprom {
            filesz
        } else {
            filesz.max(memsz)
        };
        segments.push(Segment {
            region,
            start,
            len: u64::from(len),
        });
    }
    if !segments.iter().any(|s| s.region == Region::Flash) {
        bail!("the ELF loads nothing into flash, so there is no program to run");
    }
    Ok(AvrImage {
        segments,
        hex_runs: Vec::new(),
        flash_head,
    })
}

#[cfg(test)]
mod tests {
    use super::{parse_elf, parse_ihex, AvrImage, Capacity, Region, Segment};
    use std::path::Path;

    const MEGA168: Capacity = Capacity {
        flash: 16384,
        data: 1024,
        eeprom: 512,
        fuses: 6,
        lock: 1,
    };

    fn hex_of(image: &[u8]) -> String {
        let mut s = String::new();
        for (i, chunk) in image.chunks(16).enumerate() {
            let at = u16::try_from(i * 16).unwrap();
            let mut rec = vec![u8::try_from(chunk.len()).unwrap()];
            rec.extend_from_slice(&at.to_be_bytes());
            rec.push(0);
            rec.extend_from_slice(chunk);
            let sum = rec
                .iter()
                .fold(0u8, |a, b| a.wrapping_add(*b))
                .wrapping_neg();
            rec.push(sum);
            s.push(':');
            for b in rec {
                use std::fmt::Write as _;
                write!(s, "{b:02X}").unwrap();
            }
            s.push('\n');
        }
        s + ":00000001FF\n"
    }

    #[test]
    fn hex_runs_merge_contiguous_records_and_measure_flash() {
        let image = parse_ihex(hex_of(&[0xAA; 40]).as_bytes()).unwrap();
        assert_eq!(image.hex_runs.len(), 1);
        assert_eq!(image.hex_runs[0].1.len(), 40);
        assert_eq!(image.flash_bytes(), 40);
    }

    #[test]
    fn hex_with_no_data_or_a_bad_checksum_is_refused_not_crashed_on() {
        assert!(parse_ihex(b":00000001FF\n").is_err());
        assert!(parse_ihex(b"").is_err());
        assert!(parse_ihex(b":0100000000FE\n").is_err(), "checksum");
        assert!(parse_ihex(b"garbage\n").is_err());
        assert!(parse_ihex(b":05000000000000\n").is_err(), "short record");
    }

    #[test]
    fn hex_extended_linear_address_places_data_above_64k() {
        // :02000004 0001 F9 -> base 0x10000.
        let text = ":020000040001F9\n:0100000055AA\n:00000001FF\n";
        let image = parse_ihex(text.as_bytes()).unwrap();
        assert_eq!(image.hex_runs[0].0, 0x1_0000);
        assert_eq!(image.flash_bytes(), 0x1_0001);
    }

    #[test]
    fn an_image_past_the_parts_flash_is_refused_with_both_sizes() {
        let image = AvrImage {
            segments: vec![Segment {
                region: Region::Flash,
                start: 0,
                len: 20480,
            }],
            hex_runs: Vec::new(),
            flash_head: Vec::new(),
        };
        let msg = image
            .check_fits(&MEGA168, "atmega168", Path::new("fw.elf"))
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains("20480") && msg.contains("16384") && msg.contains("atmega168"),
            "{msg}"
        );
        let fits = AvrImage {
            segments: vec![Segment {
                region: Region::Flash,
                start: 0,
                len: 16384,
            }],
            hex_runs: Vec::new(),
            flash_head: Vec::new(),
        };
        assert!(fits
            .check_fits(&MEGA168, "atmega168", Path::new("fw"))
            .is_ok());
    }

    fn elf(paddr: u32, filesz: u32, memsz: u32, data_len: usize) -> Vec<u8> {
        let mut e = vec![0x7F, b'E', b'L', b'F', 1, 1, 1, 0];
        e.resize(52, 0);
        e[0x12] = 83;
        e[0x1C..0x20].copy_from_slice(&52u32.to_le_bytes());
        e[0x2A..0x2C].copy_from_slice(&32u16.to_le_bytes());
        e[0x2C..0x2E].copy_from_slice(&1u16.to_le_bytes());
        for v in [1, 84, paddr, paddr, filesz, memsz, 5, 2] {
            e.extend_from_slice(&u32::to_le_bytes(v));
        }
        e.resize(84 + data_len, 0);
        e
    }

    #[test]
    fn elf_segments_land_in_their_regions_and_count_the_zero_fill() {
        let image = parse_elf(&elf(0, 100, 300, 100)).unwrap();
        assert_eq!(image.flash_bytes(), 300);
        let eeprom = parse_elf(&elf(0x0081_0000, 600, 600, 600)).unwrap_err();
        assert!(
            eeprom.to_string().contains("nothing into flash"),
            "{eeprom}"
        );
    }

    #[test]
    fn elf_headers_pointing_past_the_file_are_refused() {
        assert!(parse_elf(&elf(0, 100, 100, 10)).is_err());
        assert!(parse_elf(&[0x7F, b'E', b'L', b'F', 1, 1]).is_err());
        assert!(
            parse_elf(&[0x7F, b'E', b'L', b'F', 2, 1, 1, 0]).is_err(),
            "ELF64"
        );
        assert!(parse_elf(b"not an elf").is_err());
    }
}
