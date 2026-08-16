// The drift guard on the cross-field lint layer.
//
// src/spec/lint.ts reimplements `Spec::validate` in TypeScript, because there is
// no way to ask `hauksbee-ci` "just load this" cheaply enough to run on every
// keystroke. A second implementation of someone else's rules is a liability
// unless something holds the two together, so this test IS that something:
//
//   * every fixture the extension rejects must also be rejected by the real
//     `hauksbee-ci` binary (exit 2), so the editor never invents an error;
//   * every fixture the loader ACCEPTS must be silent in the editor. This is the
//     direction that matters most: a false error on a valid spec is worse than a
//     missed one, because it makes the editor untrustworthy.
//
// If the loader's rules move, this fails and lint.ts gets updated. The test
// skips when no binary can be found, so it never blocks a contributor who has
// not built the workspace.

import { describe, expect, test } from "bun:test";
import { spawnSync } from "child_process";
import * as fs from "fs";
import * as path from "path";
import { isExecutableFile } from "../binaries";
import { lintSpec } from "./lint";
import { mapLoaderStderr } from "./loaderDiag";
import { SpecSchema } from "./schemaModel";

const ROOT = path.join(__dirname, "..", "..");
const REPO = path.join(ROOT, "..", "..");
const FIX = path.join(ROOT, "test", "fixtures", "spec");
const schema = new SpecSchema(
  JSON.parse(fs.readFileSync(path.join(ROOT, "schemas", "hauksbee-ci-spec.schema.json"), "utf8"))
);

// Deliberately NOT the extension's runtime discovery order, which prefers PATH:
// a parity test must compare against THIS checkout's loader, and an installed
// `hauksbee-ci` on PATH is routinely older than the source it is being compared
// to. (That gap is also why the extension keeps its own lint layer instead of
// trusting the binary alone.)
const BIN = [
  path.join(REPO, "target", "release", "hauksbee-ci"),
  path.join(REPO, "target", "debug", "hauksbee-ci"),
].find(isExecutableFile);

/** Fixtures the extension flags. Every one must fail the loader too. */
const REJECTED = [
  "typo_kind.toml",
  "supply_no_volts.toml",
  "usb_no_profile.toml",
  "inverted_window.toml",
  "toggle_percent_tolerance.toml",
  "unknown_key.toml",
  "unknown_scenario_scope.toml",
  "no_asserts.toml",
  "vcd_sink_singular_net.toml",
  "nonfinite_duration.toml",
  "ac_missing.toml",
];

/**
 * Fixtures the loader accepts, each one a construct that is easy to get wrong.
 * They all point at a board that does not exist, so "no board file at" is the
 * loader saying structural validation passed.
 */
const ACCEPTED = [
  // The loader guards finiteness field by field; a peripheral's `temp_c` is not
  // one of them, so a blanket non-finite rule would be a false error here.
  "accept_nonfinite_temp.toml",
  // An explicit `scenario = ""` is the run-wide window, not an unknown scope.
  "accept_empty_scope.toml",
  // An I2C peripheral attaches over the bus, so it needs no `net`.
  "accept_lm75_nonet.toml",
  // Dotted keys instead of table headers.
  "accept_dotted_keys.toml",
  // Arrays of inline tables instead of [[table]] headers.
  "accept_inline_tables.toml",
  // A multi-line string holding a whole nested TOML document.
  "accept_multiline_sensor.toml",
  // The inclusive upper bound of the toggle tolerance range.
  "accept_tolerance_boundary.toml",
  // ref + pin attachment instead of a net name.
  "accept_ref_pin.toml",
];

function runLoader(fixture: string): { code: number; stderr: string } {
  const r = spawnSync(BIN!, ["run", path.join(FIX, fixture), "--quiet"], {
    cwd: FIX,
    encoding: "utf8",
    timeout: 60_000,
  });
  return { code: r.status ?? -1, stderr: r.stderr ?? "" };
}

describe.skipIf(!BIN)("loader parity", () => {
  test("the binary was found", () => {
    expect(BIN).toBeTruthy();
  });

  for (const fixture of REJECTED) {
    test(`${fixture}: the extension and hauksbee-ci both reject it`, () => {
      const local = lintSpec(fs.readFileSync(path.join(FIX, fixture), "utf8"), schema);
      expect(local.issues.filter((i) => i.severity === "error").length).toBeGreaterThan(0);

      const { code, stderr } = runLoader(fixture);
      // Exit 2 is the spec/usage-error contract; nothing else is acceptable
      // here, and in particular a 0 would mean the extension invented an error.
      expect({ fixture, code }).toEqual({ fixture, code: 2 });
      // The rejection must be about the SPEC, not about the missing board file
      // (these fixtures deliberately point at `nope.kicad_pcb`, which the
      // loader only reaches after structural validation passes).
      expect(stderr).not.toContain("no board file at");
    });
  }

  for (const fixture of ["valid.toml", ...ACCEPTED]) {
    test(`${fixture}: the loader accepts it, so the extension stays silent`, () => {
      const { stderr } = runLoader(fixture);
      // The board does not exist, and that is as far as the loader gets: reaching
      // board resolution IS the proof that structural validation passed.
      expect({ fixture, reached: stderr.includes("no board file at") }).toEqual({
        fixture,
        reached: true,
      });
      expect(stderr).not.toContain("invalid spec:");
      expect(stderr).not.toContain("TOML parse error");

      const local = lintSpec(fs.readFileSync(path.join(FIX, fixture), "utf8"), schema);
      expect({ fixture, issues: local.issues.map((i) => `${i.code}: ${i.message}`) }).toEqual({
        fixture,
        issues: [],
      });
    });
  }

  /**
   * The generated schema carries numeric bounds from `#[schemars(...)]`
   * attributes, and only SOME of them are re-checked by Rust code. Hand-picking
   * fixtures missed that distinction once, so this drives the whole class from
   * the schema itself: for every bounded numeric field, build a spec that
   * violates the bound, ask the real loader, and require the extension's verdict
   * to agree about whether it is an ERROR.
   */
  test("every bound and vocabulary in the schema agrees with the loader about severity", () => {
    const raw = JSON.parse(
      fs.readFileSync(path.join(ROOT, "schemas", "hauksbee-ci-spec.schema.json"), "utf8")
    );
    const cases = probeCases(raw);
    // A guard against this test silently covering nothing. All three generators
    // must contribute: bound-derived values, integer-type-derived ones, and one
    // bogus token per closed vocabulary.
    expect(cases.length).toBeGreaterThan(40);
    expect(cases.some((c) => c.bad === -1)).toBe(true);
    expect(cases.some((c) => typeof c.bad === "number" && c.bad > Number.MAX_SAFE_INTEGER)).toBe(
      true
    );
    expect(cases.filter((c) => typeof c.bad === "string").length).toBeGreaterThan(8);
    // And the assert table must be probed under every kind, not just one: that
    // blind spot is what let a kind-conditional bound through once.
    expect(new Set(cases.filter((c) => c.kind).map((c) => c.kind)).size).toBe(14);

    const disagreements: string[] = [];
    let probe = 0;
    for (const c of cases) {
      const src = specWith(c.table, c.key, c.bad, c.kind);
      // A unique name per probe: nothing can be left behind by a previous case.
      const file = path.join(FIX, `.probe_${probe++}.toml`);
      fs.writeFileSync(file, src);
      let loaderRejects: boolean;
      try {
        // Every one of these specs points at a board that does not exist, so the
        // loader ALWAYS has something to say. An empty stderr therefore means the
        // spawn itself misfired, not that the spec was rejected; reading it as a
        // rejection is how this sweep would report a phantom disagreement. Retry,
        // then fail loudly rather than guess.
        let stderr = "";
        for (let attempt = 0; attempt < 3 && stderr.trim() === ""; attempt++) {
          const r = spawnSync(BIN!, ["run", file, "--quiet"], {
            cwd: FIX,
            encoding: "utf8",
            timeout: 60_000,
          });
          stderr = r.stderr ?? "";
        }
        expect({ probe: `${c.table}.${c.key}`, gotOutput: stderr.trim() !== "" }).toEqual({
          probe: `${c.table}.${c.key}`,
          gotOutput: true,
        });
        // Reaching board resolution means structural validation passed.
        loaderRejects = !stderr.includes("no board file at");
      } finally {
        fs.rmSync(file, { force: true });
      }
      const errors = lintSpec(src, schema).issues.filter((i) => i.severity === "error");
      const extensionRejects = errors.length > 0;
      if (extensionRejects !== loaderRejects) {
        disagreements.push(
          `${c.table || "(root)"}${c.kind ? `(${c.kind})` : ""}.${c.key} = ${c.bad}: loader ${
            loaderRejects ? "rejects" : "accepts"
          }, extension ${extensionRejects ? `errors (${errors.map((e) => e.code).join(",")})` : "is silent"}`
        );
      }
    }
    expect(disagreements).toEqual([]);
    // eslint-disable-next-line no-console -- coverage is the point of this test
    console.log(`bounds/vocabulary sweep: ${cases.length} probes against the real loader`);
    // The sweep spawns the binary once per probe, so it needs a real timeout;
    // bun's 5 s default silently aborted it mid-run and reported the truncation
    // as a disagreement.
  }, 180_000);

  test("the loader's own message lands on the right line", () => {
    const fixture = "supply_no_volts.toml";
    const text = fs.readFileSync(path.join(FIX, fixture), "utf8");
    const { stderr } = runLoader(fixture);
    const issues = mapLoaderStderr(stderr, lintSpec(text, schema).doc);
    expect(issues).toHaveLength(1);
    expect(issues[0].message).toContain("supply on '+5V'");
    expect(issues[0].span.start.line).toBe(
      text.split("\n").findIndex((l) => l.includes('kind = "bench"'))
    );
  });
});

// ── driving the bounds sweep from the schema ─────────────────────────────────

interface ProbeCase {
  /** Table path as written in a spec, "" for the root. */
  table: string;
  key: string;
  /** A value the schema says is wrong: outside a bound, or outside a vocabulary. */
  bad: number | string;
  /**
   * For the `assert` table, which assertion kind to probe under. Several of
   * Rust's checks live INSIDE a `match kind` arm, so probing one kind proves
   * nothing about the others: `min_critical` is bounded under `model_coverage`
   * and ignored everywhere else.
   */
  kind?: string;
}

/**
 * Minimal valid bodies for every assertion kind, so the `assert` sweep can put
 * each probe under each kind. Keyed by the kind's own `kind` value.
 */
const ASSERT_BODIES: Record<string, string[]> = {
  voltage: ['net = "VCC"', "min = 3.0"],
  uart: ['contains = "ok"'],
  toggle: ['net = "LED"', "freq_hz = 1.0"],
  no_faults: [],
  max_current: ['ref = "R1"', "amps = 1.0"],
  max_temp: ['ref = "U1"'],
  peripheral: ['id = "P"', 'bytes = "4869"'],
  rail_window: ['net = "+5V"', "min = 4.5"],
  protection_trip: ['supply_net = "VBAT"', "expect_trip = true"],
  boot_coverage: ['net = "EN"', "min = 2.4", "deadline_ms = 100.0"],
  phase_margin: ['net = "FB"', "min = 45.0"],
  ac_gain: ['net = "FB"', "min = 0.0"],
  hwtrace: ['trace = "t.toml"'],
  model_coverage: ["min_resolved = 0.5"],
};

/**
 * Every numeric field in the schema that declares a bound, paired with a value
 * that violates it. Walks two levels, which covers every table a spec has.
 */
function probeCases(root: any): ProbeCase[] {
  const out: ProbeCase[] = [];
  const resolve = (node: any): any => {
    if (!node) return node;
    if (node.$ref) return resolve(root.definitions[node.$ref.replace("#/definitions/", "")]);
    if (node.allOf?.length === 1) return resolve(node.allOf[0]);
    if (node.anyOf) return resolve(node.anyOf.find((a: any) => a.type !== "null") ?? node);
    return node;
  };
  const visit = (node: any, table: string, depth: number): void => {
    const t = resolve(node);
    if (!t?.properties || depth > 2) return;
    for (const [key, rawProp] of Object.entries<any>(t.properties)) {
      const prop = resolve(rawProp);
      const item = prop?.type === "array" ? resolve(prop.items) : undefined;
      if (item?.properties || prop?.properties) {
        visit(prop?.properties ? prop : item, table ? `${table}.${key}` : key, depth + 1);
        continue;
      }
      const values: (number | string)[] = [...violating(prop)];
      // One bogus token per closed vocabulary. The enum half of the schema uses
      // the same enforced-versus-documented split as the numeric half.
      if (vocabulary(prop).length > 0) values.push("hauksbee_probe_bogus");
      for (const bad of values) {
        // `kind` is the discriminant itself: probing it under every kind would
        // just be the same spec fourteen times.
        if (table === "assert" && key !== "kind") {
          for (const kind of Object.keys(ASSERT_BODIES)) out.push({ table, key, bad, kind });
        } else {
          out.push({ table, key, bad });
        }
      }
    }
  };
  visit(root, "", 0);
  return out;
}

/**
 * Values worth probing for one field: whatever violates a declared bound, plus
 * the cases the Rust INTEGER TYPE decides rather than the bound. The format
 * cases matter because they are the half a bounds-only sweep misses: a `u64`
 * has no usable upper bound in a JavaScript double, and claiming one reported
 * errors on specs hauksbee-ci runs.
 */
/** The closed set of strings a node accepts, in either schemars spelling. */
function vocabulary(node: any): string[] {
  if (!node) return [];
  const direct = (node.enum ?? []).filter((v: unknown) => typeof v === "string");
  if (direct.length) return direct;
  return (node.oneOf ?? node.anyOf ?? [])
    .map((v: any) => v.const)
    .filter((v: unknown) => typeof v === "string");
}

function violating(node: any): number[] {
  if (!node) return [];
  const out: number[] = [];
  const integer = String(node.type).includes("integer");
  const step = integer ? 1 : 0.5;
  if (node.exclusiveMinimum !== undefined) out.push(node.exclusiveMinimum);
  else if (node.minimum !== undefined) out.push(node.minimum - step);
  if (node.exclusiveMaximum !== undefined) out.push(node.exclusiveMaximum);
  else if (node.maximum !== undefined) out.push(node.maximum + step);

  const format = /^(u?int)(8|16|32|64)?$/.exec(node.format ?? "");
  if (format) {
    const bits = format[2] ? Number(format[2]) : 64;
    // Below the type's floor: serde rejects it, whatever validation says.
    if (format[1] === "uint") out.push(-1);
    // Above 2^53 but inside the type: a perfectly good value that a naive
    // double-based range check would reject.
    if (bits >= 64) out.push(Number.MAX_SAFE_INTEGER + 2);
    else out.push(2 ** bits + 1);
  }
  return [...new Set(out)];
}

/**
 * The smallest spec that loads, with one field set to the probe value. Each
 * table gets whatever else it needs to reach the check (a supply needs its
 * `volts`, an `[ac]` block needs a sweep, an `[ensemble]` needs a tolerance).
 */
function specWith(table: string, key: string, bad: number | string, kind?: string): string {
  const set = typeof bad === "string" ? `${key} = "${bad}"` : `${key} = ${bad}`;
  const head = 'board = "nope.kicad_pcb"';
  const asserts = ['[[assert]]', 'kind = "no_faults"'];

  // The scaffolding may already set the key being probed (a supply's `kind`, an
  // assertion's `kind`); TOML forbids a duplicate, so drop it and add ours.
  const block = (...lines: string[]): string[] => [
    ...lines.filter((l) => !new RegExp(`^\\s*${key}\\s*=`).test(l)),
    set,
  ];
  const doc = (...parts: string[][]): string =>
    parts.map((p) => p.join("\n")).join("\n\n") + "\n";

  switch (table) {
    case "":
      return doc([head], block(), asserts);
    case "supply":
      return doc(
        [head],
        block("[[supply]]", 'net = "+5V"', 'kind = "bench"', "volts = 5.0"),
        asserts
      );
    case "assert": {
      const k = kind ?? "model_coverage";
      // `phase_margin` / `ac_gain` need an [ac] block; giving every kind one is
      // harmless and keeps the scaffolding uniform.
      // `boot_coverage` also needs a firmware declaration at load time. The
      // board is intentionally missing, so this placeholder is never executed.
      return doc(
        k === "boot_coverage" ? [head, 'firmware = "firmware.elf"'] : [head],
        ["[ac]", "fstart = 10.0", "fstop = 1000.0", "points = 10"],
        block("[[assert]]", `kind = "${k}"`, ...ASSERT_BODIES[k])
      );
    }
    case "peripheral":
      return doc([head], block("[[peripheral]]", 'id = "P"', 'type = "i2c_eeprom"'), asserts);
    case "peripheral.event":
      return doc(
        [head],
        ["[[peripheral]]", 'id = "P"', 'type = "pushbutton"', 'net = "B"'],
        block("[[peripheral.event]]", "t_ms = 1.0", "value = 1.0"),
        asserts
      );
    case "scenario":
      return doc(
        [head],
        block("[[scenario]]", 'part = "U5"', 'profile = "esp32_boot_wifi"'),
        asserts
      );
    case "ac":
      return doc([head], block("[ac]", "fstart = 10.0", "fstop = 1000.0", "points = 10"), asserts);
    case "fuzz":
      return doc([head], block("[fuzz]", "seeds = 4"), asserts);
    case "ensemble":
      return doc(
        [head],
        ["[[tolerance]]", 'ref = "R*"', "percent = 10.0"],
        block("[ensemble]", "seeds = 4"),
        asserts
      );
    case "tolerance":
      return doc([head], block("[[tolerance]]", 'ref = "R*"', "percent = 10.0"), asserts);
    case "override":
      return doc(
        [head],
        block("[[override]]", 'ref = "R1"', 'value = "1k"', "tolerance = 5.0"),
        asserts
      );
    case "profile":
      return doc([head], block("[[profile]]", 'id = "p"'), asserts);
    case "profile.segment":
      return doc(
        [head],
        ["[[profile]]", 'id = "p"'],
        block("[[profile.segment]]", "level_a = 0.1"),
        asserts
      );
    case "decoupling":
      return doc([head], block("[decoupling]", "parasitics = true"), asserts);
    case "decoupling.override":
      return doc(
        [head],
        ["[decoupling]", "parasitics = true"],
        block("[[decoupling.override]]", 'ref = "C1"'),
        asserts
      );
    case "sensor":
      return doc([head], block("[[sensor]]", 'id = "S"', 'spec_file = "s.toml"'), asserts);
    case "timing":
      return doc(
        [head],
        block("[timing]", "min_pulse_us = 1.0", "max_edge_error_us = 0.5"),
        asserts
      );
    default:
      // A table this helper does not know how to build. Returning a spec without
      // the probe would silently pass, so fail loudly instead: the count
      // assertion in the test would not notice a quietly-skipped table.
      throw new Error(`specWith has no scaffolding for table '${table}' (probing '${key}')`);
  }
}
