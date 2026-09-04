// Reattaching positions to the loader's own messages. Every .stderr fixture is
// REAL captured output from `hauksbee-ci run <spec> --quiet` against the spec of
// the same name in test/fixtures/spec/.

import { describe, expect, test } from "bun:test";
import * as fs from "fs";
import * as path from "path";
import { lintSpec } from "./lint";
import { mapLoaderStderr } from "./loaderDiag";
import { SpecSchema } from "./schemaModel";
import { parseToml } from "./tomlIndex";

const ROOT = path.join(__dirname, "..", "..");
const SPECS = path.join(ROOT, "test", "fixtures", "spec");
const LOADER = path.join(ROOT, "test", "fixtures", "loader");
const schema = new SpecSchema(
  JSON.parse(fs.readFileSync(path.join(ROOT, "schemas", "hauksbee-ci-spec.schema.json"), "utf8"))
);

const stderrOf = (name: string) => fs.readFileSync(path.join(LOADER, `${name}.stderr`), "utf8");
const specOf = (name: string) => fs.readFileSync(path.join(SPECS, `${name}.toml`), "utf8");

/** Map a captured failure onto the spec it came from. */
function map(name: string, specName = name) {
  const text = specOf(specName);
  return {
    issues: mapLoaderStderr(stderrOf(name), lintSpec(text, schema).doc),
    lineOf: (needle: string) => text.split("\n").findIndex((l) => l.includes(needle)),
  };
}

describe("TOML parse errors", () => {
  test("carry a line, a column and a caret width, and drop the ASCII art", () => {
    const issues = mapLoaderStderr(stderrOf("broken_string"), parseToml(""));
    expect(issues).toHaveLength(1);
    expect(issues[0]).toMatchObject({
      code: "spec/toml",
      severity: "error",
      message: "invalid basic string",
    });
    // "at line 1, column 13" -> 0-based (0, 12), one caret wide.
    expect(issues[0].span).toEqual({ start: { line: 0, col: 12 }, end: { line: 0, col: 13 } });
  });

  test("serde's unknown-field rejection keeps its full expected-key list", () => {
    const { issues, lineOf } = map("unknown_key");
    expect(issues).toHaveLength(1);
    expect(issues[0].message).toStartWith("unknown field `duraton_ms`, expected one of `name`");
    expect(issues[0].span.start).toEqual({ line: lineOf("duraton_ms"), col: 0 });
    // The caret run under the token gives the end column.
    expect(issues[0].span.end.col).toBe("duraton_ms".length);
  });
});

describe("invalid spec", () => {
  test("a supply error lands on the field the message names", () => {
    const { issues, lineOf } = map("supply_no_volts");
    expect(issues).toHaveLength(1);
    expect(issues[0].code).toBe("spec/loader");
    // The loader's words, verbatim: nothing is paraphrased.
    expect(issues[0].message).toBe(
      "supply on '+5V': `bench` needs an explicit `volts`; add the rail's real voltage " +
        "(e.g. `volts = 3.3`). Nothing is assumed: a wrong guess here would fabricate faults " +
        "on a healthy board"
    );
    expect(issues[0].span.start.line).toBe(lineOf('kind = "bench"'));
  });

  test("an unknown assertion kind lands on the kind value, hint intact", () => {
    const { issues, lineOf } = map("typo_kind");
    expect(issues).toHaveLength(1);
    expect(issues[0].message).toContain("unknown assertion kind 'voltag' (did you mean 'voltage'?)");
    expect(issues[0].span.start.line).toBe(lineOf("voltag"));
  });
});

describe("board-dependent failures", () => {
  test("unknown nets land on the net reference, with the suggestions", () => {
    const { issues, lineOf } = map("unknown_net");
    expect(issues).toHaveLength(1);
    expect(issues[0].code).toBe("spec/unknown-net");
    expect(issues[0].message).toBe(
      "net 'VCCC' (referenced in assert) does not exist on the board; did you mean: ADC0?"
    );
    expect(issues[0].span.start.line).toBe(lineOf('net = "VCCC"'));
  });

  test("a missing board is information, not an error: the spec itself is fine", () => {
    const { issues, lineOf } = map("board_missing", "valid");
    expect(issues).toHaveLength(1);
    expect(issues[0].code).toBe("spec/board-missing");
    expect(issues[0].severity).toBe("info");
    expect(issues[0].span.start.line).toBe(lineOf("board ="));
  });
});

describe("degenerate input", () => {
  test("empty stderr is no diagnostics", () => {
    expect(mapLoaderStderr("", parseToml(""))).toEqual([]);
    expect(mapLoaderStderr("   \n", parseToml(""))).toEqual([]);
  });

  test("an unrecognised message still surfaces, at file level", () => {
    const issues = mapLoaderStderr("hauksbee-ci: x.toml: something new\n", parseToml(""));
    expect(issues).toHaveLength(1);
    expect(issues[0].message).toBe("something new");
    expect(issues[0].span.start).toEqual({ line: 0, col: 0 });
  });
});

describe("failures that are about the machine, not the spec", () => {
  // `SpecError::Invalid` carries these too, so they arrive with the same exit 2
  // as a genuine spec error. No edit to the file will fix one, so calling it a
  // spec error blames the author for the toolchain.
  const environmental = [
    "hauksbee-ci: ci/x.toml: invalid spec: building engine: this build of hauksbee-engine was " +
      "compiled without the `avr` feature; rebuild with --features avr to run AVR firmware",
    "hauksbee-ci: ci/x.toml: invalid spec: this build of hauksbee was compiled without the " +
      "`renode` feature, so it cannot run this MCU",
  ];

  test("are Information, and say so", () => {
    for (const stderr of environmental) {
      const issues = mapLoaderStderr(stderr, parseToml(""));
      expect(issues).toHaveLength(1);
      expect(issues[0].code).toBe("spec/environment");
      expect(issues[0].severity).toBe("info");
      expect(issues[0].message).toContain("not about this spec");
    }
  });

  test("while a genuine spec error stays an error", () => {
    const issues = mapLoaderStderr(stderrOf("supply_no_volts"), parseToml(""));
    expect(issues[0].code).toBe("spec/loader");
    expect(issues[0].severity).toBe("error");
  });
});
