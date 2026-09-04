// Quick fixes, tested by APPLYING them and re-linting: a fix that leaves the
// diagnostic in place, or introduces a new one, is not a fix. That is the whole
// property worth asserting, so every case here goes through `applied()`.

import { describe, expect, test } from "bun:test";
import * as fs from "fs";
import * as path from "path";
import { lintSpec, type SpecIssue } from "./lint";
import { fixesFor, type SpecFix } from "./quickfix";
import { SpecSchema } from "./schemaModel";

const ROOT = path.join(__dirname, "..", "..");
const schema = new SpecSchema(
  JSON.parse(fs.readFileSync(path.join(ROOT, "schemas", "hauksbee-ci-spec.schema.json"), "utf8"))
);

/** The diagnostic with the given code, and the fixes offered for it. */
function offered(text: string, code: string): { issue: SpecIssue; fixes: SpecFix[] } {
  const issue = lintSpec(text, schema).issues.find((i) => i.code === code);
  expect({ code, found: !!issue }).toEqual({ code, found: true });
  return { issue: issue!, fixes: fixesFor(text, issue!, schema) };
}

/** Apply an edit the way an editor would, by line/column. */
function apply(text: string, fix: SpecFix): string {
  const lines = text.split("\n");
  const { start, end } = fix.edit.span;
  const before = lines.slice(0, start.line).concat(lines[start.line].slice(0, start.col));
  const after = [lines[end.line].slice(end.col), ...lines.slice(end.line + 1)];
  return before.join("\n") + fix.edit.newText + after.join("\n");
}

/** Take the first fix for `code`, apply it, and report what is left. */
function applied(text: string, code: string): { title: string; result: string; codes: string[] } {
  const { fixes } = offered(text, code);
  expect(fixes.length).toBeGreaterThan(0);
  const result = apply(text, fixes[0]);
  return {
    title: fixes[0].title,
    result,
    codes: lintSpec(result, schema).issues.map((i) => i.code),
  };
}

const HEAD = 'board = "b.kicad_pcb"';

describe("renaming to the suggestion the message already carries", () => {
  test("a mistyped top-level key", () => {
    const src = [HEAD, "duraton_ms = 5", "", "[[assert]]", 'kind = "no_faults"'].join("\n");
    const { title, result, codes } = applied(src, "spec/unknown-key");
    expect(title).toBe("Change to `duration_ms`");
    expect(result).toContain("duration_ms = 5");
    expect(codes).toEqual([]);
  });

  test("a mistyped assertion kind, with the whole vocabulary as alternatives", () => {
    const src = [HEAD, "", "[[assert]]", 'kind = "voltag"', 'net = "V"', "min = 1.0"].join("\n");
    const { fixes } = offered(src, "spec/bad-enum");
    expect(fixes[0].title).toBe("Change to `voltage`");
    expect(fixes[0].preferred).toBe(true);
    // ...and every other kind is offered too, without repeating the suggestion.
    const titles = fixes.map((f) => f.title);
    expect(titles).toContain("Change to 'model_coverage'");
    expect(titles.filter((t) => t.includes("voltage"))).toHaveLength(1);

    const { result, codes } = applied(src, "spec/bad-enum");
    expect(result).toContain('kind = "voltage"');
    expect(codes).toEqual([]);
  });

  test("a mistyped supply kind", () => {
    const src = [HEAD, "", "[[supply]]", 'net = "V"', 'kind = "bnch"', "volts = 5.0", "", "[[assert]]", 'kind = "no_faults"'].join("\n");
    const { title, result, codes } = applied(src, "spec/bad-enum");
    expect(title).toBe("Change to `bench`");
    expect(result).toContain('kind = "bench"');
    expect(codes).toEqual([]);
  });
});

describe("adding a key the message says is missing", () => {
  test("the required `board`, inserted at the top", () => {
    const src = ["duration_ms = 5", "", "[[assert]]", 'kind = "no_faults"'].join("\n");
    const { title, result, codes } = applied(src, "spec/missing-key");
    expect(title).toBe('Add `board = ""`');
    expect(result.split("\n")[1]).toBe('board = ""');
    // Only the now-empty path is left to fill in, and that is not a diagnostic.
    expect(codes).toEqual([]);
  });

  test("a bench supply's `volts`, using the loader's own worked example", () => {
    const src = [HEAD, "", "[[supply]]", 'net = "+5V"', 'kind = "bench"', "", "[[assert]]", 'kind = "no_faults"'].join("\n");
    const { title, result, codes } = applied(src, "spec/supply-needs-volts");
    // The message says `volts = 3.3`; the fix uses exactly that rather than
    // inventing a number of its own.
    expect(title).toBe("Add `volts = 3.3`");
    expect(result).toContain("volts = 3.3");
    // Inserted directly after the block's last key, not after the assertion.
    expect(result.split("\n").indexOf("volts = 3.3")).toBe(5);
    expect(codes).toEqual([]);
  });

  test("a usb supply's profile and a battery's chemistry", () => {
    for (const [kind, expected] of [
      ["usb", 'Add `usb = "5v0.5a"`'],
      ["battery", 'Add `chemistry = "liion"`'],
    ]) {
      const src = [HEAD, "", "[[supply]]", 'net = "V"', `kind = "${kind}"`, "", "[[assert]]", 'kind = "no_faults"'].join("\n");
      const code = `spec/supply-needs-${kind === "usb" ? "usb" : "chemistry"}`;
      const { title, codes } = applied(src, code);
      expect({ kind, title }).toEqual({ kind, title: expected });
      expect({ kind, codes }).toEqual({ kind, codes: [] });
    }
  });

  test("a voltage assertion's missing bound: one fix per key, kind-filtered", () => {
    const src = [HEAD, "", "[[assert]]", 'kind = "voltage"', 'net = "VCC"'].join("\n");
    const { fixes } = offered(src, "spec/assert-needs-bound");
    expect(fixes.map((f) => f.title)).toEqual(["Add `min = 0`", "Add `max = 0`"]);
    const { result, codes } = applied(src, "spec/assert-needs-bound");
    expect(result).toContain("min = 0");
    expect(codes).toEqual([]);
  });

  test("a boot_coverage assertion, which needs two keys", () => {
    const src = [HEAD, "", "[[assert]]", 'kind = "boot_coverage"', 'net = "EN"'].join("\n");
    // Fixing one at a time is how a user works through it; both must land.
    let text = applied(src, "spec/assert-needs-bound").result;
    expect(text).toContain("min = 0");
    text = applied(text, "spec/assert-needs-bound").result;
    expect(text).toContain("deadline_ms = 0");
    expect(lintSpec(text, schema).issues).toEqual([]);
  });

  test("a toggle assertion offers only the keys a toggle reads", () => {
    const src = [HEAD, "", "[[assert]]", 'kind = "toggle"', 'net = "LED"'].join("\n");
    const { fixes } = offered(src, "spec/assert-needs-bound");
    const titles = fixes.map((f) => f.title);
    expect(titles).toEqual(["Add `freq_hz = 0`", "Add `min_toggles = 0`"]);
    // Not `min`/`max`, which a toggle ignores.
    expect(titles.join()).not.toContain("`min = ");
  });
});

describe("correcting a value the message has already worked out", () => {
  test("a toggle tolerance written as a percentage", () => {
    const src = [
      HEAD,
      "",
      "[[assert]]",
      'kind = "toggle"',
      'net = "LED"',
      "freq_hz = 1.0",
      "tolerance = 25.0",
    ].join("\n");
    const { title, result, codes } = applied(src, "spec/assert-tolerance-range");
    expect(title).toBe("Change to 0.25");
    expect(result).toContain("tolerance = 0.25");
    expect(codes).toEqual([]);
  });
});

describe("a required BLOCK is inserted as a block, not as an empty array", () => {
  test("a spec with no assertions", () => {
    // `assert = []` would trade one error for another and teach syntax nobody
    // writes; the fix inserts the block the loader is asking for.
    const src = [HEAD, "duration_ms = 5"].join("\n");
    const { title, result, codes } = applied(src, "spec/missing-key");
    expect(title).toBe("Add [[assert]]");
    expect(result).toContain("[[assert]]");
    expect(codes).toEqual([]);
    // ...and what it inserts lints clean on arrival. `kind = "voltage"` would
    // need a `net` and a bound too, so adding an assertion would trade one error
    // for three; `no_faults` needs nothing and means something.
    expect(result.trimEnd().split("\n").slice(-2)).toEqual(["[[assert]]", 'kind = "no_faults"']);
  });

  test("a missing `board` when the first line is a table header", () => {
    // The diagnostic sits on line 0, which is the header, so the table AT that
    // line is the wrong place to look the key up.
    const src = ["[[assert]]", 'kind = "no_faults"'].join("\n");
    const { title, result, codes } = applied(src, "spec/missing-key");
    expect(title).toBe('Add `board = ""`');
    // Written above the header, where a root key has to go.
    expect(result.split("\n")[0]).toBe('board = ""');
    expect(codes).toEqual([]);
  });
});

describe("restraint", () => {
  test("nothing is offered when the message names no answer", () => {
    // A value out of range: only the author knows what it should be.
    const src = [
      HEAD,
      "",
      "[[supply]]",
      'net = "V"',
      'kind = "battery"',
      'chemistry = "liion"',
      "soc = 1.5",
      "",
      "[[assert]]",
      'kind = "no_faults"',
    ].join("\n");
    const { fixes } = offered(src, "spec/out-of-range");
    expect(fixes).toEqual([]);
  });

  test("a garbled diagnostic yields no fix rather than a wrong one", () => {
    const issue: SpecIssue = {
      span: { start: { line: 0, col: 0 }, end: { line: 0, col: 1 } },
      severity: "error",
      message: "something the fixer has never seen",
      code: "spec/unknown-key",
    };
    expect(fixesFor(HEAD, issue, schema)).toEqual([]);
    expect(fixesFor(HEAD, { ...issue, code: "spec/toml" }, schema)).toEqual([]);
  });
});
