// The always-on spec lint: what it flags, where, and (in parity.test.ts) that
// the real loader agrees.

import { describe, expect, test } from "bun:test";
import * as fs from "fs";
import * as path from "path";
import { didYouMean, levenshtein, lintSpec, normaliseKind, type SpecIssue } from "./lint";
import { SpecSchema } from "./schemaModel";

const ROOT = path.join(__dirname, "..", "..");
const FIX = path.join(ROOT, "test", "fixtures", "spec");
export const read = (name: string) => fs.readFileSync(path.join(FIX, name), "utf8");
export const schema = new SpecSchema(
  JSON.parse(
    fs.readFileSync(path.join(ROOT, "schemas", "hauksbee-ci-spec.schema.json"), "utf8")
  )
);

function lint(name: string): SpecIssue[] {
  const result = lintSpec(read(name), schema);
  expect(result.analysed).toBe(true);
  return result.issues;
}

/** The 0-based line of the first line containing `needle`. */
function lineOf(name: string, needle: string): number {
  const i = read(name)
    .split("\n")
    .findIndex((l) => l.includes(needle));
  expect(i).toBeGreaterThanOrEqual(0);
  return i;
}

describe("a spec that loads clean", () => {
  test("produces no diagnostics at all", () => {
    expect(lint("valid.toml")).toEqual([]);
  });

  test("the shipped example specs are clean too", () => {
    const examples = path.join(ROOT, "..", "..", "crates", "hauksbee-ci", "examples");
    const files = fs
      .readdirSync(examples)
      .filter((f) => f.endsWith(".toml"))
      .map((f) => path.join(examples, f));
    expect(files.length).toBeGreaterThan(5);
    for (const file of files) {
      const result = lintSpec(fs.readFileSync(file, "utf8"), schema);
      expect({ file: path.basename(file), issues: result.issues.map((i) => i.message) }).toEqual({
        file: path.basename(file),
        issues: [],
      });
    }
  });
});

describe("structural layer (the generated schema)", () => {
  test("an unknown top-level key is flagged on the key, with a suggestion", () => {
    const [issue, ...rest] = lint("unknown_key.toml");
    expect(rest).toEqual([]);
    expect(issue.code).toBe("spec/unknown-key");
    expect(issue.message).toContain("unknown key `duraton_ms`");
    expect(issue.message).toContain("did you mean 'duration_ms'?");
    expect(issue.span.start).toEqual({ line: lineOf("unknown_key.toml", "duraton_ms"), col: 0 });
    expect(issue.span.end.col).toBe("duraton_ms".length);
  });

  test("an unknown assertion kind is flagged on the value", () => {
    const issues = lint("typo_kind.toml");
    const bad = issues.find((i) => i.code === "spec/bad-enum")!;
    expect(bad.message).toContain("'voltag' is not a valid kind");
    expect(bad.message).toContain("did you mean 'voltage'?");
    expect(bad.span.start.line).toBe(lineOf("typo_kind.toml", "voltag"));
    // The span covers the quoted value, not the whole line.
    expect(bad.span.start.col).toBe('kind = '.length);
  });

  test("both boot_coverage spellings are accepted", () => {
    for (const kind of ["boot_coverage", "boot-coverage"]) {
      const src = [
        'board = "b.kicad_pcb"',
        "",
        "[[assert]]",
        `kind = "${kind}"`,
        'net = "EN"',
        "min = 2.4",
        "deadline_ms = 500",
      ].join("\n");
      expect(lintSpec(src, schema).issues).toEqual([]);
    }
  });

  test("a missing required key is reported on the table header", () => {
    const issues = lintSpec('duration_ms = 5\n\n[[assert]]\nkind = "no_faults"\n', schema).issues;
    const missing = issues.find((i) => i.code === "spec/missing-key")!;
    expect(missing.message).toContain("missing required key `board`");
    expect(missing.span.start.line).toBe(0);
  });

  test("`[assert]` where `[[assert]]` was meant says both spellings", () => {
    const issue = lintSpec('board = "b.kicad_pcb"\n\n[assert]\nkind = "no_faults"\n', schema)
      .issues.find((i) => i.code === "spec/bad-type")!;
    expect(issue.message).toContain("write `[[assert]]`");
    expect(issue.message).toContain("parent block it belongs to exists");
  });

  test("an out-of-range value names the bound", () => {
    const src = [
      'board = "b.kicad_pcb"',
      "",
      "[[supply]]",
      'net = "VBAT"',
      'kind = "battery"',
      'chemistry = "liion"',
      "soc = 1.5",
      "",
      "[[assert]]",
      'kind = "no_faults"',
    ].join("\n");
    const issue = lintSpec(src, schema).issues.find((i) => i.code === "spec/out-of-range")!;
    expect(issue.message).toBe("soc must be >= 0, <= 1, got 1.5");
    expect(issue.span.start.line).toBe(6);
  });

  test("a Rust unit enum (dnp) is a closed vocabulary too", () => {
    // DnpMode arrives as `oneOf: [{const: "fit-except-links"}, ...]`, not as a
    // plain `enum`; reading only one spelling would let a typo through.
    const bad = 'board = "b.kicad_pcb"\ndnp = "honor"\n\n[[assert]]\nkind = "no_faults"\n';
    const issue = lintSpec(bad, schema).issues.find((i) => i.code === "spec/bad-enum")!;
    expect(issue.message).toContain("'honor' is not a valid dnp");
    expect(issue.message).toContain("did you mean 'honour'?");
    expect(issue.message).toContain("fit-except-links | fit-all | honour");

    const good = 'board = "b.kicad_pcb"\ndnp = "honour"\n\n[[assert]]\nkind = "no_faults"\n';
    expect(lintSpec(good, schema).issues).toEqual([]);
  });

  test("a bound Spec::validate re-checks is an error", () => {
    // The schema's bounds come from `#[schemars(...)]` attributes, and the
    // loader now re-checks the peripheral address before a run. The extension
    // must make the same invalid spec actionable instead of downgrading it to
    // a warning. See parity.test.ts's bounds sweep.
    const src = [
      'board = "b.kicad_pcb"',
      "",
      "[[peripheral]]",
      'id = "E1"',
      'type = "i2c_eeprom"',
      "address = 200",
      "",
      "[[assert]]",
      'kind = "no_faults"',
    ].join("\n");
    const issues = lintSpec(src, schema).issues;
    expect(issues).toHaveLength(1);
    expect(issues[0].severity).toBe("error");
    expect(issues[0].code).toBe("spec/out-of-range");
    expect(issues[0].message).not.toContain("does not currently reject this");
  });

  test("a vocabulary Spec::validate re-checks is an error too", () => {
    // `peripheral.waveform` is a closed vocabulary validated by the loader for
    // every peripheral, so the editor must reject an unknown token up front.
    const src = [
      'board = "b.kicad_pcb"',
      "",
      "[[peripheral]]",
      'id = "B"',
      'type = "pushbutton"',
      'net = "X"',
      'waveform = "square"',
      "",
      "[[assert]]",
      'kind = "no_faults"',
    ].join("\n");
    const issues = lintSpec(src, schema).issues;
    expect(issues).toHaveLength(1);
    expect(issues[0].severity).toBe("error");
    expect(issues[0].code).toBe("spec/bad-enum");
    expect(issues[0].message).not.toContain("once the run reaches it");

    // Every other vocabulary IS re-checked, so those stay errors.
    for (const [table, line] of [
      ["[[supply]]\nnet = \"V\"\nvolts = 1.0", 'kind = "bnch"'],
      ["[[assert]]", 'kind = "voltag"'],
      ["[[peripheral]]\nid = \"P\"\nnet = \"X\"", 'type = "pushbuton"'],
    ]) {
      const s2 = `board = "b.kicad_pcb"\n\n${table}\n${line}\n\n[[assert]]\nkind = "no_faults"\n`;
      const bad = lintSpec(s2, schema).issues.find((i) => i.code === "spec/bad-enum");
      expect({ line, found: !!bad, severity: bad?.severity }).toEqual({
        line,
        found: true,
        severity: "error",
      });
    }
  });

  test("a bound the Rust type itself enforces is an error: serde rejects it", () => {
    const src = [
      'board = "b.kicad_pcb"',
      "",
      "[[assert]]",
      'kind = "model_coverage"',
      "max_active_unresolved = -1",
    ].join("\n");
    const issue = lintSpec(src, schema).issues.find((i) => i.code === "spec/out-of-range")!;
    expect(issue.severity).toBe("error");
    expect(issue.message).toContain("must be zero or positive");
    expect(issue.message).toContain("serde rejects it");
  });

  test("a uart regex is left to the loader: the Rust and JS grammars differ", () => {
    // `(?P<name>…)` is valid in the Rust `regex` crate and invalid in JavaScript.
    // A `new RegExp()` check here would reject a spec hauksbee-ci accepts.
    const src = [
      'board = "b.kicad_pcb"',
      "",
      "[[assert]]",
      'kind = "uart"',
      'matches = "(?P<v>boot ok)"',
    ].join("\n");
    expect(lintSpec(src, schema).issues).toEqual([]);
  });

  test("a wrong type is flagged rather than silently coerced", () => {
    const src = 'board = 12\n\n[[assert]]\nkind = "no_faults"\n';
    const issue = lintSpec(src, schema).issues.find((i) => i.code === "spec/bad-type")!;
    expect(issue.message).toContain("`board` must be a string");
  });

  test("no [[assert]] block is the vacuous-pass guard, phrased as the loader does", () => {
    const issue = lint("no_asserts.toml").find((i) => i.code === "spec/missing-key")!;
    // Not "missing required key `assert`": nobody writes `assert = []`, and the
    // quick fix inserts a block, so the message names the block.
    expect(issue.message).toBe(
      "this spec has no `[[assert]]` block: a check with no assertions always passes vacuously"
    );
  });
});

describe("cross-field layer (Spec::validate)", () => {
  test("a bench supply with no volts, on the kind line", () => {
    const [issue] = lint("supply_no_volts.toml");
    expect(issue.code).toBe("spec/supply-needs-volts");
    expect(issue.message).toContain("supply on '+5V': `bench` needs an explicit `volts`");
    expect(issue.message).toContain("would fabricate faults on a healthy board");
    expect(issue.span.start.line).toBe(lineOf("supply_no_volts.toml", 'kind = "bench"'));
  });

  test("a usb supply with no profile", () => {
    const [issue] = lint("usb_no_profile.toml");
    expect(issue.code).toBe("spec/supply-needs-usb");
    expect(issue.message).toContain('add `usb = "5v0.5a"`');
  });

  test("min > max is a spec error, not a hardware failure", () => {
    const [issue] = lint("inverted_window.toml");
    expect(issue.code).toBe("spec/assert-inverted-window");
    expect(issue.message).toContain("min (5) is greater than max (1)");
    expect(issue.span.start.line).toBe(lineOf("inverted_window.toml", "min = 5.0"));
  });

  test("a toggle tolerance written as a percentage suggests the fraction", () => {
    const issues = lint("toggle_percent_tolerance.toml");
    const issue = issues.find((i) => i.code === "spec/assert-tolerance-range")!;
    expect(issue.message).toContain("tolerance is a fraction (0.25 = +-25%), got 25");
    expect(issue.message).toContain("did you mean 0.25?");
    expect(issue.span.start.line).toBe(
      lineOf("toggle_percent_tolerance.toml", "tolerance = 25.0")
    );
  });

  test("a scenario scope that names nothing", () => {
    const issue = lint("unknown_scenario_scope.toml").find(
      (i) => i.code === "spec/assert-unknown-scenario"
    )!;
    expect(issue.message).toContain("scoped to scenario 'inrush'");
    expect(issue.message).toContain("the spec declares no [[scenario]] blocks");
    expect(issue.span.start.line).toBe(
      lineOf("unknown_scenario_scope.toml", 'scenario = "inrush"')
    );
  });

  test("a vcd_sink with a singular net logs nothing, so it is rejected", () => {
    const issue = lint("vcd_sink_singular_net.toml").find(
      (i) => i.code === "spec/vcd-sink-needs-nets"
    )!;
    expect(issue.message).toContain("needs `nets = [...]`");
  });

  test("an AC assertion with no [ac] block", () => {
    const issue = lint("ac_missing.toml").find((i) => i.code === "spec/assert-needs-ac")!;
    expect(issue.message).toContain("needs an [ac] sweep block");
  });

  test("inf is rejected where the loader guards it: it would hang the frame loop", () => {
    const issue = lint("nonfinite_duration.toml").find((i) => i.code === "spec/non-finite")!;
    expect(issue.message).toContain("`duration_ms` must be a finite number (got inf)");
    expect(issue.span.start.line).toBe(lineOf("nonfinite_duration.toml", "duration_ms = inf"));
  });

  test("...and only where the loader guards it", () => {
    // `Spec::validate` and friends check finiteness field by field. A
    // peripheral's `temp_c` is NOT one of them, so flagging it would report an
    // error on a spec hauksbee-ci accepts. See parity.test.ts.
    expect(lint("accept_nonfinite_temp.toml")).toEqual([]);
    // The guarded set spans several tables.
    for (const [table, key] of [
      ["[[supply]]\nnet = \"V\"\nkind = \"ideal\"", "volts"],
      ["[[assert]]\nkind = \"voltage\"\nnet = \"V\"\nmin = 1", "after_ms"],
    ]) {
      const src = `board = "b.kicad_pcb"\n\n${table}\n${key} = nan\n`;
      const codes = lintSpec(src, schema).issues.map((i) => i.code);
      expect({ key, codes }).toEqual({ key, codes: expect.arrayContaining(["spec/non-finite"]) });
    }
  });

  test("nan in a guarded field: every comparison against it is false", () => {
    // The loader writes `!tol.is_finite() || tol <= 0.0 || tol > 1.0` in that
    // order for exactly this reason; a bare range test lets `nan` through.
    const toggle = [
      'board = "b.kicad_pcb"',
      "",
      "[[assert]]",
      'kind = "toggle"',
      'net = "LED"',
      "freq_hz = 1.0",
      "tolerance = nan",
    ].join("\n");
    const t = lintSpec(toggle, schema).issues.find(
      (i) => i.code === "spec/assert-tolerance-range"
    )!;
    expect(t.severity).toBe("error");

    // `min_critical` / `min_resolved` are guarded by a 0..=1 containment check,
    // which a non-finite value also fails. But ONLY inside the `model_coverage`
    // arm: on any other kind the loader ignores the keys entirely.
    for (const key of ["min_critical", "min_resolved"]) {
      for (const value of ["nan", "inf", "5.0", "-1.0"]) {
        const guarded = `board = "b.kicad_pcb"\n\n[[assert]]\nkind = "model_coverage"\n${key} = ${value}\n`;
        const found = lintSpec(guarded, schema).issues.find(
          (i) => i.code === "spec/assert-fraction-range"
        );
        expect({ key, value, flagged: !!found, severity: found?.severity }).toEqual({
          key,
          value,
          flagged: true,
          severity: "error",
        });

        // On a `voltage` assertion the same value is accepted by hauksbee-ci, so
        // an error here would be a false claim. See parity.test.ts.
        const ignored = [
          'board = "b.kicad_pcb"',
          "",
          "[[assert]]",
          'kind = "voltage"',
          'net = "VCC"',
          "min = 3.0",
          `${key} = ${value}`,
        ].join("\n");
        const errors = lintSpec(ignored, schema).issues.filter((i) => i.severity === "error");
        expect({ key, value, errors: errors.map((e) => e.code) }).toEqual({
          key,
          value,
          errors: [],
        });
      }
    }
  });

  test("[ensemble] with nothing to sample", () => {
    const src = [
      'board = "b.kicad_pcb"',
      "",
      "[ensemble]",
      "seeds = 8",
      "",
      "[[assert]]",
      'kind = "no_faults"',
    ].join("\n");
    const issue = lintSpec(src, schema).issues.find(
      (i) => i.code === "spec/ensemble-no-tolerances"
    )!;
    expect(issue.message).toContain("has nothing to sample");
    expect(issue.span.start.line).toBe(2);
  });

  test("corners mode does not compose with [fuzz]", () => {
    const src = [
      'board = "b.kicad_pcb"',
      "",
      "[[tolerance]]",
      'ref = "R*"',
      "percent = 10.0",
      "",
      "[fuzz]",
      "seeds = 4",
      "",
      "[ensemble]",
      'mode = "corners"',
      "",
      "[[assert]]",
      'kind = "no_faults"',
    ].join("\n");
    const codes = lintSpec(src, schema).issues.map((i) => i.code);
    expect(codes).toContain("spec/ensemble-corners-fuzz");
  });
});

describe("the inline-table spelling gets the same diagnostics, in the same places", () => {
  // `assert = [{ ... }]` has no `[[assert]]` header to hang a table-level
  // diagnostic on, so without a recorded span for the object element these would
  // all collapse onto line 0 and squiggle `board`.
  const at = (src: string, code: string) => {
    const issue = lintSpec(src, schema).issues.find((i) => i.code === code);
    expect({ code, found: !!issue }).toEqual({ code, found: true });
    return issue!;
  };

  test("a table-level assertion error lands on the inline table", () => {
    const issue = at(
      'board = "n.kicad_pcb"\nassert = [{ kind = "voltage", net = "V" }]\n',
      "spec/assert-needs-bound"
    );
    expect(issue.span.start).toEqual({ line: 1, col: 10 });
  });

  test("a peripheral written inline, too", () => {
    const src = [
      'board = "n.kicad_pcb"',
      'peripheral = [{ id = "B", type = "pushbutton" }]',
      "",
      "[[assert]]",
      'kind = "no_faults"',
    ].join("\n");
    expect(at(src, "spec/peripheral-needs-net").span.start.line).toBe(1);
  });

  test("and a field-level one still points at the field", () => {
    const src = 'board = "n.kicad_pcb"\nsupply = [{ net = "V", kind = "bench" }]\n\n[[assert]]\nkind = "no_faults"\n';
    const issue = at(src, "spec/supply-needs-volts");
    // On the `kind` value, which is what the message is about.
    expect(issue.span.start).toEqual({ line: 1, col: 30 });
  });
});

describe("broken TOML", () => {
  test("stops at the parse error rather than guessing semantics", () => {
    const result = lintSpec('board = "b\n\n[[assert]]\n', schema);
    expect(result.analysed).toBe(false);
    expect(result.issues).toHaveLength(1);
    expect(result.issues[0].code).toBe("spec/toml");
  });
});

describe("helpers", () => {
  test("didYouMean matches the Rust did_you_mean contract", () => {
    const kinds = ["voltage", "uart", "toggle", "no_faults"];
    expect(didYouMean("voltag", kinds)).toBe("voltage");
    expect(didYouMean("tooggle", kinds)).toBe("toggle");
    expect(didYouMean("frobnicate", kinds)).toBeUndefined();
    // An exact member is not a typo, so there is no hint.
    expect(didYouMean("voltage", kinds)).toBeUndefined();
  });

  test("levenshtein", () => {
    expect(levenshtein("", "abc")).toBe(3);
    expect(levenshtein("kitten", "sitting")).toBe(3);
  });

  test("normaliseKind folds the legacy spelling", () => {
    expect(normaliseKind("boot-coverage")).toBe("boot_coverage");
    expect(normaliseKind("voltage")).toBe("voltage");
    expect(normaliseKind(undefined)).toBeUndefined();
  });
});
