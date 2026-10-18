// Engine ids inside a sentence, shown as their words.
//
// The engine writes some of its prose with an internal id inside it: the
// binder's "assumed corners (generic_pmos_sot_corner_high_vth, ...)", a
// stand-in model's "'signal_diode_1n4148_fallback'". The app shows those ids as
// the labels from the shared table (labels.ts), with the id kept in a tooltip;
// the report exports (HTML and Markdown) have no tooltip and no chips, so they
// use the same rule here and print the words in place of the id.
//
// The rule is deliberately narrow. A lower-case word with underscores is also
// how a net is spelt ("spi_clk"), how a TOML key is spelt ("value_re") and how
// a file is spelt, so only an id the label table lists, or one of the generic
// stand-in model shapes, is replaced. Everything else is left exactly as
// written.

import { LABEL_ENTRIES, labelFor } from './labels'

/** An engine id in a sentence (lower-case words joined by underscores), never
 *  part of a file name, a path or inline code. */
export const RAW_ID = /(?<![\w./`-])[a-z][a-z0-9]*(?:_[a-z0-9]+)+(?![\w./`-])/g

const KNOWN: ReadonlySet<string> = new Set(LABEL_ENTRIES.map(entry => entry.id))

/** The generic stand-in models the binder names when a board gives only a
 *  class: the family, its corners and the per-part fallbacks. */
const STAND_IN_MODEL = /^generic_|_corner_|_fallback$/

/** Whether an id found in a sentence is an engine id with words of its own. */
export function isLabelledId(id: string): boolean {
  return KNOWN.has(id) || STAND_IN_MODEL.test(id)
}

/** The text with each engine id replaced by its label. `keep` holds names the
 *  board itself uses (nets, parts, its file name): they are left as written
 *  even when they are shaped like an id. */
export function withLabels(text: string, keep: ReadonlySet<string> = new Set()): string {
  return text.replace(RAW_ID, id => (isLabelledId(id) && !keep.has(id) ? labelFor(id) : id))
}

/** The names a report's own board uses, so `withLabels` never rewrites one. */
export function boardNames(report: {
  nets?: string[]
  components?: { reference: string }[]
  board_name?: string
  file_name?: string
}): ReadonlySet<string> {
  const names = new Set<string>()
  for (const net of report.nets ?? []) names.add(net)
  for (const part of report.components ?? []) names.add(part.reference)
  if (report.board_name) names.add(report.board_name)
  if (report.file_name) names.add(report.file_name)
  return names
}
