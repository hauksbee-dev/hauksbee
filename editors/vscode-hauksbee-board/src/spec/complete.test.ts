// Completions and hovers, all derived from the generated schema.

import { describe, expect, test } from "bun:test";
import * as fs from "fs";
import * as path from "path";
import { contextPathAt, cursorContext, hoverAt, keysOnScreen, suggest } from "./complete";
import { SpecSchema } from "./schemaModel";

const ROOT = path.join(__dirname, "..", "..");
const schema = new SpecSchema(
  JSON.parse(fs.readFileSync(path.join(ROOT, "schemas", "hauksbee-ci-spec.schema.json"), "utf8"))
);

/** A document plus the cursor, written with `|` where the caret sits. */
function at(source: string): { text: string; pos: { line: number; col: number } } {
  const lines = source.split("\n");
  const line = lines.findIndex((l) => l.includes("|"));
  const col = lines[line].indexOf("|");
  lines[line] = lines[line].replace("|", "");
  return { text: lines.join("\n"), pos: { line, col } };
}

const labels = (source: string, nets: string[] = [], boards: string[] = []) => {
  const { text, pos } = at(source);
  return suggest(text, pos, schema, { nets, boards }).map((s) => s.label);
};

describe("cursorContext", () => {
  test("a bare word at the start of a line is a key position", () => {
    const { text, pos } = at('[[assert]]\nkin|\nnet = "A"');
    const ctx = cursorContext(text, pos);
    expect(ctx.what).toBe("key");
    expect(ctx.schemaPath).toEqual(["assert"]);
    expect(ctx.span.start.col).toBe(0);
  });

  test("after `= \"` inside a string is a value position", () => {
    const { text, pos } = at('[[assert]]\nkind = "vol|');
    const ctx = cursorContext(text, pos);
    expect(ctx.what).toBe("value");
    expect(ctx.key).toBe("kind");
    expect(ctx.inString).toBe(true);
    expect(ctx.span.start.col).toBe('kind = "'.length);
  });

  test("an unclosed bracket is a table-header position", () => {
    const { text, pos } = at("[[sup|");
    const ctx = cursorContext(text, pos);
    expect(ctx.what).toBe("table-header");
  });

  test("a line inside an open multi-line array is a value position for its key", () => {
    const { text, pos } = at('[[peripheral]]\nnets = [\n  "|\n]');
    const ctx = cursorContext(text, pos);
    expect(ctx.what).toBe("value");
    expect(ctx.key).toBe("nets");
    expect(ctx.inString).toBe(true);
  });

  test("a CLOSED array does not swallow the following lines", () => {
    const { text, pos } = at('suppress_rail = [\n  "A",\n]\ndurat|');
    const ctx = cursorContext(text, pos);
    expect(ctx.what).toBe("key");
    expect(ctx.key).toBeUndefined();
  });

  test("inside a comment nothing is offered", () => {
    const { text, pos } = at("# net |");
    expect(cursorContext(text, pos).what).toBe("none");
  });
});

describe("contextPathAt", () => {
  test("finds the nearest header above, dotted paths included", () => {
    const text = ["[[peripheral]]", 'id = "B"', "", "[[peripheral.event]]", "t_ms = 1"].join("\n");
    expect(contextPathAt(text, 1)).toEqual(["peripheral"]);
    expect(contextPathAt(text, 4)).toEqual(["peripheral", "event"]);
    expect(contextPathAt(text, 0)).toEqual(["peripheral"]);
  });

  test("no header means the root table", () => {
    expect(contextPathAt('board = "b"', 0)).toEqual([]);
  });
});

describe("key completions", () => {
  test("root keys come from the schema, tables excluded", () => {
    const found = labels('board = "b.kicad_pcb"\n|');
    expect(found).toContain("duration_ms");
    expect(found).toContain("frame_ms");
    expect(found).toContain("firmware");
    expect(found).toContain("suppress_rail");
    // `board` is already written, so it is not offered again.
    expect(found).not.toContain("board");
    // Tables are offered through `[`, not as keys.
    expect(found).not.toContain("assert");
    expect(found).not.toContain("ac");
  });

  test("assertion keys, with the required ones sorted first", () => {
    const { text, pos } = at("[[assert]]\n|");
    const items = suggest(text, pos, schema, {});
    expect(items.map((i) => i.label)).toContain("deadline_ms");
    expect(items.find((i) => i.label === "kind")?.preferred).toBe(true);
    expect(items.find((i) => i.label === "min")?.preferred).toBeFalsy();
  });

  test("a key insert is a snippet: the cursor lands on the value", () => {
    const { text, pos } = at("[[supply]]\n|");
    const items = suggest(text, pos, schema, {});
    const item = (label: string) => items.find((i) => i.label === label)!;
    // A closed vocabulary becomes a CHOICE, so the next keystroke picks from the
    // list instead of spelling a token from memory.
    expect(item("kind").insertText).toBe('kind = "${1|ideal,bench,wall,usb,battery|}"');
    expect(item("chemistry").insertText).toBe(
      'chemistry = "${1|liion,lipo,alkaline,nimh,lifepo4,lfp|}"'
    );
    // Everything else places the cursor, with the schema default pre-filled where
    // there is one.
    expect(item("net").insertText).toBe('net = "$1"');
    expect(item("volts").insertText).toBe("volts = ${1:0}");
    // `cells` is bounded at >= 1, so that is what it is seeded with.
    expect(item("cells").insertText).toBe("cells = ${1:1}");
    for (const label of ["kind", "net", "volts"]) expect(item(label).snippet).toBe(true);

    // A seeded number satisfies the field's own bounds: `points = 0` would be an
    // error the instant it landed.
    const ac = at("[ac]\n|");
    const acItems = suggest(ac.text, ac.pos, schema, {});
    expect(acItems.find((i) => i.label === "fstart")?.insertText).toBe("fstart = ${1:1}");
    expect(acItems.find((i) => i.label === "points")?.insertText).toBe("points = ${1:1}");

    // ...and an editor that cannot expand snippets still gets plain text.
    const plain = suggest(text, pos, schema, { snippets: false });
    expect(plain.find((i) => i.label === "kind")?.insertText).toBe('kind = "ideal"');
    expect(plain.find((i) => i.label === "kind")?.snippet).toBeFalsy();
  });

  test("required keys come first, and an assertion's own fields before the rest", () => {
    const supply = labels("[[supply]]\n|");
    // `net` and `kind` are the two required ones.
    expect(supply.slice(0, 2)).toEqual(["kind", "net"]);

    // What a voltage assertion reads, IN THE ORDER a person fills it in, then
    // everything else. Alphabetising this tier would lead with `after_ms`.
    const voltage = labels('[[assert]]\nkind = "voltage"\n|');
    expect(voltage.slice(0, 5)).toEqual(["net", "min", "max", "after_ms", "name"]);
    expect(voltage.indexOf("amps")).toBeGreaterThan(voltage.indexOf("net"));
    expect(voltage.indexOf("deadline_ms")).toBeGreaterThan(voltage.indexOf("max"));

    // A different kind, a different set at the top.
    const toggle = labels('[[assert]]\nkind = "toggle"\n|');
    expect(toggle.slice(0, 5)).toEqual(["net", "freq_hz", "min_toggles", "tolerance", "name"]);

    // The kind is found from BELOW the cursor too: people write `kind` last.
    expect(labels('[[assert]]\n|\nkind = "hwtrace"').slice(0, 2)).toEqual(["trace", "name"]);
  });

  test("supplies and peripherals are kind-aware too", () => {
    // For a `usb` supply the field the loader REQUIRES is `usb`, and without this
    // it sits thirteenth of fourteen, unmarked, next to `chemistry` and `soc`.
    const usb = labels('[[supply]]\nnet = "VBUS"\nkind = "usb"\n|');
    expect(usb[0]).toBe("usb");
    expect(usb.indexOf("chemistry")).toBeGreaterThan(usb.indexOf("r_out_ohms"));

    const battery = labels('[[supply]]\nnet = "VBAT"\nkind = "battery"\n|');
    expect(battery.slice(0, 3)).toEqual(["chemistry", "cells", "capacity_mah"]);
    expect(battery.indexOf("usb")).toBeGreaterThan(battery.indexOf("soc"));

    // `[[peripheral]]` discriminates on `type`, not `kind`.
    const pot = labels('[[peripheral]]\nid = "P1"\ntype = "potentiometer"\n|');
    expect(pot.slice(0, 3)).toEqual(["a", "wiper", "b"]);
    const sink = labels('[[peripheral]]\nid = "T"\ntype = "vcd_sink"\n|');
    expect(sink.slice(0, 2)).toEqual(["nets", "vcd_path"]);
  });

  test("a field that belongs to another kind says so", () => {
    const { text, pos } = at('[[assert]]\nkind = "voltage"\n|');
    const items = suggest(text, pos, schema, {});
    expect(items.find((i) => i.label === "amps")?.detail).toContain(
      "not read by a voltage assertion"
    );
    expect(items.find((i) => i.label === "min")?.detail).not.toContain("not read by");
  });

  test("each key carries its doc comment as documentation", () => {
    const { text, pos } = at("[[supply]]\n|");
    const items = suggest(text, pos, schema, {});
    const chemistry = items.find((i) => i.label === "chemistry")!;
    expect(chemistry.documentation).toContain("Battery chemistry");
    expect(chemistry.detail).toContain("string");
  });
});

describe("value completions", () => {
  test("assertion kinds, canonical spellings only", () => {
    const found = labels('[[assert]]\nkind = "|"');
    expect(found).toContain("voltage");
    expect(found).toContain("boot_coverage");
    expect(found).toContain("model_coverage");
    // The legacy alias still validates, but must not be taught.
    expect(found).not.toContain("boot-coverage");
  });

  test("supply kinds, usb profiles and chemistries", () => {
    expect(labels('[[supply]]\nkind = "|"')).toEqual([
      "ideal",
      "bench",
      "wall",
      "usb",
      "battery",
    ]);
    expect(labels('[[supply]]\nusb = "|"')).toContain("5v1.5a");
    expect(labels('[[supply]]\nchemistry = "|"')).toContain("lifepo4");
  });

  test("peripheral types and stimulus waveforms", () => {
    expect(labels('[[peripheral]]\ntype = "|"')).toContain("spi_mcp3008");
    expect(labels('[[peripheral]]\nwaveform = "|"')).toEqual(["dc", "sine", "pwl", "noise"]);
  });

  test("dnp modes keep their kebab-case spelling", () => {
    expect(labels('dnp = "|"')).toEqual(["fit-except-links", "fit-all", "honour"]);
  });

  test("outside a string the value is quoted for you", () => {
    const { text, pos } = at("[[assert]]\nkind = |");
    const items = suggest(text, pos, schema, {});
    expect(items.find((i) => i.label === "voltage")?.insertText).toBe('"voltage"');
  });

  test("board nets are offered in net-valued strings only", () => {
    const nets = ["+5V", "GND", "DISP_EN"];
    expect(labels('[[assert]]\nnet = "|"', nets)).toEqual(nets);
    expect(labels('[[assert]]\nsupply_net = "|"', nets)).toEqual(nets);
    // `name` is a free label, not a net.
    expect(labels('[[assert]]\nname = "|"', nets)).toEqual([]);
  });

  test("references to things declared elsewhere in the same spec", () => {
    // The lint layer computes these very sets in order to REJECT a bad one, so
    // not offering them would be a strange place to stop.
    const spec = [
      'board = "b.kicad_pcb"',
      "",
      "[[scenario]]",
      'id = "inrush"',
      'part = "U5"',
      'profile = "esp32_boot_wifi"',
      "",
      "[[scenario]]",
      'id = "steady"',
      'part = "U5"',
      'profile = "idle"',
      "",
      "[[peripheral]]",
      'id = "BTN1"',
      'type = "pushbutton"',
      'net = "B"',
      "",
      "[[profile]]",
      'id = "idle"',
      "",
      "[[assert]]",
      'kind = "rail_window"',
      'net = "+5V"',
      "min = 4.5",
      "",
    ].join("\n");
    const atEnd = (line: string) => {
      const text = spec + line;
      const pos = { line: spec.split("\n").length - 1, col: line.length };
      return suggest(text, pos, schema, { text }).map((s) => s.label);
    };
    expect(atEnd('scenario = "')).toEqual(["inrush", "steady"]);
    expect(atEnd('id = "')).toEqual(["BTN1"]);

    // Inside the scenario block, `profile` offers the inline one that is declared.
    const inScenario = spec.replace('profile = "idle"', 'profile = "');
    const line = inScenario.split("\n").findIndex((l) => l.trim() === 'profile = "');
    expect(
      suggest(inScenario, { line, col: 'profile = "'.length }, schema, {
        text: inScenario,
      }).map((s) => s.label)
    ).toEqual(["idle"]);

    // A key with no cross-reference source is unaffected.
    expect(atEnd('name = "')).toEqual([]);
  });

  test("booleans", () => {
    expect(labels("[[assert]]\nexpect_trip = |")).toEqual(["true", "false"]);
  });
});

describe("table completions", () => {
  test("every spec table, arrays marked as repeatable, nested ones included", () => {
    const { text, pos } = at("[|");
    const items = suggest(text, pos, schema, {});
    const found = items.map((i) => i.label);
    expect(found).toContain("[[assert]]");
    expect(found).toContain("[[supply]]");
    expect(found).toContain("[ac]");
    expect(found).toContain("[ensemble]");
    expect(found).toContain("[[peripheral.event]]");
    expect(found).toContain("[[profile.segment]]");
    expect(found).toContain("[[decoupling.override]]");
    // Ordered by how often a spec contains each, then by depth: a nested block is
    // only writable once its parent exists, so it must not outrank `[[supply]]`.
    expect(found.slice(0, 4)).toEqual(["[[assert]]", "[[supply]]", "[[peripheral]]", "[[net_drive]]"]);
    expect(found.indexOf("[[peripheral.event]]")).toBeGreaterThan(found.indexOf("[decoupling]"));
    // The insert text is the WHOLE header, brackets included, and it replaces the
    // brackets the user typed. That is what makes an arity slip harmless: typing
    // `[[` and choosing `[ac]` must not leave the unparseable `[[ac]`.
    expect(items.find((i) => i.label === "[[assert]]")?.insertText).toBe(
      '[[assert]]\nkind = "${1|voltage,uart,toggle,no_faults,max_current,max_temp,peripheral,' +
        'rail_window,protection_trip,boot_coverage,phase_margin,ac_gain,hwtrace,' +
        'model_coverage|}"'
    );
    // ...and the scaffold is the table's REQUIRED keys, in Rust field order, so
    // choosing `[[supply]]` leaves a block to fill in rather than three field
    // names to remember.
    expect(items.find((i) => i.label === "[[supply]]")?.insertText).toBe(
      '[[supply]]\nnet = "$1"\nkind = "${2|ideal,bench,wall,usb,battery|}"'
    );
    // A table with no required keys is just its header.
    expect(items.find((i) => i.label === "[decoupling]")?.insertText).toBe("[decoupling]");
  });

  test("the completion replaces the brackets, whichever arity was typed", () => {
    // Both slips are the same mistake and both must be corrected, because the
    // result would not parse otherwise.
    for (const [typed, label, expected] of [
      ["[[", "[ac]", "[ac]"],
      ["[", "[[assert]]", "[[assert]]"],
      ["[[su", "[[supply]]", "[[supply]]"],
    ]) {
      const { text, pos } = at(`${typed}|`);
      const item = suggest(text, pos, schema, { snippets: false }).find((i) => i.label === label)!;
      expect({ typed, label, head: item.insertText.split("\n")[0] }).toEqual({
        typed,
        label,
        head: expected,
      });
      // The replaced range starts at the opening bracket, not after it.
      expect({ typed, col: item.replace!.start.col }).toEqual({ typed, col: 0 });
    }
  });

  test("every completion carries the range it replaces", () => {
    // VS Code's own word range stops at `+`, `/` and `.`, so completing `+5V`
    // over a typed `+5` would insert `++5V` and a board path would duplicate its
    // prefix. The provider must supply the range itself.
    const net = at('[[assert]]\nnet = "+5|"');
    const item = suggest(net.text, net.pos, schema, { nets: ["+5V"] })[0];
    expect(item.label).toBe("+5V");
    expect(item.replace).toEqual({
      start: { line: 1, col: 7 },
      end: { line: 1, col: 9 },
    });

    const board = at('board = "boards/bl|"');
    const path0 = suggest(board.text, board.pos, schema, {
      boards: ["boards/blinky.kicad_pcb"],
    })[0];
    expect(path0.replace).toEqual({
      start: { line: 0, col: 9 },
      end: { line: 0, col: 18 },
    });
  });

  test("boards in the workspace complete the one path that must exist on disk", () => {
    const boards = ["hardware/board.kicad_pcb", "../shared/panel.kicad_sch"];
    expect(labels('board = "|"', [], boards)).toEqual(boards);
    // Only for `board`: a firmware path is not a board.
    expect(labels('firmware = "|"', [], boards)).toEqual([]);
  });
});

describe("hovers", () => {
  test("a key hover shows type, bounds and the doc comment", () => {
    const { text, pos } = at("[[assert]]\ntoler|ance = 0.25");
    const hover = hoverAt(text, pos, schema)!;
    expect(hover.markdown).toContain("**tolerance**");
    expect(hover.markdown).toContain("> 0, <= 1");
    expect(hover.markdown).toContain("FRACTION in (0, 1]");
    expect(hover.span).toEqual({ start: { line: 1, col: 0 }, end: { line: 1, col: 9 } });
  });

  test("an enum key lists its vocabulary", () => {
    const { text, pos } = at('[[supply]]\nki|nd = "bench"');
    const hover = hoverAt(text, pos, schema)!;
    expect(hover.markdown).toContain("`ideal` | `bench` | `wall` | `usb` | `battery`");
  });

  test("a table header hover describes the table", () => {
    const { text, pos } = at("[[sce|nario]]");
    const hover = hoverAt(text, pos, schema)!;
    expect(hover.markdown).toContain("**[[scenario]]** (array of tables)");
    expect(hover.markdown).toContain("Transient scenarios");
  });

  test("hovering the legacy kind spelling says what the canonical one is", () => {
    const { text, pos } = at('[[assert]]\nkind = "boot-|coverage"');
    const hover = hoverAt(text, pos, schema)!;
    expect(hover.markdown).toContain("Accepted legacy spelling");
    expect(hover.markdown).toContain("canonical kind is `boot_coverage`");
  });

  test("an unknown key has nothing to say", () => {
    const { text, pos } = at("[[assert]]\nnonsen|se = 1");
    expect(hoverAt(text, pos, schema)).toBeUndefined();
  });
});

describe("multi-line strings are not spec source", () => {
  // A `[[sensor]]`'s inline `spec` is a whole nested TOML document. Its
  // `[sensor]` header and its keys belong to the sensor format, not to the CI
  // spec, so neither completion nor hover may reach inside.
  const nested = [
    'board = "b.kicad_pcb"',
    "",
    "[[sensor]]",
    'id = "U2"',
    'spec = """',
    "[sensor]",
    'bus = "i2c"',
    "address = 0x48",
    '"""',
    "",
    "[[assert]]",
    'kind = "no_faults"',
  ].join("\n");

  test("no completions inside the string", () => {
    // Line 5 is the string's own `[sensor]` header.
    expect(suggest(nested, { line: 5, col: 8 }, schema, {})).toEqual([]);
    expect(suggest(nested, { line: 6, col: 4 }, schema, {})).toEqual([]);
    // Outside it, everything still works.
    expect(suggest(nested, { line: 11, col: 0 }, schema, {}).length).toBeGreaterThan(0);
  });

  test("no hover inside the string", () => {
    expect(hoverAt(nested, { line: 5, col: 3 }, schema)).toBeUndefined();
    expect(hoverAt(nested, { line: 6, col: 1 }, schema)).toBeUndefined();
    // The `spec` key itself still hovers, on its own line.
    expect(hoverAt(nested, { line: 4, col: 1 }, schema)?.markdown).toContain("**spec**");
  });

  test("multi-line ARRAYS are still spec source: net completion works inside", () => {
    const src = [
      'board = "b.kicad_pcb"',
      "",
      "[[peripheral]]",
      'id = "t"',
      'type = "vcd_sink"',
      "nets = [",
      '  "',
      "]",
    ].join("\n");
    const found = suggest(src, { line: 6, col: 3 }, schema, { nets: ["CLK", "MOSI"] }).map(
      (s) => s.label
    );
    expect(found).toEqual(["CLK", "MOSI"]);
  });
});

describe("keysOnScreen", () => {
  test("collects the current table's keys and stops at the next header", () => {
    const { text, pos } = at(
      ['[[supply]]', 'net = "+5V"', "|", 'kind = "bench"', "", "[[assert]]", 'kind = "x"'].join("\n")
    );
    expect(keysOnScreen(text, pos).sort()).toEqual(["kind", "net"]);
  });
});
