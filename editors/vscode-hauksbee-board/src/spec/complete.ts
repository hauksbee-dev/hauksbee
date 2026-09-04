// Completions and hovers for spec TOML, derived entirely from the generated
// schema. Pure functions over (text, position): no `vscode` import, so the
// whole surface is unit-testable.
//
// The schema is the single source of truth for what can be completed, which is
// the point of generating it from the Rust types: a new field on `Spec` shows up
// in the completion list and the hover the moment the schema is regenerated,
// with the field's doc comment as its documentation.

import {
  boundsNote,
  seedNumber,
  SpecSchema,
  typeIncludes,
  type PropertyInfo,
} from "./schemaModel";
import { parseToml, type Pos, type Span } from "./tomlIndex";

export type SuggestionKind = "key" | "value" | "table";

export interface Suggestion {
  label: string;
  /**
   * What to insert. When `snippet` is set this is VS Code snippet syntax, so the
   * cursor lands on the value and a closed vocabulary opens as a choice list:
   * accepting `kind` in a `[[supply]]` leaves you picking `ideal | bench | …`
   * rather than typing it.
   */
  insertText: string;
  snippet?: boolean;
  kind: SuggestionKind;
  detail: string;
  documentation: string;
  /**
   * The range this completion replaces. Always set by `suggest`; the individual
   * builders leave it to their caller, which is the only place that knows the
   * cursor. Without it VS Code substitutes its own word range, which stops at
   * `+`, `/` and `.`: completing `+5V` over a typed `+5` would insert `++5V`,
   * and a board path `boards/bl` would become `boards/boards/blinky.kicad_pcb`.
   */
  replace?: Span;
  /** Sort ahead of the rest: required keys, canonical spellings. */
  preferred?: boolean;
  /**
   * Sort BELOW the rest: a key that belongs to a different assertion kind. An
   * `[[assert]]` has thirty-odd fields and any one kind uses five, so the
   * ordering is what makes the list usable.
   */
  deprioritised?: boolean;
  /** Order within a tier, when the builder has a better one than alphabetical. */
  rank?: number;
}

/** What the cursor is sitting in. */
export interface CursorContext {
  what: "table-header" | "key" | "value" | "none";
  /** The table the cursor is inside, as a schema path. */
  schemaPath: string[];
  /** For `what: "value"`, the key being assigned. */
  key?: string;
  /** The text already typed that a completion should replace. */
  span: Span;
  /** True when the cursor is between quotes, so values must not re-quote. */
  inString: boolean;
}

/** Net-valued keys: the places a board net name belongs. */
export const NET_KEYS = [
  "net",
  "to",
  "a",
  "b",
  "wiper",
  "net_a",
  "net_b",
  "cs_net",
  "supply_net",
  "nets",
  "suppress_rail",
];

const HEADER = /^\s*\[\[?\s*([A-Za-z0-9_.\- ]*)/;

/**
 * The table path in effect at `line`, by scanning upward for the nearest table
 * header. Deliberately does NOT use the full parser: completions must work in a
 * buffer that is mid-edit and does not parse.
 */
export function contextPathAt(text: string, line: number): string[] {
  const lines = text.split(/\r?\n/);
  for (let i = Math.min(line, lines.length - 1); i >= 0; i--) {
    const m = /^\s*\[\[?\s*([A-Za-z0-9_."'.\-]+)\s*\]\]?/.exec(lines[i] ?? "");
    if (m) {
      return m[1]
        .split(".")
        .map((s) => s.replace(/^["']|["']$/g, ""))
        .filter((s) => s !== "");
    }
  }
  return [];
}

/**
 * Is the cursor inside a multi-line STRING value?
 *
 * A `[[sensor]]`'s inline `spec = """ … """` is a whole nested TOML document,
 * headers and all (see crates/hauksbee-ci/examples/lm75_thermostat.toml). The
 * line-scanning above cannot see that, and would happily describe the string's
 * `[sensor]` line as the spec's own `[[sensor]]` array and offer `SensorAttach`
 * keys inside a sensor definition, where a different vocabulary applies. When
 * the buffer parses, the string's value span says exactly where to stop.
 *
 * Multi-line ARRAYS are deliberately not excluded: completing net names on the
 * inner lines of `nets = [ … ]` is the point.
 */
export function insideMultilineString(text: string, pos: Pos): boolean {
  const doc = parseToml(text);
  // A buffer mid-edit often does not parse; then the line scan is all we have.
  if (doc.errors.length > 0) return false;
  for (const e of doc.entries) {
    if (typeof e.value !== "string") continue;
    const { start, end } = e.valueSpan;
    if (start.line === end.line) continue;
    if (pos.line < start.line || pos.line > end.line) continue;
    if (pos.line === start.line && pos.col < start.col) continue;
    if (pos.line === end.line && pos.col > end.col) continue;
    return true;
  }
  return false;
}

export function cursorContext(text: string, pos: Pos): CursorContext {
  const lines = text.split(/\r?\n/);
  const line = lines[pos.line] ?? "";
  const before = line.slice(0, pos.col);
  const schemaPath = contextPathAt(text, pos.line - 1 >= 0 ? pos.line - 1 : 0);
  const headerHere = HEADER.exec(before);

  if (headerHere && !before.includes("]")) {
    // The span starts at the OPENING BRACKET, not after it. A table completion
    // then rewrites the brackets too, so choosing `[ac]` having typed `[[`
    // produces `[ac]` and not the unparseable `[[ac]`.
    const start = before.indexOf("[");
    return {
      what: "table-header",
      schemaPath: [],
      span: { start: { line: pos.line, col: start }, end: pos },
      inString: false,
    };
  }

  // A multi-line array (`nets = [` … `]`) puts the cursor on a line with no `=`
  // of its own, but it is still a value position for the key that opened the
  // bracket. Without this, `nets = [\n  "` would offer table keys instead of
  // board nets, which is exactly where net completion is most wanted.
  const open = openArrayAt(lines, pos);
  const eq = open ? -1 : before.indexOf("=");
  if (open) {
    const quote = /(["'])([^"']*)$/.exec(before);
    const typed = quote?.[2] ?? "";
    return {
      what: "value",
      schemaPath,
      key: open,
      span: { start: { line: pos.line, col: pos.col - typed.length }, end: pos },
      inString: !!quote,
    };
  }
  if (eq >= 0) {
    const key = before.slice(0, eq).trim().replace(/^["']|["']$/g, "");
    const valuePart = before.slice(eq + 1);
    const quote = /(^|[[,\s])(["'])([^"']*)$/.exec(valuePart);
    if (quote) {
      const typed = quote[3];
      return {
        what: "value",
        schemaPath,
        key,
        span: { start: { line: pos.line, col: pos.col - typed.length }, end: pos },
        inString: true,
      };
    }
    const bare = /([A-Za-z0-9_.\-+]*)$/.exec(valuePart);
    const typed = bare?.[1] ?? "";
    return {
      what: "value",
      schemaPath,
      key,
      span: { start: { line: pos.line, col: pos.col - typed.length }, end: pos },
      inString: false,
    };
  }

  const word = /([A-Za-z0-9_\-]*)$/.exec(before);
  const typed = word?.[1] ?? "";
  // Only offer keys at the start of a statement; mid-comment or mid-value is not
  // a key position.
  if (/#/.test(before)) {
    return { what: "none", schemaPath, span: { start: pos, end: pos }, inString: false };
  }
  return {
    what: "key",
    schemaPath,
    span: { start: { line: pos.line, col: pos.col - typed.length }, end: pos },
    inString: false,
  };
}

/**
 * The key of an array literal the cursor sits inside, when the array was opened
 * on an earlier line and is not yet closed. Line-based on purpose: the buffer
 * mid-edit has an unterminated array and does not parse.
 */
function openArrayAt(lines: string[], pos: Pos): string | undefined {
  for (let i = pos.line - 1; i >= 0; i--) {
    const l = lines[i] ?? "";
    if (/^\s*\[/.test(l)) return undefined; // a table header, not an array
    const m = /^\s*([A-Za-z0-9_."'\-]+)\s*=\s*\[/.exec(l);
    if (!m) continue;
    let depth = 0;
    for (let j = i; j < pos.line; j++) depth += bracketDelta(lines[j] ?? "");
    depth += bracketDelta((lines[pos.line] ?? "").slice(0, pos.col));
    return depth > 0 ? m[1].replace(/^["']|["']$/g, "") : undefined;
  }
  return undefined;
}

/**
 * The things declared elsewhere in this document that `key` refers to: a
 * scenario's `id`, an inline profile's `id`, a peripheral or sensor `id`, a
 * component reference already named by an override or tolerance rule.
 *
 * Read with a line scan rather than the parse tree, because a half-typed value
 * is exactly the state the buffer is in when this is asked for.
 */
function documentReferences(
  text: string,
  key: string
): { value: string; detail: string; documentation: string }[] {
  const sources: Record<string, { tables: string[]; field: string; what: string }> = {
    scenario: { tables: ["scenario"], field: "id", what: "declared [[scenario]]" },
    profile: { tables: ["profile"], field: "id", what: "inline [[profile]]" },
    id: {
      tables: ["peripheral", "sensor"],
      field: "id",
      what: "declared [[peripheral]] / [[sensor]]",
    },
    ref: {
      tables: ["override", "tolerance", "decoupling.override"],
      field: "ref",
      what: "reference already named in this spec",
    },
    part: { tables: ["scenario"], field: "part", what: "part already named in this spec" },
  };
  const source = sources[key];
  if (!source || text === "") return [];

  const out: { value: string; detail: string; documentation: string }[] = [];
  const seen = new Set<string>();
  let table: string | undefined;
  for (const line of text.split(/\r?\n/)) {
    const header = /^\s*\[\[?\s*([A-Za-z0-9_.]+)\s*\]\]?/.exec(line);
    if (header) {
      table = header[1];
      continue;
    }
    if (!table || !source.tables.includes(table)) continue;
    const m = new RegExp(`^\\s*${source.field}\\s*=\\s*["']([^"']+)["']`).exec(line);
    if (m && !seen.has(m[1])) {
      seen.add(m[1]);
      out.push({
        value: m[1],
        detail: source.what,
        documentation: `Declared as \`${source.field} = "${m[1]}"\` in a \`[[${table}]]\` block.`,
      });
    }
  }
  return out;
}

/** Net `[` minus `]` on a line, ignoring anything quoted or commented. */
function bracketDelta(line: string): number {
  let depth = 0;
  let quote: string | undefined;
  for (const ch of line) {
    if (quote) {
      if (ch === quote) quote = undefined;
      continue;
    }
    if (ch === '"' || ch === "'") quote = ch;
    else if (ch === "#") break;
    else if (ch === "[") depth++;
    else if (ch === "]") depth--;
  }
  return depth;
}

export interface SuggestOptions {
  /** The whole document, for completing references to things declared in it. */
  text?: string;
  /** Board net names, when the spec's `board` resolved and could be listed. */
  nets?: string[];
  /** Keys already present in the current table, so they are not offered twice. */
  presentKeys?: string[];
  /** Board files in the workspace, relative to the spec, for `board = "…"`. */
  boards?: string[];
  /** True when the editor can expand snippets (VS Code can; a test may not). */
  snippets?: boolean;
}

/**
 * The fields each assertion kind actually reads, from `Assertion::validate` and
 * the checks in `assertions.rs`. Nothing else can express this: the schema has
 * one flat `Assertion` with every field optional, so without it the completion
 * list for `[[assert]]` is thirty fields in alphabetical order and the five that
 * matter are scattered through it.
 */
export const ASSERT_FIELDS_BY_KIND: Record<string, string[]> = {
  voltage: ["net", "min", "max", "after_ms"],
  uart: ["contains", "matches", "mcu"],
  toggle: ["net", "freq_hz", "min_toggles", "tolerance"],
  no_faults: [],
  max_current: ["ref", "amps", "after_ms"],
  max_temp: ["ref", "celsius"],
  peripheral: ["id", "bytes", "field", "min", "max"],
  rail_window: ["net", "min", "max", "dip_below", "for_max_ms", "recover_to", "recover_within_ms", "scenario"],
  protection_trip: ["supply_net", "expect_trip", "scenario"],
  boot_coverage: ["net", "min", "deadline_ms"],
  phase_margin: ["net", "min", "max"],
  ac_gain: ["net", "min", "max", "freq_hz"],
  hwtrace: ["trace"],
  model_coverage: ["min_critical", "min_resolved", "max_active_unresolved"],
};

/** Fields every assertion can carry, whatever its kind. */
const ASSERT_ALWAYS = ["kind", "name"];

/**
 * Fields each SUPPLY kind reads, from `SupplySpec::validate` and `build_supply`.
 * The first entry of each is the one the loader REQUIRES for that kind and the
 * schema cannot express, so it leads the list rather than sitting 13th.
 */
export const SUPPLY_FIELDS_BY_KIND: Record<string, string[]> = {
  ideal: ["volts", "current_limit_a", "r_out_ohms"],
  bench: ["volts", "current_limit_a", "r_out_ohms", "ripple_vpp", "ripple_hz"],
  wall: ["volts", "current_limit_a", "r_out_ohms", "ripple_vpp", "ripple_hz"],
  usb: ["usb", "r_out_ohms"],
  battery: ["chemistry", "cells", "capacity_mah", "soc", "r_internal_ohms",
    "protection_trip_a", "protection_delay_ms", "protection_reset_a"],
};

/** Fields each PERIPHERAL type reads, from `PeripheralSpec::validate` + the runner. */
export const PERIPHERAL_FIELDS_BY_TYPE: Record<string, string[]> = {
  pushbutton: ["net", "to", "bounce_ms", "initial"],
  toggle: ["net", "to", "initial"],
  potentiometer: ["a", "wiper", "b", "r_total", "initial"],
  encoder: ["net_a", "net_b", "vhigh", "initial"],
  stimulus: ["net", "waveform", "offset", "amplitude", "freq_hz", "pwl"],
  i2c_eeprom: ["address", "size"],
  i2c_lm75: ["address", "temp_c"],
  spi_eeprom: ["size", "cs_net"],
  spi_mcp3008: ["vref", "cs_net"],
  vcd_sink: ["nets", "vcd_path"],
};

/** Fields every peripheral can carry, whatever its type. */
const PERIPHERAL_ALWAYS = ["id", "type", "ref", "pin"];

/**
 * The fields relevant to the table the cursor is in, given the discriminant
 * written in it, plus the fields that are always relevant. Undefined when the
 * table has no discriminant, or none has been written yet.
 */
function relevantFields(
  schemaPath: string[],
  discriminant: string | undefined
): { relevant: string[]; always: string[]; label: string } | undefined {
  if (!discriminant) return undefined;
  const table = schemaPath.join(".");
  if (table === "assert" && ASSERT_FIELDS_BY_KIND[discriminant]) {
    return {
      relevant: ASSERT_FIELDS_BY_KIND[discriminant],
      always: ASSERT_ALWAYS,
      label: `${discriminant} assertion`,
    };
  }
  if (table === "supply" && SUPPLY_FIELDS_BY_KIND[discriminant]) {
    return {
      relevant: SUPPLY_FIELDS_BY_KIND[discriminant],
      always: ["net", "kind"],
      label: `${discriminant} supply`,
    };
  }
  if (table === "peripheral" && PERIPHERAL_FIELDS_BY_TYPE[discriminant]) {
    return {
      relevant: PERIPHERAL_FIELDS_BY_TYPE[discriminant],
      always: PERIPHERAL_ALWAYS,
      label: `${discriminant} peripheral`,
    };
  }
  return undefined;
}

export function suggest(
  text: string,
  pos: Pos,
  schema: SpecSchema,
  opts: SuggestOptions = {}
): Suggestion[] {
  if (insideMultilineString(text, pos)) return [];
  const ctx = cursorContext(text, pos);
  const snippets = opts.snippets ?? true;
  const stamp = (items: Suggestion[]): Suggestion[] =>
    items.map((i) => ({ ...i, replace: ctx.span }));
  switch (ctx.what) {
    case "table-header":
      return stamp(ranked(tableSuggestions(schema, snippets)));
    case "key":
      return stamp(
        ranked(
          keySuggestions(schema, ctx.schemaPath, opts.presentKeys ?? keysOnScreen(text, pos), {
            snippets,
            kind: assertKindAt(text, pos),
          })
        )
      );
    case "value":
      // Values keep their declared order: an enum reads best in the order the
      // Rust type lists it, and nets in the order the board reports them.
      return stamp(valueSuggestions(schema, ctx, { ...opts, text }));
    default:
      return [];
  }
}

/**
 * The order the list is presented in: what this context needs, then the rest,
 * then what belongs to a different assertion kind. `specLanguage` mirrors this
 * into `sortText` so VS Code shows the same order.
 */
export function ranked(items: Suggestion[]): Suggestion[] {
  const tier = (s: Suggestion) => (s.deprioritised ? 2 : s.preferred ? 0 : 1);
  return [...items].sort(
    (a, b) =>
      tier(a) - tier(b) ||
      // Inside the preferred tier, keep the order the builder chose: for an
      // assertion that is `net, min, max, after_ms`, the order a person fills
      // them in. Alphabetising would put `after_ms` first and defeat the point.
      (a.rank ?? 0) - (b.rank ?? 0) ||
      a.label.localeCompare(b.label)
  );
}

/**
 * The `kind` of the `[[assert]]` block the cursor is in, when there is one. Read
 * off the surrounding lines rather than the parse tree, because half-written
 * buffers do not parse and this is exactly when completion is wanted.
 */
export function assertKindAt(text: string, pos: Pos): string | undefined {
  const path = contextPathAt(text, pos.line);
  // `[[assert]]` and `[[supply]]` discriminate on `kind`, `[[peripheral]]` on
  // `type`. Nothing else in the format has a discriminant.
  const field = path[0] === "peripheral" ? "type" : "kind";
  if (!["assert", "supply", "peripheral"].includes(path[0] ?? "")) return undefined;
  const lines = text.split(/\r?\n/);
  const pattern = new RegExp(`^\\s*${field}\\s*=\\s*["']([^"']*)["']`);
  const scan = (from: number, step: number): string | undefined => {
    for (let i = from; i >= 0 && i < lines.length; i += step) {
      const l = lines[i] ?? "";
      if (/^\s*\[/.test(l) && i !== pos.line) return undefined;
      const m = pattern.exec(l);
      if (m) return m[1] === "boot-coverage" ? "boot_coverage" : m[1];
    }
    return undefined;
  };
  return scan(pos.line - 1, -1) ?? scan(pos.line + 1, 1);
}

/**
 * Order the table list by how often a spec contains each one, then depth. Purely
 * alphabetical put `[[decoupling.override]]` and `[[peripheral.event]]` above
 * `[[supply]]`, which is backwards: nested blocks only make sense once their
 * parent exists.
 */
const TABLE_ORDER = [
  "assert",
  "supply",
  "peripheral",
  "net_drive",
  "scenario",
  "ac",
  "fuzz",
  "override",
  "tolerance",
  "ensemble",
  "sensor",
  "profile",
  "decoupling",
];

/** Every table and array-of-tables a spec can contain, two levels deep. */
export function tableSuggestions(schema: SpecSchema, snippets = true): Suggestion[] {
  const out: Suggestion[] = [];
  const add = (path: string[], p: PropertyInfo) => {
    const dotted = [...path, p.name].join(".");
    const label = p.isArrayOfTables ? `[[${dotted}]]` : `[${dotted}]`;
    // The cursor sits after the opening bracket(s) the user already typed, so
    // the insert text is the name plus its closer, then the table's REQUIRED
    // keys: choosing `[[supply]]` should leave a block you fill in, not a header
    // you now have to remember three field names for.
    // The whole header, brackets included: the cursor context's span covers the
    // brackets the user typed, so an arity mismatch (`[[` then `[ac]`) is
    // corrected rather than left as unparseable `[[ac]`.
    const scaffold = snippets ? requiredScaffold(schema, [...path, p.name]) : "";
    out.push({
      label,
      insertText: label + scaffold,
      snippet: snippets && scaffold !== "",
      kind: "table",
      detail: p.isArrayOfTables ? "array of tables (repeatable)" : "table",
      documentation: p.description,
      preferred: p.required || dotted === "assert",
      // Nested tables (`[[peripheral.event]]`) sit below every top-level one:
      // they are only writable once their parent block exists.
      rank: path.length * 100 + (TABLE_ORDER.indexOf(p.name) + 1 || 99),
    });
  };
  for (const p of schema.tablePathsUnder([])) {
    add([], p);
    for (const child of schema.tablePathsUnder([p.name])) add([p.name], child);
  }
  return out;
}

export function keySuggestions(
  schema: SpecSchema,
  schemaPath: string[],
  present: string[],
  opts: { snippets?: boolean; kind?: string } = {}
): Suggestion[] {
  const snippets = opts.snippets ?? true;
  // Inside a block whose discriminant is known, the fields that kind reads come
  // first, in the order a person fills them in, and the rest sink.
  const scope = relevantFields(schemaPath, opts.kind);
  return schema
    .propertiesAt(schemaPath)
    .filter((p) => !p.isTable && !p.isArrayOfTables)
    .filter((p) => !present.includes(p.name))
    .map((p) => {
      const position = scope ? scope.relevant.indexOf(p.name) : -1;
      const always = !scope || scope.always.includes(p.name);
      const forThisKind = always || position >= 0;
      return {
        label: p.name,
        insertText: snippets
          ? `${p.name} = ${snippetValue(p)}`
          : `${p.name} = ${placeholder(p)}`,
        snippet: snippets,
        kind: "key" as SuggestionKind,
        detail: forThisKind
          ? detailFor(p)
          : `${detailFor(p)} (not read by a ${scope!.label})`,
        documentation: p.description,
        preferred: p.required || position >= 0,
        // The relevant fields in their authored order, then the required ones
        // the schema knows about, then everything else.
        rank: position >= 0 ? position : p.required ? scope ? 100 : 0 : 0,
        deprioritised: !forThisKind,
      };
    });
}

export function valueSuggestions(
  schema: SpecSchema,
  ctx: CursorContext,
  opts: SuggestOptions = {}
): Suggestion[] {
  if (!ctx.key) return [];
  const nets = opts.nets ?? [];
  const prop = schema.property(ctx.schemaPath, ctx.key);
  const quote = (s: string) => (ctx.inString ? s : `"${s}"`);

  // The board path is the one field whose value lives on disk, and getting it
  // wrong is the commonest reason a spec does not run.
  if (ctx.key === "board" && (opts.boards ?? []).length > 0) {
    return opts.boards!.map((b) => ({
      label: b,
      insertText: quote(b),
      kind: "value" as SuggestionKind,
      detail: "board file in this workspace",
      documentation: "Resolved relative to this spec file's own directory.",
    }));
  }

  if (prop?.enumValues.length) {
    return prop.enumValues
      // `boot-coverage` is accepted forever but the canonical spelling is the
      // snake_case one; offering both would teach the legacy form.
      .filter((v) => v !== "boot-coverage")
      .map((v) => ({
        label: v,
        insertText: quote(v),
        kind: "value" as SuggestionKind,
        detail: `${ctx.key} value`,
        documentation: prop.enumDocs[v] ?? prop.description,
      }));
  }
  // Cross-references inside the document. The lint layer already computes these
  // sets in order to REJECT a bad one (`spec/assert-unknown-scenario`,
  // `spec/assert-unknown-id`); refusing to offer the same set would be a strange
  // place to stop.
  const refs = documentReferences(opts.text ?? "", ctx.key);
  if (refs.length > 0) {
    return refs.map((r) => ({
      label: r.value,
      insertText: quote(r.value),
      kind: "value" as SuggestionKind,
      detail: r.detail,
      documentation: r.documentation,
    }));
  }
  if (NET_KEYS.includes(ctx.key) && nets.length > 0) {
    return nets.map((n) => ({
      label: n,
      insertText: quote(n),
      kind: "value" as SuggestionKind,
      detail: "board net",
      documentation: "A net on the board this spec points at.",
    }));
  }
  if (prop && typeIncludes(prop.node, "boolean")) {
    return ["true", "false"].map((v) => ({
      label: v,
      insertText: v,
      kind: "value" as SuggestionKind,
      detail: "boolean",
      documentation: prop.description,
    }));
  }
  return [];
}

// ── hovers ───────────────────────────────────────────────────────────────────

export interface HoverInfo {
  markdown: string;
  span: Span;
}

export function hoverAt(text: string, pos: Pos, schema: SpecSchema): HoverInfo | undefined {
  if (insideMultilineString(text, pos)) return undefined;
  const lines = text.split(/\r?\n/);
  const line = lines[pos.line] ?? "";

  // A table header: describe the table.
  const header = /^\s*(\[\[?)\s*([A-Za-z0-9_."'.\-]+)\s*(\]\]?)/.exec(line);
  if (header) {
    const path = header[2].split(".").map((s) => s.replace(/^["']|["']$/g, ""));
    const parent = path.slice(0, -1);
    const prop = schema.property(parent, path[path.length - 1]);
    if (!prop) return undefined;
    const col = line.indexOf(header[2]);
    return {
      markdown: [
        `**${header[1]}${header[2]}${header[3]}** (${prop.typeLabel})`,
        "",
        prop.description,
      ].join("\n"),
      span: {
        start: { line: pos.line, col },
        end: { line: pos.line, col: col + header[2].length },
      },
    };
  }

  const kv = /^(\s*)([A-Za-z0-9_."'\-]+)(\s*=\s*)(.*)$/.exec(line);
  if (!kv) return undefined;
  const keyStart = kv[1].length;
  const keyEnd = keyStart + kv[2].length;
  const key = kv[2].replace(/^["']|["']$/g, "");
  const schemaPath = contextPathAt(text, pos.line - 1 >= 0 ? pos.line - 1 : 0);
  const prop = schema.property(schemaPath, key);
  if (!prop) return undefined;

  // Hovering the VALUE of an enum key: explain that value specifically.
  const valueStart = keyEnd + kv[3].length;
  if (pos.col >= valueStart) {
    const literal = /^"([^"]*)"/.exec(kv[4])?.[1];
    if (literal && prop.enumValues.includes(literal)) {
      const note =
        literal === "boot-coverage"
          ? "\n\nAccepted legacy spelling. The canonical kind is `boot_coverage`; " +
            "hauksbee-ci folds this onto it, and will keep doing so."
          : "";
      return {
        markdown: `\`${key} = "${literal}"\`${note}\n\n${prop.description}`,
        span: {
          start: { line: pos.line, col: valueStart },
          end: { line: pos.line, col: valueStart + literal.length + 2 },
        },
      };
    }
    if (pos.col > valueStart && !literal) return undefined;
  }

  const bits = [`**${key}** (${detailFor(prop)})`];
  if (prop.enumValues.length) bits.push("", `one of: ${prop.enumValues.map((v) => `\`${v}\``).join(" | ")}`);
  if (prop.description) bits.push("", prop.description);
  return {
    markdown: bits.join("\n"),
    span: { start: { line: pos.line, col: keyStart }, end: { line: pos.line, col: keyEnd } },
  };
}

// ── helpers ──────────────────────────────────────────────────────────────────

function detailFor(p: PropertyInfo): string {
  const bits = [p.typeLabel];
  const bounds = boundsNote(p.node);
  if (bounds) bits.push(bounds);
  if (p.required) bits.push("required");
  else if (p.node.default !== undefined && p.node.default !== null)
    bits.push(`default ${JSON.stringify(p.node.default)}`);
  return bits.join(", ");
}

/**
 * The value half of a key completion, as a snippet: the cursor lands on it, and
 * a closed vocabulary becomes a CHOICE so the next keystroke picks from the list
 * instead of spelling a token from memory.
 */
function snippetValue(p: PropertyInfo): string {
  const vocabulary = p.enumValues.filter((v) => v !== "boot-coverage");
  if (vocabulary.length > 0) return `"\${1|${vocabulary.join(",")}|}"`;
  if (p.typeLabel.startsWith("array")) return "[$1]";
  if (typeIncludes(p.node, "boolean")) return "${1|true,false|}";
  const t = Array.isArray(p.node.type) ? p.node.type[0] : p.node.type;
  if (t === "string") return '"$1"';
  // A seed that satisfies the field's bounds. `fstart = 0` would put an error on
  // the `[ac]` block the instant it was scaffolded.
  const suggested =
    p.node.default !== undefined && p.node.default !== null
      ? JSON.stringify(p.node.default)
      : String(seedNumber(p.node));
  return `\${1:${suggested}}`;
}

/**
 * The required keys of a table, as snippet lines to follow its header. Choosing
 * `[[supply]]` should leave a block to fill in, not a bare header and three
 * field names to remember. Derived from the schema's own `required` list, so a
 * new required field appears here the moment the schema is regenerated.
 */
function requiredScaffold(schema: SpecSchema, schemaPath: string[]): string {
  // In the schema's `required` order, which schemars emits in Rust FIELD order:
  // `net` then `kind` for a supply, `fstart, fstop, points` for `[ac]`. That is
  // the order the docs and every example use, and it reads better than the
  // alphabetical order `properties` is sorted into.
  const byName = new Map(schema.propertiesAt(schemaPath).map((p) => [p.name, p]));
  const required = (schema.nodeAt(schemaPath)?.required ?? [])
    .map((name) => byName.get(name))
    .filter((p): p is PropertyInfo => !!p && !p.isTable && !p.isArrayOfTables);
  if (required.length === 0) return "";
  // One tab stop per field, numbered in order, so tabbing walks the block.
  return required
    .map((p, i) => `\n${p.name} = ${snippetValue(p).replace(/\$\{1/g, `\${${i + 1}`).replace(/\$1/g, `$${i + 1}`)}`)
    .join("");
}

function placeholder(p: PropertyInfo): string {
  if (p.enumValues.length) return `"${p.enumValues[0]}"`;
  if (p.typeLabel.startsWith("array")) return "[]";
  if (p.node.type === "boolean") return "true";
  if (p.node.default !== undefined && p.node.default !== null)
    return JSON.stringify(p.node.default);
  const t = Array.isArray(p.node.type) ? p.node.type[0] : p.node.type;
  return t === "string" ? '""' : String(seedNumber(p.node));
}

/** Keys already written in the table the cursor is in. */
export function keysOnScreen(text: string, pos: Pos): string[] {
  const lines = text.split(/\r?\n/);
  const out: string[] = [];
  for (let i = pos.line - 1; i >= 0; i--) {
    const l = lines[i] ?? "";
    if (/^\s*\[/.test(l)) break;
    const m = /^\s*([A-Za-z0-9_."'\-]+)\s*=/.exec(l);
    if (m) out.push(m[1].replace(/^["']|["']$/g, ""));
  }
  for (let i = pos.line + 1; i < lines.length; i++) {
    const l = lines[i] ?? "";
    if (/^\s*\[/.test(l)) break;
    const m = /^\s*([A-Za-z0-9_."'\-]+)\s*=/.exec(l);
    if (m) out.push(m[1].replace(/^["']|["']$/g, ""));
  }
  return out;
}
