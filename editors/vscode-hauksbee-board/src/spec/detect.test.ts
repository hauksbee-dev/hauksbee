// Spec detection has to be right in both directions: a missed spec gets no
// help, and a wrongly-detected Cargo.toml gets a wall of nonsense.

import { describe, expect, test } from "bun:test";
import { execFileSync } from "child_process";
import * as fs from "fs";
import * as path from "path";
import { looksPositioned, specConfidence } from "./detect";

const SPEC = ['board = "hw/board.kicad_pcb"', "", "[[assert]]", 'kind = "no_faults"'].join("\n");

describe("specConfidence", () => {
  test("board + [[assert]] is a spec, wherever it lives", () => {
    expect(specConfidence("/w/anything.toml", SPEC)).toBe("spec");
    expect(specConfidence("/w/ci/power-up.toml", SPEC)).toBe("spec");
    expect(specConfidence("/w/deeply/nested/x.toml", SPEC)).toBe("spec");
  });

  test("a positioned file that is still being written is a candidate", () => {
    expect(specConfidence("/w/ci/power-up.toml", 'board = "b.kicad_pcb"\n')).toBe("candidate");
    expect(specConfidence("/w/hauksbee.toml", "")).toBe("candidate");
    expect(specConfidence("/w/power.hauksbee.toml", "# nothing yet\n")).toBe("candidate");
  });

  test("other people's TOML is left alone", () => {
    const cargo = ['[package]', 'name = "x"', 'version = "0.1.0"'].join("\n");
    expect(specConfidence("/w/Cargo.toml", cargo)).toBe("none");
    expect(specConfidence("/w/pyproject.toml", "[build-system]\n")).toBe("none");
    // Even a Cargo.toml in a ci/ directory: it has no `board`.
    expect(specConfidence("/w/ci/Cargo.toml", cargo)).toBe("candidate");
    expect(specConfidence("/w/board.json", SPEC)).toBe("none");
  });

  test("a sensor spec_file is not a CI spec", () => {
    const sensor = ["[sensor]", 'bus = "i2c"', "address = 0x48"].join("\n");
    expect(specConfidence("/w/sensors/lm75.toml", sensor)).toBe("none");
  });

  test("a document that merely QUOTES a spec is not a spec", () => {
    // Both screening regexes match inside a multi-line string, so the decision
    // has to be confirmed against the parse tree. Four files in this repo
    // (qc/scenarios/*/scenario.toml) are exactly this shape, and linting them
    // produced seven hard errors each about a file hauksbee-ci never reads.
    const q = String.fromCharCode(34).repeat(3);
    const quoting = [
      'title = "a doc that quotes a spec"',
      "",
      "[[step]]",
      `text = ${q}`,
      'board = "b.kicad_pcb"',
      "[[assert]]",
      'kind = "no_faults"',
      q,
    ].join("\n");
    expect(specConfidence("/w/scenario.toml", quoting)).toBe("none");
    // ...and the real thing still is one.
    expect(specConfidence("/w/scenario.toml", SPEC)).toBe("spec");
  });

  test("a spec whose TOML is currently broken is still a spec", () => {
    // Otherwise the parse error itself would be suppressed the moment a quote
    // went missing, which is exactly when it is most wanted.
    const broken = 'board = "b\n\n[[assert]]\nkind = "no_faults"\n';
    expect(specConfidence("/w/x.toml", broken)).toBe("spec");
  });

  test("every TOML in this repository is classified as its author intended", () => {
    // The regression this closes: a wall of errors in somebody else's file.
    const repo = path.resolve(__dirname, "..", "..", "..", "..");
    const files = execFileSync("git", ["ls-files", "-z", "--", "*.toml"], {
      cwd: repo,
      encoding: "utf8",
      maxBuffer: 32 * 1024 * 1024,
    })
      .split("\0")
      .filter((f) => f !== "");
    expect(files.length).toBeGreaterThan(50);

    const specs: string[] = [];
    for (const rel of files) {
      let text: string;
      try {
        text = fs.readFileSync(path.join(repo, rel), "utf8");
      } catch {
        continue;
      }
      if (specConfidence(path.join(repo, rel), text) === "spec") specs.push(rel);
    }
    // Every shipped example spec is detected...
    const examples = files.filter((f) => f.startsWith("crates/hauksbee-ci/examples/"));
    expect(specs.filter((f) => f.startsWith("crates/hauksbee-ci/examples/")).sort()).toEqual(
      examples.sort()
    );
    // ...and nothing that is not a spec is.
    expect(
      specs.filter((f) => /(^|\/)(Cargo|deny|clippy|rustfmt|rust-toolchain)\.toml$/.test(f))
    ).toEqual([]);
    expect(specs.filter((f) => f.startsWith("qc/scenarios/"))).toEqual([]);
  });

  test("windows path separators", () => {
    expect(specConfidence("C:\\w\\ci\\power.toml", SPEC)).toBe("spec");
    expect(looksPositioned("C:\\w\\ci\\power.toml")).toBe(true);
    expect(looksPositioned("C:\\w\\src\\power.toml")).toBe(false);
  });
});
