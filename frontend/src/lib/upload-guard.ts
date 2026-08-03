// Everything the browser can say about a dropped file BEFORE a byte of it is
// read or sent.
//
// The failure this exists to stop: a 300 MB CAD export dropped on the board
// zone used to be handed straight to `File.arrayBuffer()`, which pulled the
// whole thing into the JS heap on the main thread. The page froze for over
// seven minutes with no error, no progress and no way out, and the request
// that followed was going to be refused by the server anyway. Every rejection
// below is one the client can make for certain, instantly, from metadata.
//
// It deliberately does not try to be a format sniffer. The engine's extractors
// are the authority on whether a file is a readable board, and a small file
// with an odd name is cheap to just send. What is checked here is only what is
// both certain and expensive to get wrong.

import { BOARD_EXTS, FIRMWARE_EXTS, acceptedFormatsSentence } from './board-formats'

/** The server's body limit (`MAX_UPLOAD_BYTES` in
 *  crates/hauksbee-server/src/frontdoor.rs). Anything larger is refused with a
 *  413 no matter what the client does, so the client refuses it first and says
 *  so in bytes the user can compare against their own file. */
export const MAX_UPLOAD_BYTES = 256 * 1024 * 1024

/** How the limit is written in prose. Kept next to the number so the two can
 *  never disagree. */
export const MAX_UPLOAD_LABEL = '256 MB'

/** Above this, an unrecognised extension stops being worth a round trip: a
 *  small mystery file is cheap to try, a large one is a several-minute upload
 *  that ends in "could not read the file". */
const UNKNOWN_EXT_LIMIT_BYTES = 20 * 1024 * 1024

/** Extensions the engine's board extractors claim, plus the two firmware
 *  extensions the board zone re-routes rather than rejects. Derived from the
 *  one accepted-formats list so a refusal can never name a shorter set of
 *  formats than the picker offers. */
const KNOWN_BOARD_EXTS = [...BOARD_EXTS, ...FIRMWARE_EXTS.map(e => e.toLowerCase())]

/** Human file size. Binary units, one decimal past MB, because the number's
 *  only job is to be compared against the stated limit. */
export function formatBytes(n: number): string {
  if (n < 1024) return `${n} bytes`
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(0)} KB`
  if (n < 1024 * 1024 * 1024) return `${(n / (1024 * 1024)).toFixed(1)} MB`
  return `${(n / (1024 * 1024 * 1024)).toFixed(2)} GB`
}

function extensionOf(name: string): string {
  // `.kicad_pcb` has a dot in the middle of nothing else, so a plain
  // lastIndexOf is right; a file with no dot at all yields ''.
  const i = name.lastIndexOf('.')
  return i < 0 ? '' : name.slice(i).toLowerCase()
}

/** True when the name ends in an extension the board zone knows what to do
 *  with (extract, or re-route to the firmware jack). */
export function hasKnownBoardExtension(name: string): boolean {
  return KNOWN_BOARD_EXTS.includes(extensionOf(name))
}

/**
 * The reason this file cannot be analyzed, in one sentence a person can act
 * on, or null when there is no certain reason to refuse it.
 *
 * Called before any read and before any request. Every branch names the
 * measurement it made (the size, the limit, the extension) so the message is
 * checkable rather than a verdict.
 */
export function precheckBoardFile(f: File): string | null {
  if (f.size === 0) {
    return `“${f.name}” is empty (0 bytes). Nothing was written to it, so there is `
      + 'no copper to read. Check the export finished, then drop it again.'
  }
  if (f.size > MAX_UPLOAD_BYTES) {
    return `“${f.name}” is ${formatBytes(f.size)}. This server accepts up to `
      + `${MAX_UPLOAD_LABEL} per upload. If this is a full CAD project or a `
      + 'library-heavy export, export the gerbers and drill file instead and drop '
      + 'that zip: the circuit is reverse-extracted from the copper, and the zip is '
      + 'usually a few MB.'
  }
  if (!hasKnownBoardExtension(f.name) && f.size > UNKNOWN_EXT_LIMIT_BYTES) {
    return `“${f.name}” does not look like a board file: nothing recognises `
      + `${extensionOf(f.name) || 'a name with no extension'}, and at `
      + `${formatBytes(f.size)} it is too large to be worth trying. `
      + `Accepted: ${acceptedFormatsSentence()}.`
  }
  return null
}

/**
 * Turn a failed analysis into a message that names what actually went wrong.
 *
 * Three cases the user experiences completely differently used to arrive as
 * one string, "Analysis failed: Failed to fetch":
 *  - the server refused the body as too large (413, or a connection reset
 *    mid-upload, which is how a body-limit rejection often looks from fetch),
 *  - the connection dropped or the server went away,
 *  - the server answered, and its answer is the error worth showing.
 *
 * `size` is the board's size when known, so the limit can be stated against a
 * real number instead of in the abstract.
 */
export function analysisFailureMessage(
  e: unknown,
  opts: { status?: number; size?: number | null } = {},
): string {
  const { status, size } = opts
  const sizeClause = size != null ? ` The file is ${formatBytes(size)}.` : ''

  if (status === 413) {
    return `The server refused this board as too large.${sizeClause} The limit is `
      + `${MAX_UPLOAD_LABEL} per upload. For a big CAD export, export the gerbers and `
      + 'drill file and drop that zip instead; the circuit is reverse-extracted from '
      + 'the copper either way, and the zip is usually a few MB.'
  }

  // fetch() rejects with a TypeError for every transport-level failure, and
  // its message is browser-specific and useless on its own ("Failed to
  // fetch", "NetworkError when attempting to fetch resource", "Load
  // failed"). Say what it means instead of relaying it.
  const isNetwork = e instanceof TypeError
    || (e instanceof Error && /failed to fetch|networkerror|load failed|network error/i.test(e.message))
  if (isNetwork) {
    const big = size != null && size > 32 * 1024 * 1024
    return 'The connection to the server dropped before the analysis came back.'
      + sizeClause
      + (big
        ? ` A board near the ${MAX_UPLOAD_LABEL} upload limit can be cut off mid-upload; `
          + 'exporting the gerbers and drill file and dropping that zip instead is both '
          + 'smaller and faster.'
        : ' Check that hauksbee is still running, then try again.')
  }

  return `Analysis failed: ${e instanceof Error ? e.message : String(e)}`
}
