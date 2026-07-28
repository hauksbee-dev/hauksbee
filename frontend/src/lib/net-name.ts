// How a net name is SHOWN, as distinct from what it IS.
//
// KiCad escapes characters that would otherwise collide with its own syntax
// inside a label: `/` is the sheet-path separator, so a literal slash in a net
// name is written `{slash}`. The escaped token travels through extraction into
// every surface that names a net, and a reader who opens the schematic sees
// `Net-(U4-LNA_IN/RF)` where the app was showing `Net-(U4-LNA_IN{slash}RF)`.
//
// The engine already de-escapes its prose (`plain::readable`). This is the same
// step for the web UI, in one place, so the selection card, the net lists, the
// probe/scope lists and the viewer tooltip cannot drift apart.
//
// The escaped form remains the identity: it is what the netlist, the check
// specs and every lookup key use. Only the rendered string changes. Never feed
// the output of this back into a lookup.

/** KiCad's label escapes, mirroring `hauksbee-extract`'s `normalize_label`. */
const ESCAPES: [string, string][] = [['{slash}', '/']]

/** A net name as a person should read it. Identity-preserving: display only. */
export function displayNet(name: string): string {
  let out = name
  for (const [token, literal] of ESCAPES) {
    if (out.includes(token)) out = out.split(token).join(literal)
  }
  return out
}
