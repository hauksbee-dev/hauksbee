// The accepted board formats, once.
//
// This list used to exist in four places: the file picker's `accept` attribute,
// the drop card's small print, the rejection card's "accepted:" line, and the
// oversize/unknown-extension refusal. They had already drifted. The rejection
// card is the worst place for a list to be short, because it is the only one
// read by someone whose file was just refused, and an Altium user reading a
// list with no Altium in it concludes the tool cannot do their format at all.
//
// One array now, and every surface derives from it. Adding a format means
// adding a row here.

export interface BoardFormat {
  /** The ECAD tool or standard that produces the file. */
  vendor: string
  /** Extensions, with the dot, in the casing a user would recognise. */
  exts: string[]
  /** Shown in the picker's `accept` list but not in the prose: an extension
   *  that is only sometimes a board (an Excellon drill file named `.txt`), so
   *  offering it is right and advertising it as a board format is not. */
  quiet?: boolean
}

export const BOARD_FORMATS: BoardFormat[] = [
  { vendor: 'KiCad', exts: ['.kicad_pcb', '.kicad_sch'] },
  { vendor: 'Eagle', exts: ['.brd'] },
  { vendor: 'Altium', exts: ['.PcbDoc'] },
  { vendor: 'IPC', exts: ['.d356', '.xml'] },
  { vendor: 'fab archive', exts: ['.zip', '.tgz', '.tar.gz', '.tar'] },
  { vendor: 'board-as-code', exts: ['.board'] },
  { vendor: 'Excellon drill', exts: ['.txt'], quiet: true },
]

/** Compiled firmware, which the board zone re-routes to the firmware jack
 *  rather than refusing. */
export const FIRMWARE_EXTS = ['.elf', '.hex']

/** Every extension the board picker offers, lowercase, with the dot. */
export const BOARD_EXTS: string[] = BOARD_FORMATS.flatMap(f => f.exts.map(e => e.toLowerCase()))

/** The `accept` attribute for the board file input. Firmware is included
 *  because a firmware file dropped on the board zone is re-routed, not
 *  refused, and a picker that cannot select it makes that route unreachable
 *  from the keyboard. */
export const BOARD_ACCEPT_ATTR = [...BOARD_FORMATS.flatMap(f => f.exts), ...FIRMWARE_EXTS].join(',')

/** The formats as one sentence fragment, for a refusal or a dead end:
 *  "KiCad .kicad_pcb / .kicad_sch, Eagle .brd, Altium .PcbDoc, ...". */
export function acceptedFormatsSentence(): string {
  return BOARD_FORMATS
    .filter(f => !f.quiet)
    .map(f => `${f.vendor} ${f.exts.join(' / ')}`)
    .join(', ')
}

/** The vendor names alone, for a line that has no room for extensions. */
export function acceptedVendors(): string {
  return BOARD_FORMATS.filter(f => !f.quiet).map(f => f.vendor).join(', ')
}

/**
 * Strip the engine's own trailing "Supported: ..." clause off a board-read
 * error, leaving the diagnostic.
 *
 * The engine's refusal (crates/hauksbee-engine/src/board_input.rs) ends with a
 * format list of its own that is one entry short: it names KiCad, Eagle,
 * IPC-D-356 and Board-as-Code, and not Altium, even though the same sentence's
 * diagnostic half reports having TRIED the altium reader. Rendered as-is it
 * landed directly above this app's complete list, so the card carried two
 * format lists that disagreed, and the shorter one came first.
 *
 * The diagnostic half ("unrecognized board format; tried altium, eagle, ...")
 * is the part worth reading and is kept verbatim. The list is dropped and the
 * card renders `acceptedFormatsSentence()` in its place, so there is one list
 * on screen and it is the right one. When the engine's message has no such
 * clause (any other read failure) this returns it untouched.
 */
export function withoutEngineFormatList(message: string): string {
  return message.replace(/\s*Supported:.*$/s, '').trim() || message
}
