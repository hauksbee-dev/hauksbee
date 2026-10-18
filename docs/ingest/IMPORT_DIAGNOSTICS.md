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
line is shown as causal. Fab packages are told to remove manifest ambiguity or provide
the original CAD source.

The `import_failure` record on `/api/analyze` is structured so the app does not
have to guess from prose. `kind` is one of `truncated` (a reader claimed the
file but it ends inside a bracket or string), `syntax_error`, `wrong_content`
(no reader claims the bytes although the extension is a board one; a Git LFS
pointer or a gerber file saved under a board name land here, with
`detected_format` and `expected_format`), `unsupported_extension`,
`empty_file`, `unsupported_format`, `invalid_content`, `fab_package`,
`schematic_sheets` and `unreadable`. `file` and `extension` name the upload;
`line` and `byte` say where the parser stopped when it said; `suggested_fix` is
the one-sentence first step; `title`, `cause` and `steps` are the plain words
the app shows; `supported_formats` serves the readable board formats as data
(`vendor`, `exts`). The wording lives in `crates/hauksbee-engine/src/import_failure.rs`;
the browser keeps its own phrase matching only as a fallback for a server that
predates these fields. The design route (`/api/analyze` with a project) returns
the same record as the single-file route.

This is not a general-purpose standalone fixture reducer. A one-line excerpt
may depend on surrounding declarations; it is the smallest honest location the
parser supplied, not a promise that the line reproduces the failure alone.

## Persistence and export

The same additive `import_diagnostics` or `import_failure` record travels in
the `/api/analyze` JSON, saved browser session, JSON export, and standalone HTML
report. The HTML includes the per-object basis and all suggested fixes. Older
reports without these optional fields continue to render.

