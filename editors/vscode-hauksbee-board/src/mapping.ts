// Pure mapping layer: hauksbee CLI output -> editor-agnostic diagnostics.
//
// Nothing in this file imports `vscode`, so the whole mapping is unit-testable
// with `bun test` against real captured CLI output (test/fixtures/).
//
// Two input formats, because the two binaries expose different machine
// surfaces (verified against the CLIs at the time of writing):
//
//  * `hauksbee run <board> --check --json`  -> a JsonReport object with a
//    `findings` array (lint + SI + USB-C) and a `drc` section (shorts +
//    clearance groups). Findings carry nets/refs/severity but NO source line
//    numbers.
//  * `hauksbee-ci run <spec> --junit <out.xml>` -> JUnit XML. hauksbee-ci has
//    no `--json` flag; the JUnit XML is its stable machine format. One
//    `<testcase>` per assertion, in SPEC ORDER (the runner evaluates
//    `spec.asserts` in order), which is what lets us map result N back to the
//    Nth `[[assert]]` block in the TOML.
//
// Severity mapping (documented contract):
//
//   engine finding severity   VS Code severity
//   ------------------------  ----------------
//   "serious"                 Error
//   "warning" / "medium"      Warning
//   "note" / "info"           Information
//   DRC short                 Error   (downgraded to Information when the
//                                      report carries a version_warning: the
//                                      copper extraction is unvalidated and
//                                      the shorts may be phantom)
//   DRC clearance violation   Warning
//   DRC at-limit group        Information
//
//   hauksbee-ci assertion     VS Code severity
//   ------------------------  ----------------
//   <failure> (FAIL)          Error
//   <error>   (INVALID)       Error   (the run is invalid-for-analysis and
//                                      gates CI with exit 3; softer than
//                                      Error would misrepresent the gate)
//   pass                      no diagnostic

export type Sev = "error" | "warning" | "info";

/** An editor-agnostic diagnostic: 0-based line, severity, message. */
export interface MappedDiagnostic {
  line: number;
  severity: Sev;
  message: string;
  /** Stable code, e.g. "lint/designator_footprint_mismatch" or "ci/fail". */
  code: string;
}

/** Status-bar summary of one run. */
export interface RunSummary {
  /** True when nothing gates: no errors (CI: every assertion passed). */
  passed: boolean;
  errors: number;
  warnings: number;
  infos: number;
  /** Total findings / assertions considered. */
  total: number;
  /** One human line, e.g. "3/4 assertions passed" or "2 findings". */
  label: string;
}

export interface MappedRun {
  diagnostics: MappedDiagnostic[];
  summary: RunSummary;
}

// ─────────────────────────────────────────────────────────────────────────────
// Engine `run --check --json`
// ─────────────────────────────────────────────────────────────────────────────

interface EngineFinding {
  check: string;
  kind: string;
  severity: string;
  nets: string[];
  refs: string[];
  actionable: boolean;
  message: string;
  plain: string;
  fix?: string | null;
}

interface DrcShort {
  net_a: string;
  net_b: string;
  layer: string;
  gap_mm: number;
  loc_mm: [number, number];
  severity: string;
}

interface DrcGroup {
  net_a: string;
  net_b: string;
  layer: string;
  count: number;
  below_count: number;
  at_limit: boolean;
  min_gap_mm: number;
  rule_mm: number;
}

interface EngineReport {
  board?: string;
  findings?: EngineFinding[];
  drc?: {
    shorts?: DrcShort[];
    violations?: DrcGroup[];
    at_limit?: DrcGroup[];
    version_warning?: string;
  };
  // `run --json` error envelope: { ok: false, error: "..." }
  ok?: boolean;
  error?: string;
}

export function severityFromEngine(s: string): Sev {
  switch (s) {
    case "serious":
      return "error";
    case "warning":
    case "medium":
      return "warning";
    default:
      // "note", "info", and anything future stays visible but non-gating.
      return "info";
  }
}

/**
 * Locate a finding inside a `.board` source, best effort. Engine findings
 * carry no line numbers, but they do carry refs and nets, and the board DSL
 * declares those on greppable lines (`comp R9 ...`, `net "OUT"`). Returns a
 * 0-based line, or 0 (file level) when nothing matches or no source given.
 */
export function locateInBoard(
  boardText: string | null,
  refs: string[],
  nets: string[]
): number {
  if (!boardText) return 0;
  const lines = boardText.split(/\r?\n/);
  for (const ref of refs) {
    const re = new RegExp(`^\\s*comp\\s+${escapeRe(ref)}\\b`);
    const i = lines.findIndex((l) => re.test(l));
    if (i >= 0) return i;
  }
  for (const net of nets) {
    const needle = `net "${net}"`;
    const i = lines.findIndex((l) => l.includes(needle));
    if (i >= 0) return i;
  }
  return 0;
}

function escapeRe(s: string): string {
  return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

/**
 * Map `hauksbee run <board> --check --json` stdout to diagnostics.
 *
 * @param jsonText raw stdout
 * @param boardText the checked file's text when it is a `.board` DSL file
 *                  (enables ref/net line location); null for other formats,
 *                  which get file-level (line 0) diagnostics.
 */
export function mapEngineCheck(
  jsonText: string,
  boardText: string | null
): MappedRun {
  let report: EngineReport;
  try {
    report = JSON.parse(jsonText);
  } catch (e) {
    return singleError(`hauksbee produced unparseable JSON: ${e}`);
  }
  if (report.ok === false) {
    return singleError(report.error ?? "hauksbee reported an error");
  }

  const diags: MappedDiagnostic[] = [];

  for (const f of report.findings ?? []) {
    const sev = severityFromEngine(f.severity);
    let message = f.message;
    if (f.fix) message += `\nfix: ${f.fix}`;
    diags.push({
      line: locateInBoard(boardText, f.refs ?? [], f.nets ?? []),
      severity: sev,
      message,
      code: `${f.check}/${f.kind}`,
    });
  }

  const drc = report.drc;
  if (drc) {
    // On an unvalidated board format the extractor itself downgrades shorts;
    // mirror that: version_warning present -> Information, else Error.
    const shortSev: Sev = drc.version_warning ? "info" : "error";
    for (const s of drc.shorts ?? []) {
      diags.push({
        line: locateInBoard(boardText, [], [s.net_a, s.net_b]),
        severity: shortSev,
        message:
          `copper short between '${s.net_a}' and '${s.net_b}' on ${s.layer}` +
          ` (gap ${s.gap_mm} mm at [${s.loc_mm[0]}, ${s.loc_mm[1]}] mm)` +
          (drc.version_warning ? `\n${drc.version_warning}` : ""),
        code: "drc/short",
      });
    }
    for (const g of drc.violations ?? []) {
      diags.push({
        line: locateInBoard(boardText, [], [g.net_a, g.net_b]),
        severity: "warning",
        message:
          `clearance below rule between '${g.net_a}' and '${g.net_b}' on ${g.layer}: ` +
          `${g.count} spot(s), tightest gap ${g.min_gap_mm} mm (rule ${g.rule_mm} mm)`,
        code: "drc/clearance",
      });
    }
    for (const g of drc.at_limit ?? []) {
      diags.push({
        line: locateInBoard(boardText, [], [g.net_a, g.net_b]),
        severity: "info",
        message:
          `exactly at minimum clearance (no margin) between '${g.net_a}' and '${g.net_b}' ` +
          `on ${g.layer}: ${g.count} spot(s) at ${g.min_gap_mm} mm (rule ${g.rule_mm} mm)`,
        code: "drc/at_limit",
      });
    }
  }

  const errors = diags.filter((d) => d.severity === "error").length;
  const warnings = diags.filter((d) => d.severity === "warning").length;
  const infos = diags.filter((d) => d.severity === "info").length;
  const label =
    diags.length === 0
      ? "check clean"
      : `${diags.length} finding(s): ${errors} error, ${warnings} warning, ${infos} info`;
  return {
    diagnostics: diags,
    summary: {
      passed: errors === 0,
      errors,
      warnings,
      infos,
      total: diags.length,
      label,
    },
  };
}

// ─────────────────────────────────────────────────────────────────────────────
// hauksbee-ci JUnit XML
// ─────────────────────────────────────────────────────────────────────────────

export interface CiCase {
  /** Assertion label (JUnit testcase name). */
  name: string;
  /** Assertion kind (JUnit classname), e.g. "voltage", "boot-coverage". */
  kind: string;
  outcome: "pass" | "fail" | "invalid";
  detail: string;
}

/**
 * Parse hauksbee-ci's JUnit XML. This is a minimal parser for OUR OWN
 * emitter's known, flat shape (crates/hauksbee-ci/src/report.rs
 * render_junit): testcases with one optional <failure>/<error>/<system-out>
 * child, all attribute values XML-escaped. Not a general XML parser.
 */
export function parseJUnit(xml: string): CiCase[] {
  const cases: CiCase[] = [];
  const caseRe =
    /<testcase\s+classname="([^"]*)"\s+name="([^"]*)">([\s\S]*?)<\/testcase>/g;
  let m: RegExpExecArray | null;
  while ((m = caseRe.exec(xml)) !== null) {
    const [, classname, name, body] = m;
    let outcome: CiCase["outcome"] = "pass";
    let detail = "";
    const fail = /<failure\s+message="([^"]*)"/.exec(body);
    const err = /<error\s+message="([^"]*)"/.exec(body);
    const out = /<system-out>([\s\S]*?)<\/system-out>/.exec(body);
    if (err) {
      outcome = "invalid";
      detail = xmlUnescape(err[1]);
    } else if (fail) {
      outcome = "fail";
      detail = xmlUnescape(fail[1]);
    } else if (out) {
      detail = xmlUnescape(out[1]);
    }
    cases.push({
      name: xmlUnescape(name),
      kind: xmlUnescape(classname),
      outcome,
      detail,
    });
  }
  return cases;
}

export function xmlUnescape(s: string): string {
  return s
    .replace(/&lt;/g, "<")
    .replace(/&gt;/g, ">")
    .replace(/&quot;/g, '"')
    .replace(/&apos;/g, "'")
    .replace(/&amp;/g, "&");
}

/**
 * 0-based line numbers of each `[[assert]]` block in a spec TOML, in file
 * order. The runner evaluates assertions in spec order and emits one JUnit
 * testcase per assertion in that same order, so testcase index N belongs to
 * block N.
 */
export function assertBlockLines(specText: string): number[] {
  const out: number[] = [];
  specText.split(/\r?\n/).forEach((l, i) => {
    if (/^\s*\[\[assert\]\]\s*(#.*)?$/.test(l)) out.push(i);
  });
  return out;
}

/**
 * Map a hauksbee-ci JUnit XML result to diagnostics on the spec TOML.
 *
 * @param xml the JUnit XML written by `hauksbee-ci run --junit`
 * @param specText the spec TOML source (for `[[assert]]` line mapping); when
 *                 null, or when block count and testcase count disagree
 *                 (defensive: an out-of-date buffer), diagnostics fall back
 *                 to file level (line 0).
 */
export function mapCiJUnit(xml: string, specText: string | null): MappedRun {
  const cases = parseJUnit(xml);
  const blocks = specText ? assertBlockLines(specText) : [];
  // Only trust positional mapping when it is exact.
  const positional = blocks.length === cases.length;

  const diags: MappedDiagnostic[] = [];
  let failed = 0;
  let invalid = 0;
  cases.forEach((c, i) => {
    if (c.outcome === "pass") return;
    if (c.outcome === "invalid") invalid += 1;
    else failed += 1;
    const prefix = c.outcome === "invalid" ? "INVALID" : "FAIL";
    diags.push({
      line: positional ? blocks[i] : 0,
      severity: "error",
      message: `[${prefix}] ${c.name}\n${c.detail}`,
      code: `ci/${c.outcome}`,
    });
  });

  const passedCount = cases.length - failed - invalid;
  const verdict = invalid > 0 ? "INVALID" : failed > 0 ? "RED" : "GREEN";
  return {
    diagnostics: diags,
    summary: {
      passed: failed === 0 && invalid === 0,
      errors: diags.length,
      warnings: 0,
      infos: 0,
      total: cases.length,
      label: `${passedCount}/${cases.length} assertions passed - ${verdict}`,
    },
  };
}

/** A single file-level error (spec error, unparseable output, etc.). */
export function singleError(message: string): MappedRun {
  return {
    diagnostics: [{ line: 0, severity: "error", message, code: "hauksbee/error" }],
    summary: {
      passed: false,
      errors: 1,
      warnings: 0,
      infos: 0,
      total: 1,
      label: message.split("\n")[0],
    },
  };
}
