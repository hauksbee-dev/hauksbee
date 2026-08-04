// A position-aware TOML reader for hauksbee-ci spec files.
//
// Why not a TOML library: every diagnostic, completion and hover in this
// extension needs to know WHERE a key or value sits in the buffer, and no
// off-the-shelf TOML parser exposes per-key spans. Parsing and indexing in one
// pass is smaller than a library plus a second position-recovery scan, and it
// keeps the extension dependency-free (the .vsix ships no node_modules).
//
// Scope: the TOML 1.0 surface a spec actually uses, plus the illegality checks
// that stop a file reading as clean when `toml-rs` cannot parse it. Comments,
// tables, arrays of tables, dotted keys, quoted keys, the four string forms,
// integers (dec/hex/oct/bin, underscores), floats (exponent, inf, nan),
// booleans, arrays, inline tables; duplicate keys and tables, leading zeros,
// misplaced underscores, redefinitions, out-of-range escapes and date-times all
// rejected. Grammar coverage is deliberate rather than complete: a construct no
// spec can contain is not worth the code, and the loader layer catches anything
// this misses on save with toml-rs's own message.
//
// Anything this reader cannot parse becomes a TomlParseError and the caller
// skips semantic linting for that buffer rather than guessing. Fail quiet, not
// fail wrong: a false diagnostic on a valid spec is worse than no diagnostic.

/** 0-based line, 0-based UTF-16 column. */
export interface Pos {
  line: number;
  col: number;
}

export interface Span {
  start: Pos;
  end: Pos;
}

export type TomlValue = string | number | boolean | TomlValue[] | TomlTable;
export interface TomlTable {
  [key: string]: TomlValue;
}

/** A path through the document. Numbers are array-of-table indices. */
export type InstancePath = (string | number)[];

export interface TomlEntry {
  /** The key's own name (last segment of a dotted key). */
  key: string;
  /** Full path to the value, including array-of-table indices. */
  instancePath: InstancePath;
  /** Path with indices stripped: the path used to walk the JSON Schema. */
  schemaPath: string[];
  keySpan: Span;
  valueSpan: Span;
  value: TomlValue;
}

export interface TomlHeader {
  /** `[[supply]]` -> instancePath ["supply", 0]. */
  instancePath: InstancePath;
  schemaPath: string[];
  arrayOfTables: boolean;
  /** The whole `[table]` / `[[table]]` header, brackets included. */
  span: Span;
}

export interface TomlParseError {
  span: Span;
  message: string;
}

export interface TomlDoc {
  root: TomlTable;
  entries: TomlEntry[];
  headers: TomlHeader[];
  errors: TomlParseError[];
  /**
   * Instance paths (as `pathKey`) whose literal was written as a FLOAT. The
   * value tree cannot tell `4` from `4.0`, and serde can: a float in a `u32`
   * field is rejected. Recorded here rather than boxing every number.
   */
  floatLiterals: Set<string>;
}

/**
 * A raw control character, which TOML forbids in strings and comments (tab is
 * the one exception). `toml-rs` rejects these as `invalid basic string`, and a
 * lone CR is the realistic case: a file with mixed line endings.
 */
function isControl(c: string): boolean {
  if (c === "\t") return false;
  const code = c.charCodeAt(0);
  return code < 0x20 || code === 0x7f;
}

/** See `Reader.depth`: a spec never nests values past three levels. */
const MAX_NESTING = 64;

/** Where a value lives: its instance path, and the path used to walk the schema. */
interface Location {
  inst: InstancePath;
  schema: string[];
}

/**
 * A path flattened to a map key: `["supply", 0, "net"]` becomes
 * `supply\0net`. The separator is a NUL escape, not a dot, on purpose: a quoted
 * TOML key may contain a dot (`"a.b" = 1`), so joining on one would collide with
 * the genuinely different dotted path `a.b`. A NUL cannot appear in any key.
 */
export function pathKey(p: InstancePath): string {
  return p.join("\u0000");
}

export function parseToml(text: string): TomlDoc {
  return new Reader(text).parse();
}

class Bail extends Error {
  constructor(
    readonly offset: number,
    readonly reason: string
  ) {
    super(reason);
  }
}

class Reader {
  private i = 0;
  private readonly lineStarts: number[];
  private readonly root: TomlTable = {};
  private readonly entries: TomlEntry[] = [];
  private readonly headers: TomlHeader[] = [];
  private readonly errors: TomlParseError[] = [];
  private readonly floatLiterals = new Set<string>();
  /** Explicit `[table]` headers already seen, so a duplicate can be rejected. */
  private readonly definedTables = new Set<string>();
  /** Paths assigned a value by `key = …`, which a later header cannot redefine. */
  private readonly staticKeys = new Set<string>();
  /** Tables brought into being by a dotted key, which a header cannot reopen. */
  private readonly implicitTables = new Set<string>();
  /** Set by `value()` when the literal just read was a float. */
  private lastWasFloat = false;
  /**
   * Array / inline-table nesting depth. Both recurse per level, so a garbage
   * file with thousands of `[` would overflow the stack, and a RangeError is not
   * a `Bail`: it would escape the parser and take every diagnostic for the
   * buffer with it. A spec never nests past three (`pwl = [[t, v], …]`).
   */
  private depth = 0;
  /** Current table context: where `key = value` lands. */
  private ctxPath: InstancePath = [];
  private ctxSchemaPath: string[] = [];
  private ctxTable: TomlTable = this.root;

  constructor(private readonly s: string) {
    this.lineStarts = [0];
    for (let k = 0; k < s.length; k++) if (s[k] === "\n") this.lineStarts.push(k + 1);
    // A UTF-8 BOM. `toml-rs` strips it, so a spec saved by an editor that emits
    // one loads fine and must not read as a broken file here. Skipped rather
    // than removed, so every span still lines up with the buffer.
    if (s.charCodeAt(0) === 0xfeff) this.i = 1;
  }

  parse(): TomlDoc {
    try {
      this.body();
    } catch (e) {
      if (e instanceof Bail) {
        this.errors.push({ span: this.spanAt(e.offset, e.offset + 1), message: e.reason });
      } else {
        // The module's contract is that a document this reader cannot handle
        // becomes a located error, never a throw: an exception here would escape
        // into the debounce callback and leave the buffer's diagnostics frozen.
        // Every known case is a `Bail`; this is the guarantee, not a known case.
        this.errors.push({
          span: this.spanAt(this.i, this.i + 1),
          message: `could not read this TOML: ${e instanceof Error ? e.message : String(e)}`,
        });
      }
    }
    return {
      root: this.root,
      entries: this.entries,
      headers: this.headers,
      errors: this.errors,
      floatLiterals: this.floatLiterals,
    };
  }

  // ── position bookkeeping ───────────────────────────────────────────────────

  private posAt(offset: number): Pos {
    // Binary search the line whose start is the greatest <= offset.
    let lo = 0;
    let hi = this.lineStarts.length - 1;
    while (lo < hi) {
      const mid = (lo + hi + 1) >> 1;
      if (this.lineStarts[mid] <= offset) lo = mid;
      else hi = mid - 1;
    }
    return { line: lo, col: offset - this.lineStarts[lo] };
  }

  private spanAt(from: number, to: number): Span {
    return { start: this.posAt(from), end: this.posAt(Math.max(to, from)) };
  }

  // ── scanning primitives ───────────────────────────────────────────────────

  private eof(): boolean {
    return this.i >= this.s.length;
  }
  private ch(k = 0): string {
    return this.s[this.i + k] ?? "";
  }
  private starts(lit: string): boolean {
    return this.s.startsWith(lit, this.i);
  }
  private bail(reason: string): never {
    throw new Bail(this.i, reason);
  }

  /** Spaces and tabs only. */
  private hspace(): void {
    while (!this.eof() && (this.ch() === " " || this.ch() === "\t")) this.i++;
  }

  /** Whitespace, newlines and comments: the gap between statements. */
  private gap(): void {
    for (;;) {
      const c = this.ch();
      if (c === " " || c === "\t" || c === "\n" || c === "\r") {
        this.i++;
      } else if (c === "#") {
        this.comment();
      } else {
        return;
      }
    }
  }

  /**
   * Consume a comment up to (not including) the newline. TOML forbids raw
   * control characters here, and a lone CR is the realistic case: a file with
   * mixed line endings, which `toml-rs` also rejects.
   */
  private comment(): void {
    while (!this.eof() && this.ch() !== "\n") {
      const c = this.ch();
      // A CR is legal only as the first half of a CRLF line ending.
      if (isControl(c) && !(c === "\r" && this.ch(1) === "\n")) {
        this.bail("a raw control character is not allowed in a comment");
      }
      this.i++;
    }
  }

  /** Consume the rest of a statement line: optional comment, then newline. */
  private endOfLine(): void {
    this.hspace();
    if (this.ch() === "#") this.comment();
    if (this.eof()) return;
    if (this.ch() === "\r") this.i++;
    if (this.ch() === "\n") {
      this.i++;
      return;
    }
    this.bail(`unexpected ${JSON.stringify(this.ch())} after a value`);
  }

  // ── document body ─────────────────────────────────────────────────────────

  private body(): void {
    for (;;) {
      this.gap();
      if (this.eof()) return;
      if (this.ch() === "[") this.tableHeader();
      else this.keyValue();
    }
  }

  private tableHeader(): void {
    const from = this.i;
    const arrayOfTables = this.starts("[[");
    this.i += arrayOfTables ? 2 : 1;
    this.hspace();
    const parts = this.keyPath();
    this.hspace();
    const close = arrayOfTables ? "]]" : "]";
    if (!this.starts(close)) this.bail(`unterminated table header, expected '${close}'`);
    this.i += close.length;
    const span = this.spanAt(from, this.i);
    this.endOfLine();

    const names = parts.map((p) => p.name);
    // Walk/create the container, then land in the new table.
    let table = this.root;
    const inst: InstancePath = [];
    for (let k = 0; k < names.length; k++) {
      const name = names[k];
      const last = k === names.length - 1;
      inst.push(name);
      // A path already given a value by `key = …` (`assert = [{…}]`,
      // `ac = { … }`) cannot be reopened as a table. Checked here, before an
      // array-of-tables push appends an index to `inst`.
      if (last && this.staticKeys.has(pathKey(inst))) {
        this.i = from;
        this.bail(
          `\`${names.join(".")}\` is already defined as a value, so it cannot be a table here`
        );
      }
      if (last && arrayOfTables) {
        let arr = table[name];
        if (arr !== undefined && !Array.isArray(arr)) {
          this.bail(
            `\`${names.join(".")}\` is already a table, so it cannot also be an array of tables`
          );
        }
        if (!Array.isArray(arr)) {
          arr = [];
          table[name] = arr;
        }
        const list = arr as TomlValue[];
        const fresh: TomlTable = {};
        list.push(fresh);
        inst.push(list.length - 1);
        table = fresh;
      } else {
        let next = table[name];
        if (Array.isArray(next)) {
          if (last) {
            // `[supply]` where `[[supply]]` already made it an array.
            this.bail(
              `\`${names.join(".")}\` is an array of tables; write \`[[${names.join(".")}]]\` to add another`
            );
          }
          // A sub-table under the LAST element of an array of tables,
          // e.g. `[peripheral.event]` after `[[peripheral]]`.
          const list = next as TomlValue[];
          if (list.length === 0) list.push({});
          inst.push(list.length - 1);
          next = list[list.length - 1];
        }
        if (typeof next !== "object" || next === null || Array.isArray(next)) {
          next = {};
          table[name] = next as TomlTable;
        }
        table = next as TomlTable;
      }
    }
    const dotted = names.join(".");
    const key = pathKey(inst);
    // A second `[x]` for the same path is a duplicate definition, and so is a
    // header for a table a dotted key already created. `[[x]]` is repeatable by
    // definition, so only the array-vs-table conflict above applies to it.
    if (!arrayOfTables) {
      if (this.definedTables.has(key) || this.implicitTables.has(key)) {
        this.i = from;
        this.bail(`table \`${dotted}\` is defined more than once`);
      }
      this.definedTables.add(key);
    }
    this.ctxPath = inst;
    this.ctxSchemaPath = names;
    this.ctxTable = table;
    this.headers.push({ instancePath: inst.slice(), schemaPath: names, arrayOfTables, span });
  }

  private keyValue(): void {
    const stmtFrom = this.i;
    const parts = this.keyPath();
    this.hspace();
    if (this.ch() !== "=") this.bail("expected '=' after a key");
    this.i++;
    this.hspace();

    // Resolve the containing table BEFORE reading the value, so the value's own
    // members (an inline table's keys, an array's elements) can be indexed with
    // real paths as they are read. Dotted keys extend the table context for this
    // statement only.
    let table = this.ctxTable;
    const inst = this.ctxPath.slice();
    const schema = this.ctxSchemaPath.slice();
    for (let k = 0; k < parts.length - 1; k++) {
      const name = parts[k].name;
      inst.push(name);
      schema.push(name);
      let next = table[name];
      if (typeof next !== "object" || next === null || Array.isArray(next)) {
        next = {};
        table[name] = next as TomlTable;
      }
      this.implicitTables.add(pathKey(inst));
      table = next as TomlTable;
    }
    const leaf = parts[parts.length - 1];
    if (table[leaf.name] !== undefined) {
      this.i = stmtFrom;
      this.bail(`duplicate key \`${leaf.name}\`: it is already set in this table`);
    }
    const at: Location = { inst: [...inst, leaf.name], schema: [...schema, leaf.name] };

    const valueFrom = this.i;
    const value = this.value(at);
    const valueSpan = this.spanAt(valueFrom, this.i);
    this.endOfLine();

    table[leaf.name] = value;
    this.staticKeys.add(pathKey(at.inst));
    this.record(leaf.name, at, leaf.span, valueSpan, value);
  }

  /** Index one key/value, wherever it was written. */
  private record(
    key: string,
    at: Location,
    keySpan: Span,
    valueSpan: Span,
    value: TomlValue
  ): void {
    this.entries.push({
      key,
      instancePath: at.inst,
      schemaPath: at.schema,
      keySpan,
      valueSpan,
      value,
    });
    if (this.lastWasFloat) this.floatLiterals.add(pathKey(at.inst));
  }

  /** A dotted key path: `a`, `a.b`, `"a b".c`. */
  private keyPath(): { name: string; span: Span }[] {
    const out: { name: string; span: Span }[] = [];
    for (;;) {
      this.hspace();
      const from = this.i;
      let name: string;
      if (this.ch() === '"' || this.ch() === "'") {
        name = this.quotedKey();
      } else {
        let n = "";
        while (!this.eof() && /[A-Za-z0-9_-]/.test(this.ch())) {
          n += this.ch();
          this.i++;
        }
        if (n === "") this.bail("expected a key");
        name = n;
      }
      out.push({ name, span: this.spanAt(from, this.i) });
      this.hspace();
      if (this.ch() === ".") {
        this.i++;
        continue;
      }
      return out;
    }
  }

  private quotedKey(): string {
    const quote = this.ch();
    if (quote === "'") {
      this.i++;
      let out = "";
      while (!this.eof() && this.ch() !== "'") {
        out += this.ch();
        this.i++;
      }
      if (this.eof()) this.bail("unterminated literal key");
      this.i++;
      return out;
    }
    return this.basicString();
  }

  // ── values ────────────────────────────────────────────────────────────────

  private value(at?: Location): TomlValue {
    const c = this.ch();
    this.lastWasFloat = false;
    if (c === '"') return this.starts('"""') ? this.multilineBasic() : this.basicString();
    if (c === "'") return this.starts("'''") ? this.multilineLiteral() : this.literalString();
    if (c === "[") return this.array(at);
    if (c === "{") return this.inlineTable(at);
    if (this.starts("true")) {
      this.i += 4;
      return true;
    }
    if (this.starts("false")) {
      this.i += 5;
      return false;
    }
    return this.number();
  }

  private basicString(): string {
    this.i++; // opening quote
    let out = "";
    for (;;) {
      if (this.eof() || this.ch() === "\n") this.bail("unterminated string");
      const c = this.ch();
      if (isControl(c)) this.bail("a raw control character is not allowed in a string");
      if (c === '"') {
        this.i++;
        return out;
      }
      if (c === "\\") {
        out += this.escape();
        continue;
      }
      out += c;
      this.i++;
    }
  }

  private literalString(): string {
    this.i++;
    let out = "";
    for (;;) {
      if (this.eof() || this.ch() === "\n") this.bail("unterminated literal string");
      if (isControl(this.ch())) {
        this.bail("a raw control character is not allowed in a string");
      }
      if (this.ch() === "'") {
        this.i++;
        return out;
      }
      out += this.ch();
      this.i++;
    }
  }

  private multilineBasic(): string {
    this.i += 3;
    if (this.ch() === "\r") this.i++;
    if (this.ch() === "\n") this.i++; // a newline right after the opener is trimmed
    let out = "";
    for (;;) {
      if (this.eof()) this.bail('unterminated multi-line string (missing """)');
      if (this.starts('"""')) {
        // A string whose content ends in quotes closes with 4 or 5 quotes; the
        // leading extras belong to the content.
        let run = 0;
        while (this.s[this.i + run] === '"') run++;
        const extra = Math.min(Math.max(run - 3, 0), 2);
        out += '"'.repeat(extra);
        this.i += extra + 3;
        return out;
      }
      if (this.ch() === "\\") {
        // Line-ending backslash swallows the following whitespace run.
        let k = this.i + 1;
        while (k < this.s.length && (this.s[k] === " " || this.s[k] === "\t" || this.s[k] === "\r"))
          k++;
        if (this.s[k] === "\n") {
          this.i = k + 1;
          while (
            !this.eof() &&
            (this.ch() === " " || this.ch() === "\t" || this.ch() === "\n" || this.ch() === "\r")
          )
            this.i++;
          continue;
        }
        out += this.escape();
        continue;
      }
      out += this.ch();
      this.i++;
    }
  }

  private multilineLiteral(): string {
    this.i += 3;
    if (this.ch() === "\r") this.i++;
    if (this.ch() === "\n") this.i++;
    let out = "";
    for (;;) {
      if (this.eof()) this.bail("unterminated multi-line literal string (missing ''')");
      if (this.starts("'''")) {
        // Same 4-or-5-quote closing rule as `"""`: content ending in apostrophes
        // pushes the extras into the string. `[[sensor]] spec = '''…'''` is the
        // documented way to inline a nested TOML document, so this matters.
        let run = 0;
        while (this.s[this.i + run] === "'") run++;
        const extra = Math.min(Math.max(run - 3, 0), 2);
        out += "'".repeat(extra);
        this.i += extra + 3;
        return out;
      }
      out += this.ch();
      this.i++;
    }
  }

  private escape(): string {
    this.i++; // backslash
    const c = this.ch();
    this.i++;
    switch (c) {
      case "n":
        return "\n";
      case "t":
        return "\t";
      case "r":
        return "\r";
      case '"':
        return '"';
      case "\\":
        return "\\";
      case "b":
        return "\b";
      case "f":
        return "\f";
      case "u":
      case "U": {
        const width = c === "u" ? 4 : 8;
        const hex = this.s.slice(this.i, this.i + width);
        if (!/^[0-9A-Fa-f]+$/.test(hex) || hex.length !== width)
          this.bail(`bad \\${c} escape (expected ${width} hex digits)`);
        const code = parseInt(hex, 16);
        // Beyond the Unicode range `String.fromCodePoint` throws a RangeError,
        // which is not a Bail and would escape the parser entirely, taking every
        // diagnostic for the buffer with it. toml-rs rejects it; so do we.
        if (code > 0x10ffff || (code >= 0xd800 && code <= 0xdfff)) {
          this.bail(`\\${c}${hex} is not a Unicode scalar value`);
        }
        this.i += width;
        return String.fromCodePoint(code);
      }
      default:
        this.bail(`unknown escape \\${c}`);
    }
  }

  private array(at?: Location): TomlValue[] {
    this.enter();
    this.i++; // [
    const out: TomlValue[] = [];
    for (;;) {
      this.gap();
      if (this.eof()) this.bail("unterminated array (missing ']')");
      if (this.ch() === "]") {
        this.i++;
        this.depth--;
        return out;
      }
      // An element's schema path is the array's own (the schema addresses the
      // ITEM type through `items`); only the instance path carries the index.
      const element = at ? { inst: [...at.inst, out.length], schema: at.schema } : undefined;
      const elementFrom = this.i;
      const v = this.value(element);
      // Index EVERY element, scalar or not. A scalar element carries a typo'd
      // net or a bad enum; an object element is an inline `{ … }` table with no
      // `[[header]]` of its own, so without a span here a table-level
      // diagnostic about it would fall back to the top of the file.
      if (element) {
        const span = this.spanAt(elementFrom, this.i);
        if (typeof v === "object" && v !== null) this.lastWasFloat = false;
        this.record(String(out.length), element, span, span, v);
      }
      out.push(v);
      this.gap();
      if (this.ch() === ",") {
        this.i++;
        continue;
      }
      if (this.ch() === "]") {
        this.i++;
        this.depth--;
        return out;
      }
      this.bail("expected ',' or ']' in an array");
    }
  }

  private inlineTable(at?: Location): TomlTable {
    this.enter();
    this.i++; // {
    const out: TomlTable = {};
    for (;;) {
      this.hspace();
      if (this.eof()) this.bail("unterminated inline table (missing '}')");
      if (this.ch() === "}") {
        this.i++;
        this.depth--;
        return out;
      }
      const parts = this.keyPath();
      this.hspace();
      if (this.ch() !== "=") this.bail("expected '=' in an inline table");
      this.i++;
      this.hspace();
      // Members of an inline table are indexed like any other key, so a
      // diagnostic about `supply = [{ net = "V", kind = "bench" }]` lands on the
      // offending member and not at the top of the file.
      const names = parts.map((p) => p.name);
      const member: Location | undefined = at
        ? { inst: [...at.inst, ...names], schema: [...at.schema, ...names] }
        : undefined;
      const valueFrom = this.i;
      const v = this.value(member);
      const valueSpan = this.spanAt(valueFrom, this.i);
      let t = out;
      for (let k = 0; k < parts.length - 1; k++) {
        const name = parts[k].name;
        let next = t[name];
        if (typeof next !== "object" || next === null || Array.isArray(next)) {
          next = {};
          t[name] = next as TomlTable;
        }
        t = next as TomlTable;
      }
      const leaf = parts[parts.length - 1];
      t[leaf.name] = v;
      if (member) this.record(leaf.name, member, leaf.span, valueSpan, v);
      this.hspace();
      if (this.ch() === ",") {
        this.i++;
        this.hspace();
        // An inline table, unlike an array, may not have a trailing comma.
        if (this.ch() === "}") this.bail("an inline table may not end with a comma");
        continue;
      }
      if (this.ch() === "}") {
        this.i++;
        this.depth--;
        return out;
      }
      this.bail("expected ',' or '}' in an inline table");
    }
  }

  private enter(): void {
    if (++this.depth > MAX_NESTING) {
      this.bail(`values nested more than ${MAX_NESTING} deep; this is not a spec`);
    }
  }

  /** Integers, floats, `inf` / `nan`. */
  private number(): TomlValue {
    const from = this.i;
    while (!this.eof() && !/[,\]}\n\r#]/.test(this.ch())) this.i++;
    const raw = this.s.slice(from, this.i).trim();
    // Give back the trailing whitespace we swallowed so the value span is tight.
    this.i = from + this.s.slice(from, this.i).replace(/\s+$/, "").length;
    if (raw === "") {
      this.i = from;
      this.bail("expected a value after '='");
    }
    // Annotated on the variable, not just the return, so TypeScript treats a
    // call as terminating control flow.
    const fail: (reason: string) => never = (reason) => {
      this.i = from;
      this.bail(reason);
    };

    if (/^[+-]?inf$/.test(raw)) {
      this.lastWasFloat = true;
      return raw[0] === "-" ? -Infinity : Infinity;
    }
    if (/^[+-]?nan$/.test(raw)) {
      this.lastWasFloat = true;
      return NaN;
    }
    // Date-times, local dates and local times are valid TOML but no field in a
    // spec accepts one, and serde would reject it wherever it appeared. Naming
    // that is more useful than silently handing on a string.
    if (/^\d{4}-\d{2}-\d{2}([Tt ]|$)/.test(raw) || /^\d{2}:\d{2}(:\d{2})?$/.test(raw)) {
      fail(`no field in a hauksbee-ci spec takes a date or time; quote it if you meant a string`);
    }

    // An underscore must sit BETWEEN two digits; `1__00` and `0x1_` are not TOML,
    // and stripping them first would erase the evidence.
    if (/(?:^|[^0-9A-Fa-f])_|_(?:$|[^0-9A-Fa-f])/.test(raw)) {
      fail(`an underscore in a number must sit between digits (${JSON.stringify(raw)})`);
    }
    const bare = raw.replace(/_/g, "");
    if (/^0x[0-9A-Fa-f]+$/.test(bare)) return parseInt(bare.slice(2), 16);
    if (/^0o[0-7]+$/.test(bare)) return parseInt(bare.slice(2), 8);
    if (/^0b[01]+$/.test(bare)) return parseInt(bare.slice(2), 2);
    const decimal = /^[+-]?(\d+)(\.\d+)?([eE][+-]?\d+)?$/.exec(bare);
    if (decimal) {
      // TOML forbids leading zeros, and toml-rs rejects `010`. Accepting it here
      // would let a spec read as fine that the loader cannot parse.
      if (/^0\d/.test(decimal[1])) {
        fail(`leading zeros are not allowed in a TOML number (${JSON.stringify(raw)})`);
      }
      this.lastWasFloat = decimal[2] !== undefined || decimal[3] !== undefined;
      return Number(bare);
    }
    fail(`not a valid TOML value: ${JSON.stringify(raw)}`);
  }
}

// ── lookups the linters and providers need ───────────────────────────────────

export interface TomlLookup {
  doc: TomlDoc;
  /** Key span for an instance path, when the path names a `key = value`. */
  keySpan(path: InstancePath): Span | undefined;
  /** Value span for an instance path. */
  valueSpan(path: InstancePath): Span | undefined;
  /** Header span for a table's instance path, else the first line of the file. */
  tableSpan(path: InstancePath): Span;
  /** Header span for a path, or undefined when no `[table]` declares it. */
  headerSpan(path: InstancePath): Span | undefined;
  /** The innermost table header containing `line`, else the root table. */
  contextAt(line: number): { instancePath: InstancePath; schemaPath: string[] };
}

const FIRST_LINE: Span = { start: { line: 0, col: 0 }, end: { line: 0, col: 0 } };

export function lookup(doc: TomlDoc): TomlLookup {
  const keys = new Map<string, Span>();
  const values = new Map<string, Span>();
  for (const e of doc.entries) {
    keys.set(pathKey(e.instancePath), e.keySpan);
    values.set(pathKey(e.instancePath), e.valueSpan);
  }
  const tables = new Map<string, Span>();
  for (const h of doc.headers) tables.set(pathKey(h.instancePath), h.span);

  return {
    doc,
    keySpan: (p) => keys.get(pathKey(p)),
    valueSpan: (p) => values.get(pathKey(p)),
    tableSpan: (p) => (p.length === 0 ? FIRST_LINE : (tables.get(pathKey(p)) ?? FIRST_LINE)),
    headerSpan: (p) => tables.get(pathKey(p)),
    contextAt(line: number) {
      let best: TomlHeader | undefined;
      for (const h of doc.headers) {
        if (h.span.start.line <= line && (!best || h.span.start.line > best.span.start.line)) {
          best = h;
        }
      }
      return best
        ? { instancePath: best.instancePath, schemaPath: best.schemaPath }
        : { instancePath: [], schemaPath: [] };
    },
  };
}
