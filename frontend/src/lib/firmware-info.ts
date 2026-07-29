// What is actually in the firmware file the user handed us.
//
// The engine does not tell us. `firmware_input.rs` resolves an upload to bytes
// and carries only `{name, bytes, note}`; the single piece of binary
// introspection anywhere in the Rust tree is `hauksbee-mcu/src/elf.rs`, which
// reads the `e_machine` half-word to refuse an architecture mismatch and keeps
// nothing. None of it reaches the browser: the wire carries a co-sim section,
// not a firmware description.
//
// So this reads the file the browser already holds. Everything below is
// measured from those bytes: no field is inferred from the extension, and a
// field that cannot be read is absent rather than guessed. The architecture
// table mirrors `hauksbee-mcu/src/elf.rs` exactly, so the name shown here is
// the name the loader would use when it accepts or rejects the image.

/** ELF `e_machine` values the engine knows, from `hauksbee-mcu/src/elf.rs`. */
const MACHINES: Record<number, string> = {
  0x28: 'ARM',
  0x53: 'AVR',
  0x5e: 'Xtensa',
  0xf3: 'RISC-V',
}

/** SHT_NOBITS: occupies address space but no file bytes (this is `.bss`). */
const SHT_NOBITS = 8

export interface FirmwareSection {
  name: string
  /** Bytes the section occupies in the loaded image. */
  size: number
  /** True for SHT_NOBITS (`.bss`): counted in RAM, absent from the file. */
  noBits: boolean
}

export interface FirmwareInfo {
  name: string
  /** Bytes on disk. */
  size: number
  /** Lowercase hex SHA-256 of the whole file, first 16 chars. */
  sha256Short: string | null
  /** What the bytes say the file is, never the extension. */
  format: 'ELF' | 'Intel HEX' | 'Zip archive' | 'unknown'
  /** ELF only, and only when `e_machine` is one the engine recognises. */
  arch?: string
  /** ELF only: the raw `e_machine`, shown when the engine has no name for it,
   *  so an unsupported image is still identifiable rather than just "unknown". */
  machineCode?: number
  /** ELF only: 32 or 64. */
  elfClass?: 32 | 64
  /** ELF only. */
  endian?: 'little' | 'big'
  /** ELF only: `e_entry`, as hex. */
  entry?: string
  /** ELF only, and only when the section header table is readable. Sorted by
   *  size, largest first, since that is the question being asked of it. */
  sections?: FirmwareSection[]
  /** Set when the file claims to be an ELF but the header is truncated or the
   *  section table does not parse. The fields that did read are still shown. */
  parseNote?: string
}

function isIntelHex(bytes: Uint8Array): boolean {
  // Intel HEX is ASCII records that each start with ':'. Test a few, not just
  // the first, so a stray colon in a binary cannot pass.
  if (bytes[0] !== 0x3a) return false
  let records = 0
  for (let i = 0; i < Math.min(bytes.length, 512); i++) {
    const b = bytes[i]
    if (b === 0x3a) records++
    // Anything outside printable ASCII + CR/LF disqualifies it.
    else if (b !== 0x0d && b !== 0x0a && (b < 0x20 || b > 0x7e)) return false
  }
  return records >= 1
}

/** Read the file's bytes and say what they are. Never throws. */
export async function readFirmwareInfo(file: File): Promise<FirmwareInfo> {
  const info: FirmwareInfo = {
    name: file.name,
    size: file.size,
    sha256Short: null,
    format: 'unknown',
  }

  let buf: ArrayBuffer
  try {
    buf = await file.arrayBuffer()
  } catch {
    return info
  }
  const bytes = new Uint8Array(buf)

  // The hash is of the whole file, so it is the thing to quote when asking
  // "is the image I uploaded the image I built".
  try {
    const digest = await crypto.subtle.digest('SHA-256', buf)
    info.sha256Short = [...new Uint8Array(digest)]
      .map(b => b.toString(16).padStart(2, '0'))
      .join('')
      .slice(0, 16)
  } catch {
    // No SubtleCrypto (an insecure origin): leave it null rather than showing
    // a hash of something else.
  }

  const isElf = bytes.length >= 4
    && bytes[0] === 0x7f && bytes[1] === 0x45 && bytes[2] === 0x4c && bytes[3] === 0x46
  const isZip = bytes.length >= 4
    && bytes[0] === 0x50 && bytes[1] === 0x4b && bytes[2] === 0x03 && bytes[3] === 0x04

  if (isZip) {
    info.format = 'Zip archive'
    return info
  }
  if (!isElf) {
    if (isIntelHex(bytes)) info.format = 'Intel HEX'
    return info
  }

  info.format = 'ELF'
  if (bytes.length < 0x34) {
    info.parseNote = 'the ELF header is truncated'
    return info
  }

  const cls = bytes[4]
  const data = bytes[5]
  const is64 = cls === 2
  const little = data !== 2
  info.elfClass = is64 ? 64 : 32
  info.endian = little ? 'little' : 'big'

  const dv = new DataView(buf)
  const u16 = (o: number) => dv.getUint16(o, little)
  const u32 = (o: number) => dv.getUint32(o, little)
  // Offsets past 2^53 cannot occur in a firmware image; Number is safe here.
  const u64 = (o: number) => Number(dv.getBigUint64(o, little))

  const machine = u16(0x12)
  info.machineCode = machine
  if (MACHINES[machine]) info.arch = MACHINES[machine]

  try {
    const entry = is64 ? u64(0x18) : u32(0x18)
    info.entry = `0x${entry.toString(16)}`

    const shoff = is64 ? u64(0x28) : u32(0x20)
    const shentsize = u16(is64 ? 0x3a : 0x2e)
    const shnum = u16(is64 ? 0x3c : 0x30)
    const shstrndx = u16(is64 ? 0x3e : 0x32)

    if (shoff === 0 || shnum === 0 || shstrndx >= shnum) {
      info.parseNote = 'the image carries no section table'
      return info
    }
    if (shoff + shnum * shentsize > bytes.length) {
      info.parseNote = 'the section table runs past the end of the file'
      return info
    }

    // The string table holding section names, found via its own header.
    const strHdr = shoff + shstrndx * shentsize
    const strOff = is64 ? u64(strHdr + 0x18) : u32(strHdr + 0x10)
    const strSize = is64 ? u64(strHdr + 0x20) : u32(strHdr + 0x14)
    const nameAt = (off: number): string => {
      if (off >= strSize) return ''
      let end = strOff + off
      while (end < bytes.length && bytes[end] !== 0) end++
      return new TextDecoder().decode(bytes.subarray(strOff + off, end))
    }

    const sections: FirmwareSection[] = []
    for (let i = 0; i < shnum; i++) {
      const h = shoff + i * shentsize
      const shName = u32(h)
      const shType = u32(h + 4)
      const shSize = is64 ? u64(h + 0x20) : u32(h + 0x14)
      const name = nameAt(shName)
      // Unnamed and zero-length sections are structural padding, not content.
      if (!name || shSize === 0) continue
      sections.push({ name, size: shSize, noBits: shType === SHT_NOBITS })
    }
    sections.sort((a, b) => b.size - a.size)
    if (sections.length > 0) info.sections = sections
  } catch {
    info.parseNote = 'the section table could not be read'
  }

  return info
}

/** Bytes as a person reads them. */
export function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KiB`
  return `${(n / (1024 * 1024)).toFixed(2)} MiB`
}
