// Unit tests for the pure CLI-output -> diagnostic mapping, run with
// `bun test`. Every fixture under test/fixtures/ is REAL captured output:
//
//   boot_gate_pass.junit.xml   hauksbee-ci run examples/boot_gate_pass.toml --junit ...   (exit 0)
//   boot_gate_fail.junit.xml   hauksbee-ci run examples/boot_gate_fail.toml --junit ...   (exit 1)
//   run_check_blinky.json      hauksbee run blinky --check --json
//   run_check_stormduino.json  hauksbee run stormduino --check --json
//
// plus the matching spec TOML sources for line mapping.

import { describe, expect, test } from "bun:test";
import * as fs from "fs";
import * as path from "path";
import {
  assertBlockLines,
  mapCiJUnit,
  mapEngineCheck,
  parseJUnit,
  severityFromEngine,
  xmlUnescape,
} from "./mapping";

const FIX = path.join(__dirname, "..", "test", "fixtures");
const read = (name: string) => fs.readFileSync(path.join(FIX, name), "utf8");

describe("severityFromEngine", () => {
  test("documented mapping", () => {
    expect(severityFromEngine("serious")).toBe("error");
    expect(severityFromEngine("warning")).toBe("warning");
    expect(severityFromEngine("medium")).toBe("warning");
    expect(severityFromEngine("note")).toBe("info");
    expect(severityFromEngine("info")).toBe("info");
    // Unknown future severities stay visible, non-gating.
    expect(severityFromEngine("mystery")).toBe("info");
  });
});

describe("parseJUnit (real hauksbee-ci output)", () => {
  test("passing run: 2 testcases, all pass, details unescaped", () => {
    const cases = parseJUnit(read("boot_gate_pass.junit.xml"));
    expect(cases.length).toBe(2);
    expect(cases[0].kind).toBe("boot-coverage");
    expect(cases[0].name).toBe("GATE_CTRL driven to >= 3 V within 20 ms of reset");
    expect(cases[0].outcome).toBe("pass");
    expect(cases[0].detail).toContain("driven to >= 3 V at 1.00 ms");
    expect(cases[1].kind).toBe("no_faults");
    expect(cases[1].outcome).toBe("pass");
  });

  test("failing run: failure message extracted and unescaped", () => {
    const cases = parseJUnit(read("boot_gate_fail.junit.xml"));
    expect(cases.length).toBe(1);
    expect(cases[0].outcome).toBe("fail");
    expect(cases[0].detail).toContain(
      "control net 'GATE_CTRL' was never driven to >= 3 V"
    );
  });

  test("invalid (analog abort) maps to outcome invalid", () => {
    // Synthetic but shape-exact: render_junit emits <error> for INVALID
    // assertions (crates/hauksbee-ci/src/report.rs).
    const xml = `<?xml version="1.0" encoding="UTF-8"?>
<testsuites name="hauksbee-ci" tests="1" failures="0" errors="1" time="0.1">
  <testsuite name="s" tests="1" failures="0" errors="1" time="0.1">
    <testcase classname="voltage" name="VDD in [4.9, 5.1] V">
      <error message="window overlaps a failed analog chunk">window overlaps a failed analog chunk</error>
    </testcase>
  </testsuite>
</testsuites>`;
    const cases = parseJUnit(xml);
    expect(cases[0].outcome).toBe("invalid");
  });

  test("xmlUnescape handles the emitter's five entities", () => {
    expect(xmlUnescape("&lt;&gt;&quot;&apos;&amp;")).toBe("<>\"'&");
  });
});

describe("assertBlockLines", () => {
  test("finds [[assert]] blocks at their real 0-based lines", () => {
    // grep -n '^\[\[assert\]\]' gives 1-based 25 (fail) and 27, 34 (pass).
    expect(assertBlockLines(read("boot_gate_fail.toml"))).toEqual([24]);
    expect(assertBlockLines(read("boot_gate_pass.toml"))).toEqual([26, 33]);
  });
});

describe("mapCiJUnit", () => {
  test("passing run: no diagnostics, GREEN summary", () => {
    const mapped = mapCiJUnit(read("boot_gate_pass.junit.xml"), read("boot_gate_pass.toml"));
    expect(mapped.diagnostics).toEqual([]);
    expect(mapped.summary.passed).toBe(true);
    expect(mapped.summary.label).toBe("2/2 assertions passed - GREEN");
  });

  test("failing run: one Error diagnostic on the [[assert]] line", () => {
    const mapped = mapCiJUnit(read("boot_gate_fail.junit.xml"), read("boot_gate_fail.toml"));
    expect(mapped.diagnostics.length).toBe(1);
    const d = mapped.diagnostics[0];
    expect(d.severity).toBe("error");
    expect(d.line).toBe(24); // the [[assert]] block, 0-based
    expect(d.code).toBe("ci/fail");
    expect(d.message).toContain("[FAIL] GATE_CTRL driven to >= 3 V within 20 ms of reset");
    expect(d.message).toContain("was never driven");
    expect(mapped.summary.passed).toBe(false);
    expect(mapped.summary.label).toBe("0/1 assertions passed - RED");
  });

  test("no spec text -> file-level (line 0) fallback", () => {
    const mapped = mapCiJUnit(read("boot_gate_fail.junit.xml"), null);
    expect(mapped.diagnostics[0].line).toBe(0);
  });

  test("block/testcase count mismatch -> file-level fallback (stale buffer)", () => {
    // boot_gate_pass spec has 2 blocks; fail XML has 1 testcase.
    const mapped = mapCiJUnit(read("boot_gate_fail.junit.xml"), read("boot_gate_pass.toml"));
    expect(mapped.diagnostics[0].line).toBe(0);
  });
});

describe("mapEngineCheck (real hauksbee run --check --json output)", () => {
  test("clean board (blinky): zero diagnostics, passed", () => {
    const mapped = mapEngineCheck(read("run_check_blinky.json"));
    expect(mapped.diagnostics).toEqual([]);
    expect(mapped.summary.passed).toBe(true);
    expect(mapped.summary.label).toBe("check clean");
  });

  test("stormduino: 11 findings mapped with severities and codes", () => {
    const mapped = mapEngineCheck(read("run_check_stormduino.json"));
    expect(mapped.diagnostics.length).toBe(11);
    // All severities are warning/info on this board (lint + usb_c).
    expect(mapped.summary.errors).toBe(0);
    expect(mapped.summary.passed).toBe(true); // no errors -> not gating
    expect(mapped.summary.warnings).toBeGreaterThan(0);

    const r9 = mapped.diagnostics.find((d) => d.message.startsWith("R9 is a R designator"));
    expect(r9).toBeDefined();
    expect(r9!.severity).toBe("warning");
    expect(r9!.code).toBe("lint/designator_footprint_mismatch");
    // Findings carry no source line, so they land at file level.
    expect(r9!.line).toBe(0);
    // The fix line is appended to the message.
    expect(r9!.message).toContain("fix: Make the reference/value and footprint agree");
  });

  test("error envelope ({ok:false}) becomes one file-level error", () => {
    const mapped = mapEngineCheck(JSON.stringify({ ok: false, error: "no MCU found" }));
    expect(mapped.diagnostics.length).toBe(1);
    expect(mapped.diagnostics[0].severity).toBe("error");
    expect(mapped.diagnostics[0].message).toBe("no MCU found");
  });

  test("garbage stdout becomes one file-level error, not a throw", () => {
    const mapped = mapEngineCheck("not json at all");
    expect(mapped.diagnostics.length).toBe(1);
    expect(mapped.summary.passed).toBe(false);
  });

  test("DRC shorts map to Error; version_warning downgrades to Info", () => {
    const base = {
      board: "b",
      bind: {},
      findings: [],
      drc: {
        clearance_rule_mm: 0.2,
        primitive_count: 10,
        shorts: [
          {
            net_a: "+5V",
            net_b: "GND",
            layer: "F.Cu",
            gap_mm: 0,
            loc_mm: [1.0, 2.0],
            severity: "serious",
          },
        ],
        violations: [
          {
            net_a: "A",
            net_b: "B",
            layer: "F.Cu",
            count: 3,
            below_count: 2,
            at_limit: false,
            min_gap_mm: 0.1,
            rule_mm: 0.2,
          },
        ],
        at_limit: [],
      },
    };
    const mapped = mapEngineCheck(JSON.stringify(base));
    expect(mapped.diagnostics.length).toBe(2);
    const short = mapped.diagnostics.find((d) => d.code === "drc/short")!;
    expect(short.severity).toBe("error");
    expect(short.message).toContain("copper short between '+5V' and 'GND'");
    const clearance = mapped.diagnostics.find((d) => d.code === "drc/clearance")!;
    expect(clearance.severity).toBe("warning");
    expect(mapped.summary.passed).toBe(false);

    // Unvalidated format: shorts may be phantom -> Information, non-gating.
    const withWarning = {
      ...base,
      drc: { ...base.drc, version_warning: "KiCad 10 board: copper extraction unvalidated" },
    };
    const downgraded = mapEngineCheck(JSON.stringify(withWarning));
    const s2 = downgraded.diagnostics.find((d) => d.code === "drc/short")!;
    expect(s2.severity).toBe("info");
    expect(s2.message).toContain("unvalidated");
  });
});
