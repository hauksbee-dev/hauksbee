// What kind of zip is this, read from the zip's own directory in the browser.
//
// The board zone accepts a zip because a gerber set is a zip. A firmware zip
// is also a legitimate input, just to the other slot: the engine's
// `firmware_input.rs` will happily find (or build) an image inside a
// PlatformIO project archive. Dropping that project zip on the board zone used
// to hand it to the gerber extractor, which answered with a gerber complaint
// about a file that has no gerbers in it and never will. The user's mistake was
// one slot; the error talked about apertures.
//
// So before routing a zip, read its file list. This costs two small reads at
// the tail of the file regardless of how big the archive is: the End of Central
// Directory record, then the central directory it points at. No entry is
// decompressed and no entry body is read.

/** Local-file-header / central-directory / EOCD signatures. */
const EOCD_SIG = 0x06054b50
const CD_SIG = 0x02014b50

/** The EOCD sits within 22 + 65535 bytes of the end (the trailing comment is
 *  the only variable part). Scanning the last 64 KiB covers every real zip. */
const EOCD_SCAN_BYTES = 66 * 1024

/** A central directory large enough to hold this is a zip with tens of
 *  thousands of entries; reading more than this to classify one drop is not
 *  worth it, and the first entries are enough to recognise a project. */
const MAX_CD_BYTES = 4 * 1024 * 1024

/** Names that only exist in a firmware project or a firmware build tree. */
const FIRMWARE_MARKERS: { re: RegExp; says: string }[] = [
  { re: /(^|\/)platformio\.ini$/i, says: 'platformio.ini' },
  { re: /(^|\/)\.pio\//i, says: '.pio/ build tree' },
  { re: /(^|\/)sdkconfig(\.|$)/i, says: 'sdkconfig (ESP-IDF)' },
  { re: /\.elf$/i, says: 'a compiled .elf' },
  { re: /\.hex$/i, says: 'a compiled .hex' },
]

/** Names that mean this zip really is a fab package, which outranks any
 *  firmware-looking file that happens to sit beside it.
 *
 *  Only unambiguous extensions belong here. `.txt` was the tempting one to add
 *  (an Excellon drill file is often named `.txt`) and would have been wrong:
 *  every CMake-based firmware project ships a `CMakeLists.txt`, which would
 *  have made every ESP-IDF zip claim to be a fab package. A drill file with a
 *  `.txt` name in a zip with no copper layer in it is a zip with nothing to
 *  extract either way. */
const GERBER_MARKERS: RegExp[] = [
  /\.(gbr|gbl|gtl|gbs|gts|gbo|gto|gm1|gko|gpt|gpb|gpi)$/i,
  /\.(drl|xln|dri)$/i,
  /\.(kicad_pcb|kicad_sch|brd|pcbdoc|d356|board)$/i,
]

export interface ZipReport {
  /** The first four bytes were a zip local-file-header signature. */
  isZip: boolean
  /** Entry names read from the central directory (may be truncated; see
   *  `truncated`). Empty when the directory could not be read. */
  names: string[]
  /** Only part of the directory was read, so absence of a marker is not proof
   *  of absence in the archive. */
  truncated: boolean
  /** Human names of the firmware markers found, in the order listed above. */
  firmwareMarkers: string[]
  /** True when at least one entry looks like a fab/board file. */
  hasBoardFiles: boolean
}

const EMPTY: ZipReport = {
  isZip: false, names: [], truncated: false, firmwareMarkers: [], hasBoardFiles: false,
}

/** PK\x03\x04, the local file header every non-empty zip opens with. */
async function looksLikeZip(f: File): Promise<boolean> {
  if (f.size < 4) return false
  const head = new Uint8Array(await f.slice(0, 4).arrayBuffer())
  return head[0] === 0x50 && head[1] === 0x4b && head[2] === 0x03 && head[3] === 0x04
}

/** Byte offset of the EOCD record, or -1. Scans backwards so a zip whose
 *  comment happens to contain the signature does not win over the real one. */
function findEocd(view: DataView): number {
  for (let i = view.byteLength - 22; i >= 0; i--) {
    if (view.getUint32(i, true) === EOCD_SIG) return i
  }
  return -1
}

/**
 * Classify a dropped zip from its directory. Never throws: a zip this cannot
 * parse (Zip64, a spanned archive, a truncated download) comes back as
 * `isZip` with no markers, which routes it exactly as an unclassifiable zip
 * was always routed, to the board extractor.
 */
export async function inspectZip(f: File): Promise<ZipReport> {
  try {
    if (!await looksLikeZip(f)) return EMPTY

    const tailStart = Math.max(0, f.size - EOCD_SCAN_BYTES)
    const tail = new Uint8Array(await f.slice(tailStart).arrayBuffer())
    const tailView = new DataView(tail.buffer, tail.byteOffset, tail.byteLength)
    const eocd = findEocd(tailView)
    if (eocd < 0) return { ...EMPTY, isZip: true }

    const cdSize = tailView.getUint32(eocd + 12, true)
    const cdOffset = tailView.getUint32(eocd + 16, true)
    // Zip64 parks 0xFFFFFFFF in these fields and puts the truth in its own
    // record. Not worth parsing for a classification: fall through as an
    // unclassified zip rather than reading a bogus offset.
    if (cdSize === 0xffffffff || cdOffset === 0xffffffff) return { ...EMPTY, isZip: true }

    const readBytes = Math.min(cdSize, MAX_CD_BYTES)
    const cd = new Uint8Array(await f.slice(cdOffset, cdOffset + readBytes).arrayBuffer())
    const cdView = new DataView(cd.buffer, cd.byteOffset, cd.byteLength)

    const names: string[] = []
    const decoder = new TextDecoder()
    let p = 0
    while (p + 46 <= cd.byteLength && cdView.getUint32(p, true) === CD_SIG) {
      const nameLen = cdView.getUint16(p + 28, true)
      const extraLen = cdView.getUint16(p + 30, true)
      const commentLen = cdView.getUint16(p + 32, true)
      const nameAt = p + 46
      if (nameAt + nameLen > cd.byteLength) break
      names.push(decoder.decode(cd.subarray(nameAt, nameAt + nameLen)))
      p = nameAt + nameLen + extraLen + commentLen
    }

    const firmwareMarkers: string[] = []
    for (const m of FIRMWARE_MARKERS) {
      if (names.some(n => m.re.test(n))) firmwareMarkers.push(m.says)
    }
    const hasBoardFiles = names.some(n => GERBER_MARKERS.some(re => re.test(n)))

    return {
      isZip: true,
      names,
      truncated: readBytes < cdSize,
      firmwareMarkers,
      hasBoardFiles,
    }
  } catch {
    // A read that failed tells us nothing; say nothing.
    return EMPTY
  }
}

/**
 * Should a zip dropped on the BOARD zone be treated as firmware instead?
 *
 * Yes only when it carries firmware markers and carries nothing a board
 * extractor would want. A zip with both (a repo holding hardware/ and
 * firmware/) stays a board drop: the board is what the board zone was asked
 * about, and the firmware jack is one click away.
 */
export function zipIsFirmwareOnly(z: ZipReport): boolean {
  return z.isZip && z.firmwareMarkers.length > 0 && !z.hasBoardFiles
}
