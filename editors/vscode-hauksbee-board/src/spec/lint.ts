// Spec linting with no binary and no network: everything `hauksbee-ci`'s
// loader would reject, checked in the editor as you type.
//
// Two layers, because they answer different questions:
//
//  1. `schemaIssues` walks the value tree against the GENERATED schema. This is
//     the structural layer serde owns in Rust: unknown keys
//     (deny_unknown_fields), missing required keys, wrong types, values outside
//     a closed enum, numeric bounds.
//  2. `crossFieldIssues` mirrors `Spec::validate` / `SupplySpec::validate` /
//     `Assertion::validate` / `PeripheralSpec::validate`: the conditional
//     rules JSON Schema cannot express, and the ones that matter most in
//     practice: a `bench` supply with no `volts`, a `voltage` assertion with no
//     bound, `min > max`, a toggle `tolerance` written as a percentage, a
//     `scenario` scope naming a scenario that does not exist.
//
// Layer 2 is a deliberate second implementation of Rust logic, so it is pinned
// by test/parity.test.ts: every bad fixture it flags is fed to the real
// `hauksbee-ci` binary, which must reject it too. If the loader's rules move,
// that test fails instead of the extension quietly lying.

import type { Sev } from "../mapping";
import { boundsNote, enumOf, SpecSchema, typeIncludes, type SchemaNode } from "./schemaModel";
import {
  lookup,
  parseToml,
  pathKey,
  type InstancePath,
  type Span,
  type TomlDoc,
  type TomlLookup,
  type TomlTable,
  type TomlValue,
} from "./tomlIndex";

export interface SpecIssue {
  span: Span;
  severity: Sev;
  message: string;
  /** Stable code, e.g. "spec/unknown-key" or "spec/assert-needs-bound". */
  code: string;
}

export interface LintResult {
  issues: SpecIssue[];
  doc: TomlDoc;
  /** False when the TOML could not be read, so semantic checks were skipped. */
  analysed: boolean;
}

export function lintSpec(text: string, schema: SpecSchema): LintResult {
  const doc = parseToml(text);
  if (doc.errors.length > 0) {
    return {
      issues: doc.errors.map((e) => ({
        span: e.span,
        severity: "error" as Sev,
        message: e.message,
        code: "spec/toml",
      })),
      doc,
      analysed: false,
    };
  }
  const lk = lookup(doc);
  const issues: SpecIssue[] = [];
  schemaIssues(doc.root, schema, lk, issues);
  crossFieldIssues(doc.root, lk, issues);
  nonFiniteNumbers(lk, issues);
  issues.sort(
    (a, b) => a.span.start.line - b.span.start.line || a.span.start.col - b.span.start.col
  );
  return { issues, doc, analysed: true };
}

// ── layer 1: the generated schema ────────────────────────────────────────────

function schemaIssues(
  root: TomlTable,
  schema: SpecSchema,
  lk: TomlLookup,
  out: SpecIssue[]
): void {
  walkTable(root, schema.nodeAt([]), [], schema, lk, out);
}

function walkTable(
  value: TomlTable,
  node: SchemaNode | undefined,
  path: InstancePath,
  schema: SpecSchema,
  lk: TomlLookup,
  out: SpecIssue[]
): void {
  if (!node?.properties) return;
  const props = node.properties;
  const names = Object.keys(props);

  for (const req of node.required ?? []) {
    if (value[req] === undefined) {
      const shape = schema.property(pathNames(path), req);
      // A repeatable table is not a "key": phrase it the way the loader does,
      // and the way the author will write it.
      const what =
        shape?.isArrayOfTables || shape?.isTable
          ? `this spec has no ${shape.isArrayOfTables ? `\`[[${req}]]\`` : `\`[${req}]\``} block` +
            (req === "assert"
              ? ": a check with no assertions always passes vacuously"
              : `${describeFor(schema, props[req])}`)
          : `missing required key \`${req}\`${describeFor(schema, props[req])}`;
      out.push({
        span: tableSpan(lk, path),
        severity: "error",
        message: what,
        code: "spec/missing-key",
      });
    }
  }

  for (const [key, v] of Object.entries(value)) {
    const child = props[key];
    if (!child) {
      if (node.additionalProperties === false) {
        out.push({
          // `[[supplies]]` is the commonest spec typo of all and has no
          // `key = value` entry, so fall back to its HEADER before the file.
          span:
            lk.keySpan([...path, key]) ??
            lk.headerSpan([...path, key]) ??
            lk.headerSpan([...path, key, 0]) ??
            tableSpan(lk, path),
          severity: "error",
          message:
            `unknown key \`${key}\`${hint(key, names)}. ` +
            `hauksbee-ci rejects unknown keys rather than ignoring them; ` +
            `valid here: ${names.join(", ")}`,
          code: "spec/unknown-key",
        });
      } else if (typeof node.additionalProperties === "object") {
        walkValue(v, node.additionalProperties, [...path, key], schema, lk, out);
      }
      continue;
    }
    walkValue(v, child, [...path, key], schema, lk, out);
  }
}

function walkValue(
  value: TomlValue,
  raw: SchemaNode,
  path: InstancePath,
  schema: SpecSchema,
  lk: TomlLookup,
  out: SpecIssue[]
): void {
  const node = schema.resolve(raw);
  if (!node) return;

  if (Array.isArray(value)) {
    if (!typeIncludes(node, "array")) {
      // serde's derived `Deserialize` implements `visit_seq`, so an array is a
      // valid positional encoding of any struct: `supply = [["V", "bench", 5.0]]`
      // really does load. Nobody writes that on purpose, so say so, but as a
      // warning: an error would be a false claim about what CI does.
      out.push({
        span: spanFor(lk, path),
        severity: "warning",
        message:
          `\`${String(path[path.length - 1])}\` is a table here, not an array. ` +
          "hauksbee-ci does accept an array as a positional encoding of the table's " +
          "fields, in declaration order, but that is almost certainly not what was meant",
        code: "spec/table-as-sequence",
      });
      return;
    }
    if (node.minItems !== undefined && value.length < node.minItems) {
      out.push({
        span: spanFor(lk, path),
        severity: "error",
        message: `needs at least ${node.minItems} entr${node.minItems === 1 ? "y" : "ies"}, got ${value.length}`,
        code: "spec/too-few",
      });
    }
    const items = node.items;
    if (items) {
      value.forEach((v, idx) => walkValue(v, items, [...path, idx], schema, lk, out));
    }
    return;
  }

  if (isTable(value)) {
    if (typeIncludes(node, "array")) {
      // `[assert]` where the format wants `[[assert]]`: a repeatable block
      // written as a single table. Name both spellings, since the reader may
      // have arrived here from a nested `[peripheral.event]` whose parent
      // `[[peripheral]]` is simply absent.
      out.push(
        typeError(
          lk,
          path,
          node,
          `repeatable: write \`[[${String(path[path.length - 1])}]]\`, and make sure the ` +
            "parent block it belongs to exists"
        )
      );
      return;
    }
    if (node.properties) {
      walkTable(value, node, path, schema, lk, out);
      return;
    }
    // A Rust `HashMap<String, T>`: any key, every value of type T. `[sensor.inputs]`
    // is the one in a spec, and its keys are sensor-defined, so they cannot be
    // enumerated here.
    if (typeIncludes(node, "object")) {
      if (typeof node.additionalProperties === "object") {
        for (const [k, v] of Object.entries(value)) {
          walkValue(v, node.additionalProperties, [...path, k], schema, lk, out);
        }
      }
      return;
    }
    out.push(typeError(lk, path, node, "a table"));
    return;
  }

  // Scalars.
  const allowed = enumOf(node);
  const want = (Array.isArray(node.type) ? node.type : node.type ? [node.type] : []).filter(
    (t) => t !== "null"
  );
  // A Rust unit enum arrives as `oneOf: [{const: "..."}]` with no `type` of its
  // own, so its values are still strings.
  if (want.length === 0 && allowed.length > 0) want.push("string");
  if (want.length > 0 && !matchesScalar(value, want)) {
    const article = /^[aeiou]/.test(want[0]) ? "an" : "a";
    out.push(typeError(lk, path, node, `${article} ${want.join(" or ")}`));
    return;
  }
  if (allowed.length > 0 && typeof value === "string" && !allowed.includes(value)) {
    const key = String(path[path.length - 1]);
    // Same enforced-versus-documented split as the numeric bounds. A real Rust
    // enum (schemars writes it as a `oneOf` of consts) is enforced by serde; a
    // string field carrying `#[schemars(extend("enum" = …))]` is only enforced
    // where `Spec::validate` re-checks the token; ENFORCED_ENUMS records those
    // checks so the editor and loader keep the same severity.
    const enforced = !!node.oneOf || ENFORCED_ENUMS.has(`${tableOf(path)}.${key}`);
    out.push({
      span: spanFor(lk, path),
      severity: enforced ? "error" : "warning",
      message:
        `'${value}' is not a valid ${key}${hint(value, allowed)}. one of: ${allowed.join(" | ")}` +
        (enforced
          ? ""
          : ". hauksbee-ci accepts the spec and rejects this once the run reaches it, " +
            "so the build fails later rather than at load"),
      code: enforced ? "spec/bad-enum" : "spec/bad-enum-unchecked",
    });
    return;
  }
  if (typeof value === "number" && Number.isFinite(value)) {
    const key = String(path[path.length - 1]);
    const table = tableOf(path);
    // A value outside the Rust integer type's own range is rejected by serde
    // during deserialization, before any validation runs, so it is always a hard
    // error however permissive `Spec::validate` is. `-1` in a `usize` field is
    // the common case.
    const native = nativeIntRange(node);
    const overMax =
      native?.max !== undefined && (native.exclusiveMax ? value > native.max : value > native.max);
    if (native && (value < native.min || overMax)) {
      const range = native.exclusiveMax
        ? `${native.min === 0 ? "zero or positive" : "an integer"} and within a 64-bit integer`
        : native.min === 0
          ? `between 0 and ${native.max}`
          : `between ${native.min} and ${native.max}`;
      out.push({
        span: spanFor(lk, path),
        severity: "error",
        message: `${key} must be ${range}, got ${value}; serde rejects it before the spec is even validated`,
        code: "spec/out-of-range",
      });
      return;
    }
    // A float literal in an integer field: serde rejects it, and the value tree
    // cannot tell 4 from 4.0, so the reader records which literals were floats.
    if (
      lk.doc.floatLiterals.has(pathKey(path)) &&
      typeIncludes(node, "integer") &&
      !typeIncludes(node, "number")
    ) {
      out.push({
        span: spanFor(lk, path),
        severity: "error",
        message: `${key} must be a whole number, got ${value}; serde rejects a float here`,
        code: "spec/bad-type",
      });
      return;
    }
    if (BOUNDS_OWNED_BY_CROSS_FIELD.has(`${table}.${key}`)) return;
    const bad =
      (node.minimum !== undefined && value < node.minimum) ||
      (node.maximum !== undefined && value > node.maximum) ||
      (node.exclusiveMinimum !== undefined && value <= node.exclusiveMinimum) ||
      (node.exclusiveMaximum !== undefined && value >= node.exclusiveMaximum);
    if (bad) {
      // The schema's bounds come from `#[schemars(...)]` attributes, and only
      // SOME of them are re-checked in Rust code. Where `Spec::validate` checks
      // it, an out-of-range value is a spec error and CI will reject it. Where
      // it does not, the bound is a documented expectation the loader currently
      // lets through, so calling it an error would flag a spec hauksbee-ci
      // accepts. It is still almost certainly a mistake, hence a warning.
      const enforced = ENFORCED_BOUNDS[table]?.includes(key) ?? false;
      out.push({
        span: spanFor(lk, path),
        severity: enforced ? "error" : "warning",
        message:
          `${key} must be ${boundsNote(node)}, got ${value}` +
          (enforced
            ? ""
            : ". hauksbee-ci does not currently reject this, so the build will not " +
              "fail on it, but nothing downstream expects a value out of that range"),
        code: enforced ? "spec/out-of-range" : "spec/out-of-range-unchecked",
      });
    }
  }
}

/**
 * The range of the Rust integer type behind a schema node, from the `format`
 * schemars emits for it. This range is enforced by serde during deserialization,
 * so a value outside it never reaches validation.
 *
 * A 64-bit type gets NO upper bound. A `u64` accepts everything a TOML integer
 * can hold, and a JavaScript double cannot even represent the boundary, so
 * claiming one would report an error on `seed = 9007199254740993`, which
 * hauksbee-ci runs perfectly happily.
 */
function nativeIntRange(
  node: SchemaNode
): { min: number; max?: number; exclusiveMax?: boolean } | undefined {
  const m = /^(u?int)(8|16|32|64)?$/.exec(node.format ?? "");
  if (!m) return undefined;
  const bits = m[2] ? Number(m[2]) : 64;
  const unsigned = m[1] === "uint";
  if (bits > 32) {
    // A TOML integer is an i64, so 2^63 and up cannot be represented whatever
    // the Rust type is. The bound is STRICT because a double cannot tell
    // i64::MAX (2^63 - 1) from 2^63, and erring on a valid `i64::MAX` would
    // break the promise; a u64-range literal like 18446744073709551615 is far
    // enough above to be caught anyway.
    return unsigned ? { min: 0, max: 2 ** 63, exclusiveMax: true } : { min: -(2 ** 63), max: 2 ** 63, exclusiveMax: true };
  }
  return unsigned
    ? { min: 0, max: 2 ** bits - 1 }
    : { min: -(2 ** (bits - 1)), max: 2 ** (bits - 1) - 1 };
}

/** The instance path's names, indices dropped: the path used to walk the schema. */
function pathNames(path: InstancePath): string[] {
  return path.filter((seg): seg is string => typeof seg === "string");
}

/** The instance path with array indices dropped, e.g. `supply.0.volts` -> `supply`. */
function tableOf(path: InstancePath): string {
  return path
    .slice(0, -1)
    .filter((seg) => typeof seg === "string")
    .join(".");
}

/**
 * Numeric bounds that `Spec::validate` (or one of the per-type `validate`
 * methods it calls) re-checks in Rust code, so an out-of-range value really
 * does fail the build. Keyed by table path, "" for the root.
 *
 * Everything else the schema carries is documentation. Keeping the two apart is
 * what stops the editor claiming CI will reject a spec it will happily run.
 */
const ENFORCED_BOUNDS: Record<string, string[]> = {
  "": ["duration_ms", "frame_ms"],
  supply: [
    "current_limit_a",
    "r_out_ohms",
    "ripple_vpp",
    "ripple_hz",
    "capacity_mah",
    "r_internal_ohms",
    "protection_trip_a",
    "protection_delay_ms",
    "protection_reset_a",
    "soc",
    "cells",
  ],
  ac: ["fstart", "fstop", "points"],
  tolerance: ["percent"],
  override: ["tolerance"],
  timing: ["min_pulse_us", "max_edge_error_us"],
  peripheral: ["address", "size"],
  scenario: ["start_ms"],
  "decoupling.override": ["esr_ohms", "esl_henries"],
  fuzz: ["seeds"],
  ensemble: ["seeds"],
  assert: ["spike_for_max_ms", "settle_within_ms"],
};

/**
 * Closed vocabularies that a `validate` method re-checks at LOAD time, so a bad
 * token fails the build immediately. Rust unit enums are not listed: serde
 * enforces those by construction, and `node.oneOf` identifies them.
 */
const ENFORCED_ENUMS = new Set([
  "supply.kind",
  "supply.usb",
  "supply.chemistry",
  "peripheral.type",
  "assert.kind",
  "ac.sweep",
  "ensemble.mode",
  "tolerance.distribution",
  "override.distribution",
  "peripheral.waveform",
]);

/**
 * Bounds the cross-field layer judges with context, so the schema layer must
 * stay quiet or the same mistake is reported twice. A toggle `tolerance` is the
 * one case: Rust bounds it only inside the `kind = "toggle"` arm, so the bound
 * is real there and absent everywhere else.
 */
const BOUNDS_OWNED_BY_CROSS_FIELD = new Set([
  "assert.tolerance",
  // Rust bounds both inside the `kind = "model_coverage"` arm, so on any other
  // assertion kind the loader accepts whatever is written.
  "assert.min_critical",
  "assert.min_resolved",
]);

function matchesScalar(value: TomlValue, want: string[]): boolean {
  for (const t of want) {
    if (t === "string" && typeof value === "string") return true;
    if (t === "boolean" && typeof value === "boolean") return true;
    if (t === "number" && typeof value === "number") return true;
    if (t === "integer" && typeof value === "number" && Number.isInteger(value)) return true;
  }
  return false;
}

function typeError(
  lk: TomlLookup,
  path: InstancePath,
  node: SchemaNode,
  want: string
): SpecIssue {
  return {
    span: spanFor(lk, path),
    severity: "error",
    message: `\`${String(path[path.length - 1])}\` must be ${want}${
      node.description ? `\n${firstSentence(node.description)}` : ""
    }`,
    code: "spec/bad-type",
  };
}

// ── layer 2: the conditional rules Spec::validate owns ───────────────────────

/** Assertion kinds that take a [min, max] window `Assertion::validate` inverts-checks. */
const WINDOW_KINDS = ["voltage", "rail_window", "phase_margin", "ac_gain", "peripheral"];

const NET_ATTACHED_PERIPHERALS = [
  "pushbutton",
  "toggle",
  "potentiometer",
  "encoder",
  "stimulus",
];

function crossFieldIssues(root: TomlTable, lk: TomlLookup, out: SpecIssue[]): void {
  const supplies = tables(root.supply);
  const asserts = tables(root.assert);
  const peripherals = tables(root.peripheral);
  const sensors = tables(root.sensor);
  const scenarios = tables(root.scenario);
  const overrides = tables(root.override);

  // ── supplies: no silent electrical assumptions ────────────────────────────
  supplies.forEach((s, i) => {
    const kind = str(s.kind);
    const net = str(s.net) ?? "?";
    const need = (field: string, extra: string) => {
      if (s[field] === undefined) {
        out.push({
          span: spanFor(lk, ["supply", i, "kind"]),
          severity: "error",
          message: `supply on '${net}': \`${kind}\` needs an explicit \`${field}\`; ${extra}`,
          code: `spec/supply-needs-${field}`,
        });
      }
    };
    if (kind === "ideal" || kind === "bench" || kind === "wall") {
      need(
        "volts",
        "add the rail's real voltage (e.g. `volts = 3.3`). Nothing is assumed: a wrong guess here would fabricate faults on a healthy board"
      );
    } else if (kind === "usb") {
      if (s.usb === undefined) {
        out.push({
          span: spanFor(lk, ["supply", i, "kind"]),
          severity: "error",
          message:
            `supply on '${net}': \`usb\` needs an explicit profile; add \`usb = "5v0.5a"\` ` +
            "(or 5v1.5a | 5v3a) to say what the port can actually deliver",
          code: "spec/supply-needs-usb",
        });
      }
    } else if (kind === "battery") {
      need(
        "chemistry",
        'add `chemistry = "liion"` (or alkaline | nimh | lifepo4), it sets the pack\'s voltage curve'
      );
    }
  });

  // ── peripherals ───────────────────────────────────────────────────────────
  peripherals.forEach((p, i) => {
    const kind = str(p.type);
    const id = str(p.id) ?? "?";
    if (
      kind &&
      NET_ATTACHED_PERIPHERALS.includes(kind) &&
      p.net === undefined &&
      p.ref === undefined &&
      p.nets === undefined &&
      p.net_a === undefined
    ) {
      out.push({
        span: tableSpan(lk, ["peripheral", i]),
        severity: "error",
        message: `peripheral '${id}' (${kind}) needs a \`net\`, a \`ref\`+\`pin\`, or \`nets\``,
        code: "spec/peripheral-needs-net",
      });
    }
    if (kind === "vcd_sink" && (!Array.isArray(p.nets) || p.nets.length === 0)) {
      out.push({
        span: tableSpan(lk, ["peripheral", i]),
        severity: "error",
        message:
          `peripheral '${id}' (vcd_sink) needs \`nets = [...]\` (the signals to log); ` +
          "a singular `net` is not read by the sink",
        code: "spec/vcd-sink-needs-nets",
      });
    }
  });

  // ── sensors ───────────────────────────────────────────────────────────────
  sensors.forEach((s, i) => {
    const id = str(s.id) ?? "?";
    const hasInline = s.spec !== undefined;
    const hasFile = s.spec_file !== undefined;
    if (!hasInline && !hasFile) {
      out.push({
        span: tableSpan(lk, ["sensor", i]),
        severity: "error",
        message: `sensor '${id}': needs either \`spec = "..."\` (inline) or \`spec_file = "path/to/sensor.toml"\``,
        code: "spec/sensor-needs-source",
      });
    } else if (hasInline && hasFile) {
      out.push({
        span: spanFor(lk, ["sensor", i, "spec_file"]),
        severity: "error",
        message: `sensor '${id}': \`spec\` and \`spec_file\` are mutually exclusive; provide only one`,
        code: "spec/sensor-source-conflict",
      });
    }
  });

  // ── assertions ────────────────────────────────────────────────────────────
  const scenarioIds = scenarios.map((s) => str(s.id)).filter((s): s is string => !!s);
  const peripheralIds = [
    ...peripherals.map((p) => str(p.id)),
    ...sensors.map((s) => str(s.id)),
  ].filter((s): s is string => !!s);
  const hasAc = isTable(root.ac);

  asserts.forEach((a, i) => {
    const kind = normaliseKind(str(a.kind));
    const here = (field?: string): Span =>
      (field ? lk.keySpan(["assert", i, field]) : undefined) ?? tableSpan(lk, ["assert", i]);
    /**
     * For a message about the VALUE rather than the key ("got 25; did you mean
     * 0.25?"), squiggle the value: that is where the mistake is, and it is what a
     * quick fix has to replace.
     */
    const atValue = (field: string): Span => lk.valueSpan(["assert", i, field]) ?? here(field);
    const fail = (message: string, code: string, field?: string) =>
      out.push({ span: here(field), severity: "error", message, code: `spec/${code}` });
    const target = str(a.net) ?? str(a.supply_net) ?? str(a.ref) ?? str(a.id) ?? "?";
    const needs = (field: string, what: string) => {
      if (a[field] === undefined) fail(`${kind} assertion needs ${what}`, `assert-needs-${field}`);
    };

    switch (kind) {
      case "voltage":
        needs("net", "a `net`");
        if (a.min === undefined && a.max === undefined)
          fail(`voltage assertion on '${target}' needs a \`min\` and/or \`max\``, "assert-needs-bound");
        break;
      case "uart":
        if (a.contains === undefined && a.matches === undefined)
          fail("uart assertion needs `contains` or `matches`", "assert-needs-bound");
        // No regex validation here on purpose. The loader compiles `matches`
        // with the Rust `regex` crate, whose grammar differs from JavaScript's
        // in BOTH directions: `(?P<name>…)` is valid in Rust and not in JS,
        // backreferences and lookbehind are valid in JS and not in Rust. A
        // `new RegExp()` check would therefore reject patterns hauksbee-ci
        // accepts. The loader layer reports a bad pattern with the real engine's
        // own message, which is the only correct answer available here.
        break;
      case "toggle":
        needs("net", "a `net`");
        if (a.freq_hz === undefined && a.min_toggles === undefined)
          fail(
            `toggle assertion on '${target}' needs \`freq_hz\` or \`min_toggles\``,
            "assert-needs-bound"
          );
        if (a.freq_hz !== undefined && a.min_toggles !== undefined)
          fail(
            `toggle assertion on '${target}' sets both \`freq_hz\` and \`min_toggles\`; use one (frequency OR count)`,
            "assert-exclusive",
            "min_toggles"
          );
        if (a.after_ms !== undefined)
          fail(
            `toggle assertion on '${target}' does not support \`after_ms\` (toggles are counted over the whole run)`,
            "assert-no-after-ms",
            "after_ms"
          );
        if (
          typeof a.tolerance === "number" &&
          // `!is_finite()` first, exactly as Rust has it: every comparison
          // against NaN is false, so a bare range test lets `nan` through.
          (!Number.isFinite(a.tolerance) || a.tolerance <= 0 || a.tolerance > 1)
        )
          out.push({
            span: atValue("tolerance"),
            severity: "error",
            message:
              `toggle assertion on '${target}': tolerance is a fraction (0.25 = +-25%), ` +
              `got ${a.tolerance}` +
              (a.tolerance > 1 ? `; did you mean ${a.tolerance / 100}?` : ""),
            code: "spec/assert-tolerance-range",
          });
        break;
      case "max_current":
        if (a.ref === undefined || a.amps === undefined)
          fail("max_current assertion needs `ref` and `amps`", "assert-needs-bound");
        break;
      case "max_temp":
        needs("ref", "a `ref` (the component to check)");
        break;
      case "peripheral": {
        needs("id", "an `id` naming the [[peripheral]] / [[sensor]] to read");
        const hasCheck =
          a.bytes !== undefined ||
          (a.field !== undefined && (a.min !== undefined || a.max !== undefined));
        if (!hasCheck)
          fail(
            `peripheral assertion on '${target}' needs \`bytes\` or a \`field\` with \`min\`/\`max\``,
            "assert-needs-bound"
          );
        if (a.bytes !== undefined && a.field !== undefined)
          fail(
            `peripheral assertion on '${target}' sets both \`bytes\` and \`field\`; use one ` +
              "(EEPROM-bytes OR a field range); a combined spec silently drops the field check",
            "assert-exclusive",
            "field"
          );
        const id = str(a.id);
        if (id && !peripheralIds.includes(id))
          fail(
            `peripheral assertion reads id '${id}', but no [[peripheral]] or [[sensor]] declares it` +
              (peripheralIds.length
                ? ` (declared ids: ${peripheralIds.join(", ")})`
                : " (the spec declares no [[peripheral]] or [[sensor]] blocks)"),
            "assert-unknown-id",
            "id"
          );
        break;
      }
      case "rail_window": {
        needs("net", "a `net`");
        const hasCheck =
          a.min !== undefined ||
          a.max !== undefined ||
          (a.dip_below !== undefined &&
            (a.for_max_ms !== undefined || a.recover_within_ms !== undefined));
        if (!hasCheck)
          fail(
            `rail_window on '${target}' needs at least one of: \`min\`, \`max\`, or \`dip_below\` with \`for_max_ms\`/\`recover_within_ms\``,
            "assert-needs-bound"
          );
        if (a.recover_within_ms !== undefined && (a.dip_below === undefined || a.recover_to === undefined))
          fail(
            "rail_window `recover_within_ms` needs both `dip_below` and `recover_to`",
            "assert-window-partner",
            "recover_within_ms"
          );
        if (a.recover_to !== undefined && a.recover_within_ms === undefined)
          fail(
            "rail_window `recover_to` needs `recover_within_ms` (and `dip_below`) or it is never evaluated",
            "assert-window-partner",
            "recover_to"
          );
        if (
          a.dip_below !== undefined &&
          a.for_max_ms === undefined &&
          a.recover_within_ms === undefined
        )
          fail(
            "rail_window `dip_below` needs `for_max_ms` or `recover_within_ms` or it is never evaluated",
            "assert-window-partner",
            "dip_below"
          );
        break;
      }
      case "protection_trip":
        needs("supply_net", "a `supply_net`");
        if (a.expect_trip === undefined)
          fail("protection_trip assertion needs `expect_trip = true|false`", "assert-needs-bound");
        break;
      case "phase_margin":
        needs("net", "a `net` (the loop break/output net)");
        if (a.min === undefined && a.max === undefined)
          fail(
            `phase_margin on '${target}' needs a \`min\` (and/or \`max\`) in degrees, e.g. min = 45`,
            "assert-needs-bound"
          );
        break;
      case "ac_gain":
        needs("net", "a `net`");
        if (a.min === undefined && a.max === undefined)
          fail(`ac_gain on '${target}' needs a \`min\` and/or \`max\` in dB`, "assert-needs-bound");
        break;
      case "hwtrace":
        needs("trace", "a `trace` (path to the trace.toml, relative to the spec file)");
        break;
      case "boot_coverage":
        needs("net", "a `net` (the control net to watch)");
        if (a.min === undefined)
          fail(
            `boot_coverage assertion on '${target}' needs a \`min\` (the driven level in volts the firmware must reach)`,
            "assert-needs-bound"
          );
        if (a.deadline_ms === undefined)
          fail(
            `boot_coverage assertion on '${target}' needs a \`deadline_ms\` (the boot deadline)`,
            "assert-needs-bound"
          );
        break;
      case "model_coverage":
        if (
          a.min_critical === undefined &&
          a.min_resolved === undefined &&
          a.max_active_unresolved === undefined
        )
          fail(
            "model_coverage assertion needs at least one of `min_critical` (fraction of active ICs bound), " +
              "`min_resolved` (fraction of all parts bound) or `max_active_unresolved` " +
              "(unresolved parts on connected nets)",
            "assert-needs-bound"
          );
        // Rust's `!(0.0..=1.0).contains(&v)`, which a non-finite value also
        // fails. Only inside this arm: elsewhere the keys are ignored.
        for (const field of ["min_critical", "min_resolved"]) {
          const v = a[field];
          if (typeof v === "number" && !(Number.isFinite(v) && v >= 0 && v <= 1)) {
            fail(
              `model_coverage \`${field}\` is a fraction between 0.0 and 1.0, got ${v}`,
              "assert-fraction-range",
              field
            );
          }
        }
        break;
      default:
        break;
    }

    // An inverted window can never hold, so it reads as a hardware RED for a
    // bound no measurement could satisfy. It is a spec error.
    if (
      kind &&
      WINDOW_KINDS.includes(kind) &&
      typeof a.min === "number" &&
      typeof a.max === "number" &&
      a.min > a.max
    ) {
      fail(
        `${kind} assertion on '${target}': min (${a.min}) is greater than max (${a.max}), ` +
          "a window nothing can satisfy; swap the bounds or fix the typo",
        "assert-inverted-window",
        "min"
      );
    }

    // AC assertions need the [ac] sweep block to drive them.
    if ((kind === "phase_margin" || kind === "ac_gain") && !hasAc) {
      fail(
        "a phase_margin / ac_gain assertion needs an [ac] sweep block (fstart, fstop, points)",
        "assert-needs-ac",
        "kind"
      );
    }

    // A `scenario` scope must name a declared [[scenario]] id, or the assertion
    // is silently measured over the WHOLE run instead of the scenario window.
    const scope = str(a.scenario);
    if (scope && !scenarioIds.includes(scope)) {
      const detail = scenarios.length === 0
        ? "the spec declares no [[scenario]] blocks"
        : scenarioIds.length === 0
          ? "the declared [[scenario]] blocks have no `id`; give the scenario an `id` and reference it here"
          : `declared scenario ids: ${scenarioIds.join(", ")}`;
      fail(
        `${kind} assertion is scoped to scenario '${scope}', but no [[scenario]] declares that id (${detail}); ` +
          "an unknown scope would silently be measured over the whole run instead of the scenario window",
        "assert-unknown-scenario",
        "scenario"
      );
    }
  });

  // ── overrides ─────────────────────────────────────────────────────────────
  overrides.forEach((o, i) => {
    if (o.distribution !== undefined && o.tolerance === undefined) {
      out.push({
        span: spanFor(lk, ["override", i, "distribution"]),
        severity: "error",
        message: `override on '${str(o.ref) ?? "?"}': \`distribution\` is only meaningful with \`tolerance\``,
        code: "spec/override-distribution",
      });
    }
  });

  // ── ensemble ──────────────────────────────────────────────────────────────
  if (isTable(root.ensemble)) {
    const hasTolerances =
      tables(root.tolerance).length > 0 || overrides.some((o) => o.tolerance !== undefined);
    if (!hasTolerances) {
      out.push({
        span: tableSpan(lk, ["ensemble"]),
        severity: "error",
        message:
          "[ensemble] without any [[tolerance]] rules (or an override with a `tolerance`) has nothing to sample",
        code: "spec/ensemble-no-tolerances",
      });
    }
    if (str(root.ensemble.mode) === "corners" && isTable(root.fuzz)) {
      out.push({
        span: spanFor(lk, ["ensemble", "mode"]),
        severity: "error",
        message:
          '[ensemble] mode = "corners" does not compose with [fuzz] (the corner index enumerates ' +
          'min/max combinations, not fuzz seeds); use mode = "monte-carlo" to run tolerances and net fuzz together',
        code: "spec/ensemble-corners-fuzz",
      });
    }
  }

  // ── [ac] sweep sanity ─────────────────────────────────────────────────────
  if (isTable(root.ac)) {
    const { fstart, fstop } = root.ac;
    if (typeof fstart === "number" && typeof fstop === "number" && fstop <= fstart) {
      out.push({
        span: spanFor(lk, ["ac", "fstop"]),
        severity: "error",
        message: `[ac] needs 0 < fstart < fstop, got fstart = ${fstart}, fstop = ${fstop}`,
        code: "spec/ac-sweep",
      });
    }
  }
}

/**
 * The fields whose finiteness the loader actually guards, keyed by table path
 * ("" is the root). TOML accepts `inf` and `nan` float literals and every
 * comparison against them is false, so each of these has an explicit
 * `is_finite()` check in Rust: `duration_ms = inf` would spin the frame loop
 * forever, a NaN `after_ms` panics the threshold sort, a non-finite supply
 * `volts` poisons every node it touches.
 *
 * Listing them rather than flagging every non-finite number matters, because
 * the loader does NOT guard, say, a peripheral's `temp_c`, and flagging a spec
 * hauksbee-ci accepts is the worst thing this file could do.
 */
const GUARDED_FINITE: Record<string, string[]> = {
  "": ["duration_ms", "frame_ms"],
  supply: [
    "volts",
    "current_limit_a",
    "r_out_ohms",
    "ripple_vpp",
    "ripple_hz",
    "capacity_mah",
    "r_internal_ohms",
    "protection_trip_a",
    "protection_delay_ms",
    "protection_reset_a",
    "soc",
  ],
  // NOT `tolerance`: Rust bounds it only inside the `kind = "toggle"` arm, so
  // the cross-field layer owns it and a `voltage` assertion's `tolerance = nan`
  // is something the loader accepts.
  assert: ["after_ms", "deadline_ms"],
  ac: ["fstart", "fstop"],
  tolerance: ["percent"],
  override: ["tolerance"],
};

function nonFiniteNumbers(lk: TomlLookup, out: SpecIssue[]): void {
  for (const e of lk.doc.entries) {
    if (typeof e.value !== "number" || Number.isFinite(e.value)) continue;
    const table = e.schemaPath.slice(0, -1).join(".");
    if (!GUARDED_FINITE[table]?.includes(e.key)) continue;
    const literal = e.value === Infinity ? "inf" : e.value === -Infinity ? "-inf" : "nan";
    out.push({
      span: e.valueSpan,
      severity: "error",
      message:
        `\`${e.key}\` must be a finite number (got ${literal}); hauksbee-ci rejects a ` +
        "non-finite value here rather than running a simulation nothing can interpret",
      code: "spec/non-finite",
    });
  }
}

// ── helpers ──────────────────────────────────────────────────────────────────

/** `boot-coverage` is the accepted legacy spelling of `boot_coverage`. */
export function normaliseKind(kind: string | undefined): string | undefined {
  return kind === "boot-coverage" ? "boot_coverage" : kind;
}

function isTable(v: TomlValue | undefined): v is TomlTable {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

function tables(v: TomlValue | undefined): TomlTable[] {
  if (!Array.isArray(v)) return [];
  return v.filter(isTable);
}

function str(v: TomlValue | undefined): string | undefined {
  return typeof v === "string" ? v : undefined;
}

function spanFor(lk: TomlLookup, path: InstancePath): Span {
  return lk.valueSpan(path) ?? lk.keySpan(path) ?? tableSpan(lk, path.slice(0, -1));
}

function tableSpan(lk: TomlLookup, path: InstancePath): Span {
  return lk.valueSpan(path) ?? lk.tableSpan(path);
}

function describeFor(schema: SpecSchema, node: SchemaNode | undefined): string {
  const resolved = schema.resolve(node);
  const d = node?.description ?? resolved?.description;
  return d ? `: ${firstSentence(d)}` : "";
}

function firstSentence(s: string): string {
  const flat = s.replace(/\s+/g, " ").trim();
  const stop = flat.indexOf(". ");
  return stop > 0 ? flat.slice(0, stop + 1) : flat;
}

/** Port of `hauksbee_ci::error::did_you_mean`: closest option within 2 edits. */
export function didYouMean(target: string, options: readonly string[]): string | undefined {
  const t = target.toLowerCase();
  let best: { d: number; o: string } | undefined;
  for (const o of options) {
    const d = levenshtein(t, o.toLowerCase());
    if (d === 0) return undefined; // in the vocabulary; not a typo
    if (d <= 2 && (!best || d < best.d)) best = { d, o };
  }
  return best?.o;
}

function hint(target: string, options: readonly string[]): string {
  const m = didYouMean(target, options);
  return m ? ` (did you mean '${m}'?)` : "";
}

export function levenshtein(a: string, b: string): number {
  if (a.length === 0) return b.length;
  if (b.length === 0) return a.length;
  let prev = Array.from({ length: b.length + 1 }, (_, k) => k);
  let cur = new Array<number>(b.length + 1);
  for (let i = 1; i <= a.length; i++) {
    cur[0] = i;
    for (let j = 1; j <= b.length; j++) {
      const cost = a[i - 1] === b[j - 1] ? 0 : 1;
      cur[j] = Math.min(prev[j] + 1, cur[j - 1] + 1, prev[j - 1] + cost);
    }
    [prev, cur] = [cur, prev];
  }
  return prev[b.length];
}

export { pathKey };
