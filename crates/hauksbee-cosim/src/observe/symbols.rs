//! Name the function a program counter sits in, from a firmware ELF.
//!
//! A parked core is only useful to a bench engineer when it says *where*:
//! `Error_Handler` and `main` mean different things. The MCU backends report a
//! raw program counter; this resolves it against the image's `.symtab`.
//!
//! Little-endian ELF32 only (every ARM Cortex-M, RISC-V32 and Xtensa image the
//! co-sim runs); anything else (a raw `.bin`, an Intel hex, a stripped or
//! 64-bit image) yields `None`, and the bench row then shows the bare address.

use std::path::Path;

const ELF_MAGIC: &[u8; 4] = b"\x7fELF";
/// `STT_NOTYPE` and `STT_FUNC`: the symbol kinds that name code.
const CODE_SYMBOL_TYPES: [u8; 2] = [0, 2];
/// How far past a size-less symbol's start a program counter still counts as
/// inside it (assembly handlers often carry no `.size`).
const SIZELESS_REACH: u32 = 32;

/// Code symbols of one image, sorted by address.
#[derive(Debug, Clone, Default)]
pub struct SymbolTable {
    /// `(start, size, name)`, Thumb bit cleared, sorted by `start`.
    symbols: Vec<(u32, u32, String)>,
}

impl SymbolTable {
    /// Read the symbol table of the ELF at `path`.
    #[must_use]
    pub fn from_elf(path: &Path) -> Option<Self> {
        Self::from_image(&std::fs::read(path).ok()?)
    }

    /// Read the symbol table of an in-memory ELF32 image.
    #[must_use]
    pub fn from_image(image: &[u8]) -> Option<Self> {
        let u16_at = |at: usize| Some(u16::from_le_bytes(image.get(at..at + 2)?.try_into().ok()?));
        let u32_at = |at: usize| Some(u32::from_le_bytes(image.get(at..at + 4)?.try_into().ok()?));
        let usize_at = |at: usize| u32_at(at).and_then(|v| usize::try_from(v).ok());
        if image.get(..4)? != ELF_MAGIC || image.get(4) != Some(&1) || image.get(5) != Some(&1) {
            return None;
        }
        let shoff = usize_at(0x20)?;
        let shentsize = usize::from(u16_at(0x2E)?);
        let shnum = usize::from(u16_at(0x30)?);
        let section = |i: usize| shoff.checked_add(i.checked_mul(shentsize)?);
        let mut symbols = Vec::new();
        for i in 0..shnum {
            let sh = section(i)?;
            if u32_at(sh + 4)? != 2 {
                continue; // SHT_SYMTAB only
            }
            let (sym_off, sym_size, link) =
                (usize_at(sh + 16)?, usize_at(sh + 20)?, usize_at(sh + 24)?);
            let str_off = usize_at(section(link)? + 16)?;
            for sym in (sym_off..sym_off.checked_add(sym_size)?).step_by(16) {
                let (Some(name_idx), Some(value), Some(size)) =
                    (usize_at(sym), u32_at(sym + 4), u32_at(sym + 8))
                else {
                    break;
                };
                let (Some(&info), Some(shndx)) = (image.get(sym + 12), u16_at(sym + 14)) else {
                    break;
                };
                if shndx == 0 || !CODE_SYMBOL_TYPES.contains(&(info & 0xF)) || value == 0 {
                    continue;
                }
                let Some(rest) = str_off.checked_add(name_idx).and_then(|at| image.get(at..))
                else {
                    continue;
                };
                let Some(end) = rest.iter().position(|&b| b == 0) else {
                    continue;
                };
                let Ok(name) = std::str::from_utf8(&rest[..end]) else {
                    continue;
                };
                // Local labels (`$t`, `.L...`, mapping symbols) are not functions.
                if name.is_empty() || name.starts_with('$') || name.starts_with(".L") {
                    continue;
                }
                symbols.push((value & !1, size, name.to_string()));
            }
        }
        if symbols.is_empty() {
            return None;
        }
        symbols.sort_by_key(|(start, _, _)| *start);
        Some(Self { symbols })
    }

    /// The symbol containing `pc`, preferring the closest preceding start.
    #[must_use]
    pub fn lookup(&self, pc: u32) -> Option<&str> {
        let pc = pc & !1;
        let after = self.symbols.partition_point(|(start, _, _)| *start <= pc);
        // Scan back a few entries: aliases share a start, and a sized symbol
        // may sit just behind a size-less label.
        self.symbols[..after]
            .iter()
            .rev()
            .take(8)
            .find(|(start, size, _)| {
                let reach = if *size == 0 { SIZELESS_REACH } else { *size };
                pc - *start < reach
            })
            .map(|(_, _, name)| name.as_str())
    }
}

/// A minimal ELF32 LE image with a `.symtab` holding the given symbols, for
/// tests here and in the bench builder.
///
/// # Panics
///
/// Panics if the image would exceed 4 GiB (never, for a test fixture).
#[cfg(test)]
#[must_use]
pub fn test_image(symbols: &[(&str, u32, u32)]) -> Vec<u8> {
    let mut strtab = vec![0_u8];
    let mut entries = Vec::new();
    for (name, value, size) in symbols {
        let idx = strtab.len();
        strtab.extend_from_slice(name.as_bytes());
        strtab.push(0);
        let mut e = Vec::new();
        e.extend_from_slice(&u32::try_from(idx).unwrap().to_le_bytes());
        e.extend_from_slice(&value.to_le_bytes());
        e.extend_from_slice(&size.to_le_bytes());
        e.push(2); // STT_FUNC
        e.push(0);
        e.extend_from_slice(&1_u16.to_le_bytes()); // a real section
        entries.push(e);
    }
    let symtab: Vec<u8> = entries.concat();
    let mut img = vec![0_u8; 0x34];
    img[..4].copy_from_slice(ELF_MAGIC);
    img[4] = 1;
    img[5] = 1;
    let symtab_off = img.len();
    img.extend_from_slice(&symtab);
    let strtab_off = img.len();
    img.extend_from_slice(&strtab);
    let shoff = img.len();
    // Section 0 null, 1 symtab (link -> 2), 2 strtab.
    let sh = |ty: u32, off: usize, size: usize, link: u32| {
        let mut s = vec![0_u8; 40];
        s[4..8].copy_from_slice(&ty.to_le_bytes());
        s[16..20].copy_from_slice(&u32::try_from(off).unwrap().to_le_bytes());
        s[20..24].copy_from_slice(&u32::try_from(size).unwrap().to_le_bytes());
        s[24..28].copy_from_slice(&link.to_le_bytes());
        s
    };
    let null = vec![0_u8; 40];
    let sym_sh = sh(2, symtab_off, symtab.len(), 2);
    let str_sh = sh(3, strtab_off, strtab.len(), 0);
    img.extend_from_slice(&null);
    img.extend_from_slice(&sym_sh);
    img.extend_from_slice(&str_sh);
    img[0x20..0x24].copy_from_slice(&u32::try_from(shoff).unwrap().to_le_bytes());
    img[0x2E..0x30].copy_from_slice(&40_u16.to_le_bytes());
    img[0x30..0x32].copy_from_slice(&3_u16.to_le_bytes());
    img
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_a_program_counter_to_its_function() {
        let table = SymbolTable::from_image(&test_image(&[
            ("main", 0x0800_0101, 0x40),
            ("Error_Handler", 0x0800_0201, 0x8),
            ("Default_Handler", 0x0800_0301, 0),
        ]))
        .unwrap();
        assert_eq!(table.lookup(0x0800_0120), Some("main"));
        assert_eq!(table.lookup(0x0800_0202), Some("Error_Handler"));
        assert_eq!(table.lookup(0x0800_0302), Some("Default_Handler"));
        // Past the end of a sized symbol: not in it.
        assert_eq!(table.lookup(0x0800_0250), None);
        assert_eq!(table.lookup(0x0000_0010), None);
    }

    #[test]
    fn refuses_non_elf_input() {
        assert!(SymbolTable::from_image(b"not an elf").is_none());
        assert!(SymbolTable::from_image(&[]).is_none());
    }
}
