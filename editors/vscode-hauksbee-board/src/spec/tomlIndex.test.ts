// The position-aware TOML reader. Values matter, but SPANS are the reason this
// exists: every diagnostic, hover and completion is placed with them.

import { describe, expect, test } from "bun:test";
import * as fs from "fs";
import * as path from "path";
import { lookup, parseToml, type TomlTable } from "./tomlIndex";

const FIX = path.join(__dirname, "..", "..", "test", "fixtures", "spec");
const read = (name: string) => fs.readFileSync(path.join(FIX, name), "utf8");

describe("parseToml", () => {
  test("reads a real spec into the shape serde would", () => {
    const doc = parseToml(read("valid.toml"));
    expect(doc.errors).toEqual([]);
    expect(doc.root.name).toBe("power-up sanity");
    expect(doc.root.duration_ms).toBe(200);
    expect(doc.root.suppress_rail).toEqual(["ANALOG_VDD"]);

    const supplies = doc.root.supply as TomlTable[];
    expect(supplies).toHaveLength(2);
    expect(supplies[0]).toMatchObject({ net: "+5V", kind: "bench", volts: 5 });
    expect(supplies[1]).toMatchObject({ kind: "battery", chemistry: "liion", cells: 1, soc: 0.8 });

    // `[[peripheral.event]]` nests under the LAST `[[peripheral]]`.
    const peripherals = doc.root.peripheral as TomlTable[];
    const events = peripherals[0].event as TomlTable[];
    expect(events).toEqual([{ t_ms: 100, value: 1 }]);

    const asserts = doc.root.assert as TomlTable[];
    expect(asserts.map((a) => a.kind)).toEqual(["voltage", "boot_coverage", "rail_window"]);
  });

  test("spans point at the key and at the value, separately", () => {
    const text = ['name = "x"', "", "[[assert]]", 'kind = "voltage"'].join("\n");
    const doc = parseToml(text);
    const lk = lookup(doc);

    expect(lk.keySpan(["name"])).toEqual({
      start: { line: 0, col: 0 },
      end: { line: 0, col: 4 },
    });
    expect(lk.valueSpan(["name"])).toEqual({
      start: { line: 0, col: 7 },
      end: { line: 0, col: 10 },
    });
    // Array-of-tables entries are addressed by index.
    expect(lk.valueSpan(["assert", 0, "kind"])).toEqual({
      start: { line: 3, col: 7 },
      end: { line: 3, col: 16 },
    });
    expect(lk.tableSpan(["assert", 0])).toEqual({
      start: { line: 2, col: 0 },
      end: { line: 2, col: 10 },
    });
  });

  test("value forms a spec can contain", () => {
    const doc = parseToml(
      [
        "i = 42",
        "neg = -1_000",
        "hex = 0xff",
        "f = 1e6",
        "small = 1.5e-3",
        "yes = true",
        "inf_v = inf",
        "nan_v = nan",
        'lit = \'C:\\path\'',
        'esc = "a\\tb\\u0041"',
        "arr = [1, 2, 3]",
        "nested = [[1.0, 2.0], [3.0, 4.0]]",
        "inline = { a = 1, b = \"two\" }",
        'multi = """',
        "line one",
        'line two"""',
      ].join("\n")
    );
    expect(doc.errors).toEqual([]);
    expect(doc.root.i).toBe(42);
    expect(doc.root.neg).toBe(-1000);
    expect(doc.root.hex).toBe(255);
    expect(doc.root.f).toBe(1e6);
    expect(doc.root.small).toBe(0.0015);
    expect(doc.root.yes).toBe(true);
    expect(doc.root.inf_v).toBe(Infinity);
    expect(Number.isNaN(doc.root.nan_v as number)).toBe(true);
    expect(doc.root.lit).toBe("C:\\path");
    expect(doc.root.esc).toBe("a\tbA");
    expect(doc.root.arr).toEqual([1, 2, 3]);
    expect(doc.root.nested).toEqual([
      [1, 2],
      [3, 4],
    ]);
    expect(doc.root.inline).toEqual({ a: 1, b: "two" });
    expect(doc.root.multi).toBe("line one\nline two");
  });

  test("comments and blank lines are skipped, trailing comments too", () => {
    const doc = parseToml(
      ["# a spec", "", 'board = "b.kicad_pcb"  # relative to this file', "", "# done"].join("\n")
    );
    expect(doc.errors).toEqual([]);
    expect(doc.root.board).toBe("b.kicad_pcb");
  });

  test("multi-line arrays keep their span across lines", () => {
    const doc = parseToml(['nets = [\n  "A",\n  "B",\n]'].join("\n"));
    expect(doc.errors).toEqual([]);
    expect(doc.root.nets).toEqual(["A", "B"]);
    const span = lookup(doc).valueSpan(["nets"])!;
    expect(span.start.line).toBe(0);
    expect(span.end.line).toBe(3);
  });

  test("dotted keys land in the nested table", () => {
    const doc = parseToml(["[[sensor]]", 'id = "U2"', "inputs.temperature_c = 40.0"].join("\n"));
    expect(doc.errors).toEqual([]);
    const sensors = doc.root.sensor as TomlTable[];
    expect(sensors[0].inputs).toEqual({ temperature_c: 40 });
    expect(lookup(doc).valueSpan(["sensor", 0, "inputs", "temperature_c"])).toBeDefined();
  });

  test("a UTF-8 BOM is skipped, and does not shift any span", () => {
    // toml-rs strips a BOM, so a spec saved by an editor that emits one loads
    // fine. Treating it as a syntax error would silence every other diagnostic
    // in the file.
    const doc = parseToml('﻿board = "b.kicad_pcb"\nduration_ms = 5');
    expect(doc.errors).toEqual([]);
    expect(doc.root.board).toBe("b.kicad_pcb");
    // The BOM occupies column 0, so `board` starts at column 1.
    expect(lookup(doc).keySpan(["board"])).toEqual({
      start: { line: 0, col: 1 },
      end: { line: 0, col: 6 },
    });
    expect(lookup(doc).keySpan(["duration_ms"])?.start).toEqual({ line: 1, col: 0 });
  });

  test("multi-line strings whose content ends in their own quote character", () => {
    // TOML lets 4 or 5 closing quotes end a string whose content ends in quotes.
    const basic = parseToml('a = """say "hi""""');
    expect(basic.errors).toEqual([]);
    expect(basic.root.a).toBe('say "hi"');
    const literal = parseToml("b = '''it's ''fine'''''");
    expect(literal.errors).toEqual([]);
    expect(literal.root.b).toBe("it's ''fine''");
  });

  test("an unterminated string is one located error, not a crash", () => {
    const doc = parseToml('name = "oops\nboard = "b"');
    expect(doc.errors).toHaveLength(1);
    expect(doc.errors[0].message).toContain("unterminated string");
    expect(doc.errors[0].span.start.line).toBe(0);
  });

  // (the `parseToml` describe closes here; the rest are their own groups)
});

describe("documents toml-rs rejects, which must not read as clean", () => {
  // Each of these is exit 2 from the real binary (a TOML parse error). Accepting
  // them would put "✓ spec ok" on a file hauksbee-ci cannot even parse.
  const rejects = (src: string, needle: string) => {
    const doc = parseToml(src);
    expect({ src, errors: doc.errors.map((e) => e.message) }).not.toEqual({ src, errors: [] });
    expect(doc.errors[0].message).toContain(needle);
    return doc.errors[0];
  };

  test("a duplicate key, reported on the second one", () => {
    const err = rejects("duration_ms = 10\nduration_ms = 20\n", "duplicate key `duration_ms`");
    expect(err.span.start).toEqual({ line: 1, col: 0 });
  });

  test("a table defined twice", () => {
    rejects("[ac]\nfstart = 1.0\n\n[ac]\nfstop = 2.0\n", "defined more than once");
  });

  test("a table and an array of tables at the same path, either order", () => {
    rejects('[supply]\nnet = "A"\n\n[[supply]]\nnet = "B"\n', "array of tables");
    rejects('[[supply]]\nnet = "A"\n\n[supply]\nnet = "B"\n', "array of tables");
  });

  test("but a sub-table under the last array element is fine", () => {
    const doc = parseToml('[[peripheral]]\nid = "P"\n\n[peripheral.event]\nt_ms = 1\n');
    expect(doc.errors).toEqual([]);
  });

  test("leading zeros in a number", () => {
    rejects("duration_ms = 010\n", "leading zeros");
  });

  test("an underscore that does not sit between digits", () => {
    rejects("duration_ms = 1__00\n", "between digits");
    rejects("duration_ms = 0x1_\n", "between digits");
    rejects("duration_ms = _100\n", "between digits");
    // The legal form still parses.
    expect(parseToml("duration_ms = 1_000_000\n").root.duration_ms).toBe(1000000);
  });

  test("a trailing comma in an inline table, but not in an array", () => {
    rejects('supply = [{ net = "V", }]\n', "may not end with a comma");
    // Arrays DO allow one.
    expect(parseToml('suppress_rail = ["A",]\n').root.suppress_rail).toEqual(["A"]);
  });

  test("a table reopened after a dotted key created it", () => {
    rejects("ac.fstart = 1.0\n\n[ac]\nfstop = 2.0\n", "defined more than once");
  });

  test("a header for a path already given a value", () => {
    rejects('assert = [{ kind = "no_faults" }]\n\n[[assert]]\nkind = "no_faults"\n', "already defined as a value");
    rejects('ac = { fstart = 1.0 }\n\n[ac]\nfstop = 2.0\n', "already defined as a value");
  });

  test("a date-time, which no spec field accepts", () => {
    rejects("name = 1979-05-27T07:32:00Z\n", "no field in a hauksbee-ci spec takes a date");
  });

  test("an escape outside the Unicode range, which must not throw past the parser", () => {
    // `String.fromCodePoint` throws a RangeError here; that is not a parse error
    // and would escape parseToml, taking every diagnostic for the buffer with it.
    const err = rejects('board = "b\\U0011FFFF"\n', "not a Unicode scalar value");
    expect(err.span.start.line).toBe(0);
    // A lone surrogate is equally invalid.
    rejects('board = "b\\uD800"\n', "not a Unicode scalar value");
  });
});

describe("the reader only ever produces located errors, never a throw", () => {
  test("deeply nested values bail instead of overflowing the stack", () => {
    // `array()` and `inlineTable()` recurse per level, and a RangeError is not a
    // Bail: it would escape parseToml and take every diagnostic for the buffer
    // with it. Both a balanced and an unclosed run must be caught.
    for (const src of [
      `nets = ${"[".repeat(8000)}${"]".repeat(8000)}`,
      `nets = ${"[".repeat(8000)}`,
      `inline = ${"{ a = ".repeat(8000)}1${" }".repeat(8000)}`,
    ]) {
      const doc = parseToml(src);
      expect(doc.errors.length).toBeGreaterThan(0);
      expect(doc.errors[0].span.start.line).toBe(0);
    }
    // A spec's real nesting (`pwl = [[t, v], ...]`) is unaffected.
    expect(parseToml("pwl = [[0.0, 1.0], [1.0, 2.0]]").errors).toEqual([]);
  });
});

describe("int and float literals stay distinguishable", () => {
  test("a float literal is recorded, an integer one is not", () => {
    const doc = parseToml("a = 4\nb = 4.0\nc = 4e0\nd = inf\ne = 0x10\n");
    expect(doc.root).toMatchObject({ a: 4, b: 4, c: 4, e: 16 });
    // `4` and `4.0` are the same double, and serde tells them apart: a float in
    // a `u32` field is rejected. The reader records which literals were floats.
    expect([...doc.floatLiterals].sort()).toEqual(["b", "c", "d"]);
  });
});

describe("values written inside inline tables are indexed too", () => {
  test("an inline table's members get their own key and value spans", () => {
    const src = 'supply = [{ net = "V", kind = "bench" }]';
    const lk = lookup(parseToml(src));
    // Without this, a diagnostic about the supply's `kind` would fall back to
    // the top of the file, which is the one shape where it is unusable.
    expect(lk.valueSpan(["supply", 0, "kind"])).toEqual({
      start: { line: 0, col: 30 },
      end: { line: 0, col: 37 },
    });
    expect(lk.keySpan(["supply", 0, "net"])).toEqual({
      start: { line: 0, col: 12 },
      end: { line: 0, col: 15 },
    });
  });

  test("array elements are addressed by index", () => {
    const lk = lookup(parseToml('suppress_rail = ["A", "B"]'));
    expect(lk.valueSpan(["suppress_rail", 1])).toEqual({
      start: { line: 0, col: 22 },
      end: { line: 0, col: 25 },
    });
  });
});

describe("parseToml", () => {
  test("contextAt finds the table a line belongs to", () => {
    const doc = parseToml(read("valid.toml"));
    const lk = lookup(doc);
    const lines = read("valid.toml").split("\n");
    const kindLine = lines.findIndex((l) => l.includes('kind = "boot_coverage"'));
    expect(lk.contextAt(kindLine).schemaPath).toEqual(["assert"]);
    const soc = lines.findIndex((l) => l.startsWith("soc = "));
    expect(lk.contextAt(soc).instancePath).toEqual(["supply", 1]);
  });
});


describe("line endings and control characters", () => {
  test("CRLF throughout, comments included", () => {
    const doc = parseToml(
      '# a spec\r\nboard = "b.kicad_pcb"  # relative\r\n\r\n[[assert]]\r\nkind = "no_faults"\r\n'
    );
    expect(doc.errors).toEqual([]);
    expect(doc.root.board).toBe("b.kicad_pcb");
    expect(lookup(doc).tableSpan(["assert", 0]).start.line).toBe(3);
  });

  test("a lone CR, and other raw control characters, are rejected like toml-rs does", () => {
    // A file with mixed line endings is the realistic way to hit this.
    for (const src of ['name = "a\rb"\n', "# a\rcomment\n", 'name = "a\u0001b"\n', "name = 'a\rb'\n"]) {
      const doc = parseToml(src);
      expect({ src: JSON.stringify(src), errors: doc.errors.length }).not.toEqual({
        src: JSON.stringify(src),
        errors: 0,
      });
      expect(doc.errors[0].message).toContain("control character");
    }
    // A tab is the one control character TOML allows.
    expect(parseToml('name = "a\tb"\n').errors).toEqual([]);
  });
});
