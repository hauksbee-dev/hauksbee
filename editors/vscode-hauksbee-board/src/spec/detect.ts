// Deciding whether a `.toml` buffer is a hauksbee-ci spec.
//
// This has to be right in both directions: a spec that goes undetected gets no
// help, and a Cargo.toml or pyproject.toml wrongly detected gets a wall of
// nonsense diagnostics. So content wins over filename: only a hauksbee-ci spec
// has both a `board = "..."` key and an `[[assert]]` table.
//
//  * "spec":      certain. Lint it, run the loader against it.
//  * "candidate": a file POSITIONED like a spec (named `hauksbee*.toml`, or any
//                  `.toml` under a `ci/` directory) whose content is not yet
//                  conclusive. Offer completions and hovers, but pass no
//                  judgement at all: a half-typed file is not a broken file,
//                  and a `ci/renovate.toml` is not ours to grade.
//  * "none":      leave it alone entirely.

import { parseToml } from "./tomlIndex";

export type SpecConfidence = "spec" | "candidate" | "none";

const NAMED_LIKE_A_SPEC = /(^|[/\\])(hauksbee[^/\\]*|[^/\\]*\.hauksbee)\.toml$/;
const UNDER_CI_DIR = /(^|[/\\])ci[/\\][^/\\]+\.toml$/;

export function specConfidence(fsPath: string, text: string): SpecConfidence {
  if (!fsPath.endsWith(".toml")) return "none";
  // Cheap screen first, so nothing is parsed unnecessarily.
  const hasAssert = /^\s*\[\[assert\]\]/m.test(text);
  const hasBoard = /^\s*board\s*=/m.test(text);
  // Then confirm against the PARSE TREE, not the raw text. A document that
  // merely quotes a spec inside a multi-line string matches both regexes, and
  // linting it would fill the Problems panel with errors about a file
  // hauksbee-ci never reads. (This repo has four such files under qc/scenarios.)
  if (hasAssert && hasBoard) {
    const doc = parseToml(text);
    if (typeof doc.root.board === "string" && Array.isArray(doc.root.assert)) return "spec";
    // Broken TOML that looks like a spec is still a spec being edited: the
    // caller reports the parse error and stops there.
    if (doc.errors.length > 0) return "spec";
  }
  const positioned = NAMED_LIKE_A_SPEC.test(fsPath) || UNDER_CI_DIR.test(fsPath);
  if (!positioned) return "none";
  // Somewhere a spec would live, but not (yet) recognisably one. Offering
  // completions costs nothing if it turns out to be someone else's file;
  // diagnostics would be actively wrong, so callers must not produce any.
  return "candidate";
}

/** Glob-ish check used to decide which files to watch, before reading them. */
export function looksPositioned(fsPath: string): boolean {
  return NAMED_LIKE_A_SPEC.test(fsPath) || UNDER_CI_DIR.test(fsPath);
}
