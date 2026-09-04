// `hauksbee-ci` stderr -> positioned diagnostics.
//
// The loader's messages are the best documentation in the project: they name the
// field, the net, the value, and usually the fix ("add `chemistry = \"liion\"`
// (or alkaline | nimh | lifepo4), it sets the pack's voltage curve"). What they
// do NOT carry is a source location, except for TOML parse errors. So this
// module keeps the loader's words verbatim and reattaches a position by reading
// the identifiers back out of the message and finding them in the parsed
// document.
//
// Shapes handled, captured from the real binary:
//
//   hauksbee-ci: ci/x.toml: could not parse spec ci/x.toml: TOML parse error at line 2, column 1
//     |
//   2 | duraton_ms = 5
//     | ^^^^^^^^^^
//   unknown field `duraton_ms`, expected one of `name`, `board`, ...
//
//   hauksbee-ci: ci/x.toml: invalid spec: supply on '+5V': `bench` needs an explicit `volts`; ...
//   hauksbee-ci: ci/x.toml: no board file at 'nope.kicad_pcb' (resolved from the spec's `board` key). ...
//   hauksbee-ci: ci/x.toml: spec references net(s) not found on the board:
//     'VCCC' (in assert); did you mean: ADC0?

import type { Sev } from "../mapping";
import type { SpecIssue } from "./lint";
import { normaliseKind } from "./lint";
import {
  lookup,
  pathKey,
  type InstancePath,
  type Span,
  type TomlDoc,
  type TomlTable,
  type TomlValue,
} from "./tomlIndex";

const FILE_LEVEL: Span = { start: { line: 0, col: 0 }, end: { line: 0, col: 0 } };

/**
 * Map one `hauksbee-ci` failure to diagnostics on the spec.
 *
 * @param stderr the binary's stderr, verbatim
 * @param doc the parsed spec (for locating identifiers named in the message);
 *            pass a doc with no entries when the buffer could not be parsed
 */
export function mapLoaderStderr(stderr: string, doc: TomlDoc): SpecIssue[] {
  const text = stderr.trim();
  if (text === "") return [];
  const body = stripPrefix(text);

  const parse = tomlParseError(body);
  if (parse) return [parse];

  const nets = unknownNets(body, doc);
  if (nets.length > 0) return nets;

  if (/^no board file at /.test(body)) {
    return [
      {
        span: locate(doc, ["board"]) ?? FILE_LEVEL,
        // Not an error in the editor: a spec can legitimately be edited in a
        // workspace where the board is not checked out yet, and the loader's
        // own exit-2 in CI is the gate that matters.
        severity: "info",
        message: body,
        code: "spec/board-missing",
      },
    ];
  }

  const invalid = /^invalid spec: ([\s\S]*)$/.exec(body);
  const message = invalid ? invalid[1] : body;

  // Some `SpecError::Invalid` messages are not about the spec at all: they are
  // about the machine. A build without the `avr` / `qemu` / `renode` feature, or
  // a missing simulator, produces the same exit 2, and no edit to the file will
  // change it. Reporting those as spec errors would blame the author for the
  // toolchain, and would also suppress nothing useful, since the file is fine.
  if (isEnvironment(message)) {
    return [
      {
        span: FILE_LEVEL,
        severity: "info",
        message: `${message}\n\nThis is about the hauksbee build or the simulators installed, not about this spec.`,
        code: "spec/environment",
      },
    ];
  }

  return [
    {
      span: locateFromMessage(message, doc),
      severity: "error",
      message,
      code: "spec/loader",
    },
  ];
}

/** A loader failure that no edit to the spec can fix. */
function isEnvironment(message: string): boolean {
  return (
    /was compiled without the `[a-z]+` feature/.test(message) ||
    /^building engine: /.test(message) ||
    /is not installed|could not be found on PATH|install it with/.test(message)
  );
}

/** Drop the `hauksbee-ci: <spec>: ` prefix the CLI puts on every failure. */
function stripPrefix(text: string): string {
  const m = /^hauksbee-ci: [^\n]*?: ([\s\S]*)$/.exec(text);
  return m ? m[1] : text;
}

function tomlParseError(body: string): SpecIssue | undefined {
  const m = /TOML parse error at line (\d+), column (\d+)/.exec(body);
  if (!m) return undefined;
  const line = Math.max(Number(m[1]) - 1, 0);
  const col = Math.max(Number(m[2]) - 1, 0);
  // The caret run under the offending token gives the span width.
  const caret = /\n\s*\|\s*(\^+)/.exec(body);
  const width = caret ? caret[1].length : 1;
  // Everything after the caret line is the actual reason ("unknown field ...").
  const after = body.split(/\n\s*\|\s*\^+\n?/)[1];
  const reason = (after ?? body.slice(m.index)).trim();
  return {
    span: { start: { line, col }, end: { line, col: col + width } },
    severity: "error",
    message: reason === "" ? body : reason,
    code: "spec/toml",
  };
}

function unknownNets(body: string, doc: TomlDoc): SpecIssue[] {
  if (!/^spec references net\(s\) not found on the board:/.test(body)) return [];
  const out: SpecIssue[] = [];
  for (const line of body.split("\n").slice(1)) {
    const m = /^\s*'(.*?)' \(in (\w+)\)(?:; did you mean: (.*?)\?)?\s*$/.exec(line);
    if (!m) continue;
    const [, net, ctx, suggestions] = m;
    out.push({
      span: locateNet(doc, net) ?? FILE_LEVEL,
      severity: "error",
      message:
        `net '${net}' (referenced in ${ctx}) does not exist on the board` +
        (suggestions ? `; did you mean: ${suggestions}?` : ""),
      code: "spec/unknown-net",
    });
  }
  return out;
}

// ── locating an identifier named in a message ────────────────────────────────

function locateFromMessage(message: string, doc: TomlDoc): Span {
  // Backticked identifiers in the message, most specific first: the loader
  // writes "`bench` needs an explicit `volts`", so the row's own `kind` line is
  // a better home for the squiggle than its `net` line.
  const named = [...message.matchAll(/`([A-Za-z0-9_]+)`/g)].map((m) => m[1]);

  const kindTable = (table: string, idField: string, id: string): Span | undefined => {
    const rows = arrayOf(doc.root[table]);
    const idx = rows.findIndex((r) => r[idField] === id);
    if (idx < 0) return undefined;
    for (const field of [...named, "kind", "type", idField]) {
      if (rows[idx][field] === undefined) continue;
      const at = locate(doc, [table, idx, field]);
      if (at) return at;
    }
    return locate(doc, [table, idx]);
  };

  let m: RegExpExecArray | null;

  if ((m = /^supply on '(.+?)':/.exec(message))) {
    const at = kindTable("supply", "net", m[1]);
    if (at) return at;
  }
  if ((m = /^peripheral '(.+?)'/.exec(message))) {
    const at = kindTable("peripheral", "id", m[1]);
    if (at) return at;
  }
  if ((m = /^sensor '(.+?)':/.exec(message))) {
    const at = kindTable("sensor", "id", m[1]);
    if (at) return at;
  }
  if ((m = /^\[\[tolerance\]\] on '(.+?)':/.exec(message))) {
    const at = kindTable("tolerance", "ref", m[1]);
    if (at) return at;
  }
  if ((m = /^override on '(.+?)':/.exec(message))) {
    const at = kindTable("override", "ref", m[1]);
    if (at) return at;
  }
  if ((m = /^unknown assertion kind '(.+?)'/.exec(message))) {
    const rows = arrayOf(doc.root.assert);
    const idx = rows.findIndex((r) => r.kind === m![1]);
    if (idx >= 0) {
      const at = locate(doc, ["assert", idx, "kind"]);
      if (at) return at;
    }
  }
  // "<kind> assertion ..." / "<kind> assertion on '<net>' ..." / "a phase_margin / ac_gain assertion ..."
  if ((m = /(?:^|\b)([a-z_]+(?:-[a-z]+)?) assertion\b/.exec(message))) {
    const kind = normaliseKind(m[1]);
    const net = /'([^']+)'/.exec(message.slice(m.index))?.[1];
    const rows = arrayOf(doc.root.assert);
    let idx = rows.findIndex(
      (r) => normaliseKind(asString(r.kind)) === kind && (!net || matchesTarget(r, net))
    );
    if (idx < 0) idx = rows.findIndex((r) => normaliseKind(asString(r.kind)) === kind);
    if (idx >= 0) {
      const at = locate(doc, ["assert", idx, "kind"]) ?? locate(doc, ["assert", idx]);
      if (at) return at;
    }
  }
  for (const table of ["ac", "ensemble", "fuzz"]) {
    if (message.startsWith(`[${table}]`)) {
      const at = locate(doc, [table]);
      if (at) return at;
    }
  }
  for (const field of ["duration_ms", "frame_ms", "ambient_c"]) {
    if (message.startsWith(field)) {
      const at = locate(doc, [field]);
      if (at) return at;
    }
  }
  return FILE_LEVEL;
}

function locateNet(doc: TomlDoc, net: string): Span | undefined {
  // Any entry whose value is exactly this net name; net-valued keys only, so a
  // `name = "VCC"` label cannot steal the diagnostic.
  const netKeys = new Set([
    "net",
    "to",
    "a",
    "b",
    "wiper",
    "net_a",
    "net_b",
    "cs_net",
    "supply_net",
  ]);
  for (const e of doc.entries) {
    if (netKeys.has(e.key) && e.value === net) return e.valueSpan;
    if (Array.isArray(e.value) && (e.key === "nets" || e.key === "suppress_rail")) {
      if (e.value.includes(net)) return e.valueSpan;
    }
  }
  return undefined;
}

/**
 * The span for a path, or undefined when the document does not actually have
 * one there. Returning undefined is what lets `locateFromMessage` fall through
 * to its next candidate, so this must NOT substitute a file-level span:
 * `tableSpan` does that internally, hence the explicit header lookup.
 */
function locate(doc: TomlDoc, path: InstancePath): Span | undefined {
  const lk = lookup(doc);
  const direct = lk.valueSpan(path) ?? lk.keySpan(path);
  if (direct) return direct;
  const key = pathKey(path);
  return doc.headers.find((h) => pathKey(h.instancePath) === key)?.span;
}

function arrayOf(v: TomlValue | undefined): TomlTable[] {
  if (!Array.isArray(v)) return [];
  return v.filter(
    (x): x is TomlTable => typeof x === "object" && x !== null && !Array.isArray(x)
  );
}

function asString(v: TomlValue | undefined): string | undefined {
  return typeof v === "string" ? v : undefined;
}

function matchesTarget(row: TomlTable, target: string): boolean {
  return [row.net, row.supply_net, row.ref, row.id, row.name, row.trace].some(
    (v) => v === target
  );
}

/** Severity of a loader diagnostic, exported for the status-bar summary. */
export function worstSeverity(issues: SpecIssue[]): Sev | undefined {
  if (issues.some((i) => i.severity === "error")) return "error";
  if (issues.some((i) => i.severity === "warning")) return "warning";
  if (issues.length > 0) return "info";
  return undefined;
}
