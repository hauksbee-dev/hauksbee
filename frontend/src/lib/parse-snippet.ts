// A parse failure names where it stopped ("parse error at line 1 (byte 58):
// unclosed list"). The place is easier to act on with the text around it, so
// this reads the dropped file and shows the line with a caret under the byte.

export interface ParseLocation { line: number | null; byte: number | null }

/** The line and byte offset a read error names, when it names any. */
export function parseLocation(message: string): ParseLocation | null {
  const line = /\bline (\d+)/i.exec(message)
  const byte = /\bbyte (\d+)/i.exec(message)
  if (!line && !byte) return null
  return { line: line ? Number(line[1]) : null, byte: byte ? Number(byte[1]) : null }
}

export interface Snippet { lineNo: number; text: string; caret: number | null }

const WINDOW = 110

/** The source line at the error (clipped to a readable window) and where in it
 *  the caret goes. Reads only the head of the file, up to the offset plus a margin. */
export async function snippetAt(file: Blob, loc: ParseLocation): Promise<Snippet | null> {
  const upTo = loc.byte !== null ? loc.byte + 400 : 64 * 1024
  const bytes = new Uint8Array(await file.slice(0, upTo).arrayBuffer())
  const lines: { start: number; end: number }[] = []
  let start = 0
  for (let i = 0; i <= bytes.length; i++) {
    if (i === bytes.length || bytes[i] === 0x0a) { lines.push({ start, end: i }); start = i + 1 }
  }
  const byte = loc.byte !== null ? Math.min(loc.byte, bytes.length) : null
  const index = byte !== null
    ? lines.findIndex(l => byte >= l.start && byte <= l.end)
    : loc.line !== null ? loc.line - 1 : -1
  if (index < 0 || index >= lines.length) return null
  const decoder = new TextDecoder()
  const lineText = (i: number) => decoder.decode(bytes.slice(lines[i].start, lines[i].end)).replace(/\r$/, '')
  // "Unclosed list" is reported at the end of the file, on an empty line: the
  // text worth showing is the last line that has any, with the caret after it.
  let at = index
  while (at > 0 && lineText(at).trim() === '') at--
  const moved = at !== index
  let text = lineText(at)
  let caret = moved ? text.length : byte !== null ? decoder.decode(bytes.slice(lines[at].start, byte)).length : null
  if (text.length > WINDOW) {
    const from = Math.max(0, Math.min((caret ?? 0) - Math.floor(WINDOW / 2), text.length - WINDOW))
    const more = from + WINDOW < text.length
    text = `${from > 0 ? '…' : ''}${text.slice(from, from + WINDOW)}${more ? '…' : ''}`
    if (caret !== null) caret = caret - from + (from > 0 ? 1 : 0)
  }
  return { lineNo: at + 1, text, caret }
}
