# Import diagnostics

The browser report shows an **Import coverage** panel for every successfully
read board. It describes what the selected reader actually recovered before
binding, checks, or simulation. It is not a second electrical verdict and it is
not a claim about the manufactured board.

Each recovered component has one row:

- **recovered** means its reference, value, pins, and board coordinate are all
  present;
- **partial** names the absent fields (for example, no pins or no coordinate);
- **high / medium / low confidence** is derived from those fields and whether
  connectivity was declared by the source or reconstructed from manufacturing
  copper. It is not a probability;
- **Show** pans the map only when the reader supplied a real coordinate. An
  unplaced object stays in the table as `not placeable`; Hauksbee does not draw
  it at a guessed point.

The overlay paints located recovered objects green and located partial objects
amber. Missing or refused objects cannot in general be counted or located from
an incomplete source, so their reader limitation is shown as text rather than
as a fabricated marker. The summary's `missing/refused limits` count is a count
of those named limitations, not a count of physical parts.

For Gerber reconstruction, a synthetic net boundary is an explicit issue. If
the reader can name the reconstructed net, **Inspect NET_n** highlights only
the located imported objects actually attached to it. If none were placeable,
the panel says so. Supplying the original CAD layout, ODB++, or authoritative
IPC-D-356 connectivity replaces that geometric inference.

## Parser refusals

An unreadable input keeps the ordinary refusal and adds the parser stage plus a
stage-specific suggested fix. The failure excerpt is conservatively minimized:
it contains only the exact line number named by the underlying parser, capped
at 300 characters. If the parser did not localize the failure, no arbitrary
line is shown as causal. Board-as-Code points to `hauksbee from-code` for the
full diagnostic; fab packages are told to remove manifest ambiguity or provide
the original CAD source.

This is not a general-purpose standalone fixture reducer. A one-line excerpt
may depend on surrounding declarations; it is the smallest honest location the
parser supplied, not a promise that the line reproduces the failure alone.

## Persistence and export

The same additive `import_diagnostics` or `import_failure` record travels in
the `/api/analyze` JSON, saved browser session, JSON export, and standalone HTML
report. The HTML includes the per-object basis and all suggested fixes. Older
reports without these optional fields continue to render.

