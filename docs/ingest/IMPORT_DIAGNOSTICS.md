# Import diagnostics

The browser report (`hauksbee serve`) shows an **Import coverage** panel for
every board it reads: what the reader recovered before binding, checks or
simulation ran. It is not an electrical verdict.

Per recovered component:

- **recovered**: reference, value, pins and board coordinate all present.
- **partial**: names the absent fields (no pins, no coordinate, ...).
- **high / medium / low confidence**: derived from those fields and from
  whether connectivity was declared by the source or reconstructed from
  copper. Not a probability.
- **Show** pans the map only when the reader supplied a real coordinate; an
  unplaced object stays in the table as `not placeable`.

Located recovered objects are painted green, located partial objects amber.
Missing or refused objects are described in text, never drawn at a guessed
point. The `missing/refused limits` count is a count of named reader
limitations, not of physical parts.

For Gerber reconstruction, a synthetic net (`NET_n`) is an explicit issue:
**Inspect NET_n** highlights the located objects on it. Supplying the original
CAD layout, ODB++ or IPC-D-356 connectivity replaces the geometric inference.

## Parser refusals

An unreadable input carries the parser stage and a stage-specific fix. The
excerpt contains only the line the parser named, capped at 300 characters; if
the parser did not localise the failure, no line is shown. Board-as-Code
points at `hauksbee from-code` for the full diagnostic.

## Persistence

The `import_diagnostics` / `import_failure` record is additive and travels in
the `/api/analyze` JSON, the saved browser session, the JSON export and the
standalone HTML report. Older reports without it still render.
