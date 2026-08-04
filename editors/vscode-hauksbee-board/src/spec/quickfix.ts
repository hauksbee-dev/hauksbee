// Quick fixes for spec diagnostics.
//
// Every diagnostic this extension raises already knows the answer, because the
// loader's messages name it: "did you mean 'duration_ms'?", "add `volts = 3.3`",
// "needs a `min` and/or `max`". Leaving the user to retype it by hand wastes the
// most useful thing in the message. So each fix is derived from the diagnostic's
// code plus its own text, as a pure (text, diagnostic) -> edit function with no
// `vscode` import.
//
// The rule these follow: a fix either produces exactly what the message asked
// for, or it is not offered. Nothing guesses a value the author has to check.

import { ASSERT_FIELDS_BY_KIND } from "./complete";
import type { SpecIssue } from "./lint";
import { seedNumber, SpecSchema } from "./schemaModel";
import { lookup, parseToml, type Span } from "./tomlIndex";

export interface SpecFix {
  /** Menu title, e.g. "Change to `duration_ms`". */
  title: string;
  /** The range to replace, and what to put there. */
  edit: { span: Span; newText: string };
  /** True for the one fix that is obviously right, so the editor can prefer it. */
  preferred?: boolean;
}

/** Fixes for one diagnostic, best first. */
export function fixesFor(text: string, issue: SpecIssue, schema: SpecSchema): SpecFix[] {
  switch (issue.code) {
    case "spec/unknown-key":
      return renameFix(issue, "key");
    case "spec/bad-enum":
    case "spec/bad-enum-unchecked":
      return [...renameFix(issue, "value"), ...vocabularyFixes(issue)];
    case "spec/missing-key":
      return addMissingKeyFix(text, issue, schema);
    case "spec/supply-needs-volts":
    case "spec/supply-needs-usb":
    case "spec/supply-needs-chemistry":
      return addSuggestedKeyFix(text, issue, schema);
    case "spec/assert-needs-bound":
    case "spec/assert-needs-net":
    case "spec/assert-needs-ref":
    case "spec/assert-needs-id":
    case "spec/assert-needs-supply_net":
    case "spec/assert-needs-trace":
      return addAssertKeyFixes(text, issue, schema);
    case "spec/assert-tolerance-range":
      return toleranceFix(issue);
    case "spec/non-finite":
    case "spec/out-of-range":
    case "spec/out-of-range-unchecked":
      return [];
    default:
      return [];
  }
}

// ── renaming to the suggestion the message already contains ──────────────────

/**
 * Both the schema layer and the loader spell their hints the same way, because
 * the TypeScript `didYouMean` is a port of `hauksbee_ci::error::did_you_mean`:
 * `(did you mean 'voltage'?)`.
 */
function renameFix(issue: SpecIssue, what: "key" | "value"): SpecFix[] {
  const m = /did you mean '([^']+)'\?/.exec(issue.message);
  if (!m) return [];
  const replacement = m[1];
  return [
    {
      title: `Change to \`${replacement}\``,
      edit: {
        span: issue.span,
        // A value's span includes its quotes; a key's does not.
        newText: what === "value" ? `"${replacement}"` : replacement,
      },
      preferred: true,
    },
  ];
}

/** Every token the vocabulary allows, when the message listed them. */
function vocabularyFixes(issue: SpecIssue): SpecFix[] {
  const m = /one of: (.+?)(?:\.|$)/.exec(issue.message);
  if (!m) return [];
  const suggested = /did you mean '([^']+)'\?/.exec(issue.message)?.[1];
  return m[1]
    .split(" | ")
    .map((v) => v.trim())
    // `boot-coverage` still validates, but the canonical spelling is the
    // snake_case one and a menu entry would teach the legacy form.
    .filter((v) => v !== "" && v !== suggested && v !== "boot-coverage")
    .map((v) => ({
      title: `Change to '${v}'`,
      edit: { span: issue.span, newText: `"${v}"` },
    }));
}

// ── adding a key the message says is missing ─────────────────────────────────

/** `missing required key \`board\`` -> insert `board = ""` under the header. */
function addMissingKeyFix(text: string, issue: SpecIssue, schema: SpecSchema): SpecFix[] {
  const key =
    /missing required key `([A-Za-z0-9_]+)`/.exec(issue.message)?.[1] ??
    // The repeatable-table phrasing: "this spec has no `[[assert]]` block".
    /has no `\[\[?([A-Za-z0-9_]+)\]\]?` block/.exec(issue.message)?.[1];
  return key ? insertKeyFixes(text, issue, schema, [key]) : [];
}

/** The loader names the key AND a value: ``add `chemistry = "liion"` ``. */
function addSuggestedKeyFix(text: string, issue: SpecIssue, schema: SpecSchema): SpecFix[] {
  // Prefer the loader's own worked example, which carries a sensible value.
  const example = /`([A-Za-z0-9_]+ = [^`]+)`/.exec(issue.message);
  if (example) {
    return insertLineFixes(text, issue, `Add \`${example[1]}\``, example[1]);
  }
  const named = /needs an explicit `([A-Za-z0-9_]+)`/.exec(issue.message);
  return named ? insertKeyFixes(text, issue, schema, [named[1]]) : [];
}

/**
 * `needs a \`min\` and/or \`max\``, `needs \`freq_hz\` or \`min_toggles\``: one
 * fix per key the message backticks, filtered to the ones this assertion kind
 * actually reads so a `voltage` assertion is never offered `amps`.
 */
function addAssertKeyFixes(text: string, issue: SpecIssue, schema: SpecSchema): SpecFix[] {
  const kind = /^([a-z_]+(?:-[a-z]+)?) assertion|^(?:voltage|toggle|rail_window|ac_gain|phase_margin|peripheral|boot_coverage|model_coverage) on /.exec(
    issue.message
  );
  const named = [...issue.message.matchAll(/`([A-Za-z0-9_]+)`/g)].map((m) => m[1]);
  const relevant = kind ? ASSERT_FIELDS_BY_KIND[kind[1] ?? ""] : undefined;
  const keys = named.filter((k) => !relevant || relevant.includes(k));
  return insertKeyFixes(text, issue, schema, keys.length > 0 ? keys : named);
}

/** `tolerance is a fraction (0.25 = +-25%), got 25; did you mean 0.25?` */
function toleranceFix(issue: SpecIssue): SpecFix[] {
  const m = /did you mean ([0-9.eE+-]+)\?/.exec(issue.message);
  if (!m) return [];
  return [
    {
      title: `Change to ${m[1]}`,
      edit: { span: issue.span, newText: m[1] },
      preferred: true,
    },
  ];
}

// ── placing an inserted line ─────────────────────────────────────────────────

function insertKeyFixes(
  text: string,
  issue: SpecIssue,
  schema: SpecSchema,
  keys: string[]
): SpecFix[] {
  const from = tablePathAt(text, issue.span.start.line);
  return keys.flatMap((key) => {
    // A missing ROOT key is reported at line 0, which may itself be a table
    // header (`[[assert]]` on the first line), so the line's table is the wrong
    // place to look. Walk outwards until the key is found.
    const found = resolveIn(schema, from, key);
    if (!found) return [];
    const { path, prop } = found;
    // A required key that is a repeatable TABLE is not written `assert = []`;
    // it is written `[[assert]]` with a body. Inserting the array form would
    // trade one error for another and teach syntax nobody writes.
    if (prop.isArrayOfTables || prop.isTable) {
      const header = prop.isArrayOfTables ? `[[${key}]]` : `[${key}]`;
      const body = requiredLines(schema, [...path, key]);
      return [
        {
          title: `Add ${header}`,
          edit: appendAtEnd(text, [header, ...body].join("\n")),
          preferred: keys.length === 1,
        },
      ];
    }
    const line = `${key} = ${suggestedValue(prop)}`;
    // A key that belongs to an OUTER table has to be written before the first
    // header, not inside the block the diagnostic happens to sit in.
    const fixes =
      path.length < from.length
        ? [
            {
              title: `Add \`${line}\``,
              edit: insertBeforeFirstHeader(text, line),
              preferred: keys.length === 1,
            },
          ]
        : insertLineFixes(text, issue, `Add \`${line}\``, line, keys.length === 1);
    return fixes;
  });
}

/** The key, looked up in `path` and then in each enclosing table up to the root. */
function resolveIn(
  schema: SpecSchema,
  path: string[],
  key: string
): { path: string[]; prop: NonNullable<ReturnType<SpecSchema["property"]>> } | undefined {
  for (let depth = path.length; depth >= 0; depth--) {
    const at = path.slice(0, depth);
    const prop = schema.property(at, key);
    if (prop) return { path: at, prop };
  }
  return undefined;
}

/** The required keys of a table, as lines, for a scaffolded block. */
function requiredLines(schema: SpecSchema, schemaPath: string[]): string[] {
  const byName = new Map(schema.propertiesAt(schemaPath).map((p) => [p.name, p]));
  return (schema.nodeAt(schemaPath)?.required ?? []).flatMap((name) => {
    const prop = byName.get(name);
    if (!prop || prop.isTable || prop.isArrayOfTables) return [];
    const seed = SAFE_SEED[`${schemaPath.join(".")}.${name}`] ?? suggestedValue(prop);
    return [`${name} = ${seed}`];
  });
}

/** An edit appending a block at the end of the document. */
function appendAtEnd(text: string, block: string): SpecFix["edit"] {
  const lines = text.split(/\r?\n/);
  const lastLine = lines.length - 1;
  const end = { line: lastLine, col: (lines[lastLine] ?? "").length };
  const lead = (lines[lastLine] ?? "").trim() === "" ? "" : "\n";
  return { span: { start: end, end }, newText: `${lead}\n${block}\n` };
}

/** An edit inserting a line above the document's first table header. */
function insertBeforeFirstHeader(text: string, line: string): SpecFix["edit"] {
  const lines = text.split(/\r?\n/);
  let at = lines.findIndex((l) => /^\s*\[/.test(l));
  if (at < 0) at = lines.length;
  // Just past the last key before that header, so the root block stays together.
  let insert = 0;
  for (let i = 0; i < at; i++) if (/^\s*[A-Za-z0-9_."']+\s*=/.test(lines[i] ?? "")) insert = i + 1;
  const pos = { line: insert, col: 0 };
  return { span: { start: pos, end: pos }, newText: `${line}\n` };
}

/**
 * Insert a line into the table the diagnostic sits in, after its last existing
 * key so the block stays together.
 */
function insertLineFixes(
  text: string,
  issue: SpecIssue,
  title: string,
  line: string,
  preferred = false
): SpecFix[] {
  const lines = text.split(/\r?\n/);
  const at = insertionLine(lines, issue.span.start.line);
  const indent = /^(\s*)/.exec(lines[Math.max(at - 1, 0)] ?? "")?.[1] ?? "";
  // Past the last line (the block being fixed is the last thing in the file, with
  // no trailing newline): append to the end of that line instead, since there is
  // no line `at` for an editor to place the edit on.
  if (at >= lines.length) {
    const lastLine = lines.length - 1;
    const end = { line: lastLine, col: (lines[lastLine] ?? "").length };
    return [
      { title, edit: { span: { start: end, end }, newText: `\n${indent}${line}` }, preferred },
    ];
  }
  const pos = { line: at, col: 0 };
  return [
    {
      title,
      edit: { span: { start: pos, end: pos }, newText: `${indent}${line}\n` },
      preferred,
    },
  ];
}

/**
 * The line to insert at: just past the last `key = value` of the block the
 * diagnostic is in, which is where a human would type it.
 */
function insertionLine(lines: string[], from: number): number {
  let last = from;
  for (let i = from; i < lines.length; i++) {
    const l = lines[i] ?? "";
    if (i > from && /^\s*\[/.test(l)) break;
    if (/^\s*[A-Za-z0-9_."']+\s*=/.test(l) || /^\s*\[/.test(l)) last = i;
  }
  return last + 1;
}

/** The table path at a line, by scanning upward for its header. */
function tablePathAt(text: string, line: number): string[] {
  const doc = parseToml(text);
  if (doc.errors.length === 0) return lookup(doc).contextAt(line).schemaPath;
  const lines = text.split(/\r?\n/);
  for (let i = Math.min(line, lines.length - 1); i >= 0; i--) {
    const m = /^\s*\[\[?\s*([A-Za-z0-9_.]+)\s*\]\]?/.exec(lines[i] ?? "");
    if (m) return m[1].split(".");
  }
  return [];
}

/** A value to seed an inserted key with: never a made-up number. */
function suggestedValue(prop: ReturnType<SpecSchema["property"]> & object): string {
  if (prop.enumValues.length > 0) return `"${prop.enumValues[0]}"`;
  if (prop.typeLabel.startsWith("array")) return "[]";
  if (prop.node.type === "boolean" || prop.typeLabel.includes("boolean")) return "true";
  const t = Array.isArray(prop.node.type) ? prop.node.type[0] : prop.node.type;
  if (t === "string") return '""';
  if (prop.node.default !== undefined && prop.node.default !== null) {
    return JSON.stringify(prop.node.default);
  }
  // A value inside the field's bounds: an inserted `points = 0` would be an
  // error the moment it landed.
  return String(seedNumber(prop.node));
}

/**
 * The value to seed a scaffolded block's discriminant with: the one that leaves
 * the block IMMEDIATELY valid. `kind = "voltage"` would need a `net` and a bound
 * too, so adding an assertion would trade one error for three; `no_faults` needs
 * nothing and means something ("no stress faults raised").
 */
const SAFE_SEED: Record<string, string> = { "assert.kind": '"no_faults"' };
