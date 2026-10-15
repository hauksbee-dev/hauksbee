// The accepted board formats, served. The engine owns the one table
// (`ACCEPTED_FORMATS` in hauksbee-bind's board_input.rs) and sends it with
// `/api/startup` as `board_formats`; the file picker's `accept` attribute, the
// drop zone's small print, the rejection text, the oversize/unknown-extension
// refusal, the firmware re-route and the SPICE-deck route all read it from
// here. This module holds no list of its own: before the server has answered
// (or against an older server that never sends it) every list is empty, and the
// callers degrade to "no client-side opinion" (the picker offers every file, the
// precheck refuses only on size).

/** One row of the engine's table. */
export interface ServedBoardFormat {
  /** The ECAD tool or standard that produces the file. */
  vendor: string
  /** Extensions, with the dot, in the casing a user would recognise. */
  exts: string[]
  /** `board` goes to the board analyzer, `deck` to the SPICE deck simulator. */
  route: 'board' | 'deck'
  /** Offered by the picker but not advertised in prose: an extension that is
   *  only sometimes a board (an Excellon drill file named `.txt`). */
  quiet: boolean
}

/** The shape of `board_formats` in `/api/startup`. */
export interface ServedBoardFormats {
  formats: ServedBoardFormat[]
  /** Compiled firmware, which the board zone re-routes rather than refusing. */
  firmware_exts: string[]
}

let served: ServedBoardFormats | null = null

/** Record the engine's table. Called once by the boot that reads
 *  `/api/startup`, before the shell (and so any reader of it) mounts. */
export function setServedBoardFormats(next: ServedBoardFormats | null | undefined): void {
  served = next ?? null
}

const rows = (): ServedBoardFormat[] => served?.formats ?? []

/** Compiled firmware extensions, lowercase, with the dot. */
export function firmwareExts(): string[] {
  return (served?.firmware_exts ?? []).map(e => e.toLowerCase())
}

/** Extensions routed to the SPICE deck simulator, lowercase, with the dot. */
export function deckExts(): string[] {
  return rows().filter(f => f.route === 'deck').flatMap(f => f.exts.map(e => e.toLowerCase()))
}

/** Deck extensions a board format also claims (`.net`: SPICE or a KiCad
 *  netlist), which only the file's content can tell apart. */
export function ambiguousDeckExts(): string[] {
  const board = new Set(rows().filter(f => f.route === 'board').flatMap(f => f.exts.map(e => e.toLowerCase())))
  return deckExts().filter(e => board.has(e))
}

/** Every extension the board picker offers, lowercase, deduped. `.net` is
 *  claimed by both KiCad and SPICE, and the drop router sniffs the content to
 *  tell them apart (see `isSpiceDeck`). */
export function boardExts(): string[] {
  return [...new Set(rows().flatMap(f => f.exts.map(e => e.toLowerCase())))]
}

/** The `accept` attribute for the board file input, or undefined (accept any
 *  file) when the table is not here. Firmware is included because a firmware
 *  file dropped on the board zone is re-routed, not refused, and a picker that
 *  cannot select it makes that route unreachable from the keyboard. */
export function boardAcceptAttr(): string | undefined {
  const all = [...rows().flatMap(f => f.exts), ...(served?.firmware_exts ?? [])]
  return all.length > 0 ? all.join(',') : undefined
}

/** The formats as one sentence fragment, for a refusal or a dead end:
 *  "KiCad .kicad_pcb / .kicad_sch / .net, Eagle .brd, ...". Empty without the table. */
export function acceptedFormatsSentence(): string {
  return rows()
    .filter(f => !f.quiet)
    .map(f => `${f.vendor} ${f.exts.join(' / ')}`)
    .join(', ')
}

/** Does `name` end in one of `exts` (lowercase, with the dot)? Matches the WHOLE
 *  extension, so `board.tar.gz` is `.tar.gz`, not `.gz`. */
export function endsWithAny(name: string, exts: readonly string[]): boolean {
  const lower = name.toLowerCase()
  return exts.some(e => lower.endsWith(e))
}

/**
 * Strip the engine's own trailing "Supported: ..." clause off a board-read
 * error, leaving the diagnostic.
 *
 * The engine's refusal carries its own copy of the list (generated from the same
 * table, but without the SPICE decks the browser routes elsewhere), and it
 * renders directly above this app's list, so the card would carry two lists.
 * The diagnostic half ("unrecognized board format; tried altium, eagle, ...")
 * is kept verbatim; the list is dropped and `acceptedFormatsSentence()` takes
 * its place. A message with no such clause is returned untouched.
 */
export function withoutEngineFormatList(message: string): string {
  return message.replace(/\s*Supported:.*$/s, '').trim() || message
}
