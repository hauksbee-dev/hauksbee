// Hauksbee VS Code extension: a THIN shell-out client (no LSP — deferred by
// design, docs/dev-plans/07-ux-and-integrations.md §4). Two commands shell out
// to the `hauksbee` / `hauksbee-ci` binaries; their machine output is mapped
// to VS Code diagnostics by the pure functions in ./mapping.

import * as vscode from "vscode";
import { execFile } from "child_process";
import * as fs from "fs";
import * as os from "os";
import * as path from "path";
import { mapCiJUnit, mapEngineCheck, singleError, MappedRun, Sev } from "./mapping";

const BOARD_EXTS = [".board", ".kicad_pcb", ".kicad_sch", ".net", ".brd", ".d356"];
const INSTALL_HINT =
  "Install it with `cargo build --release` in the hauksbee repo and set the " +
  "setting to `target/release/<binary>`, or put the binary on PATH.";

let diagnostics: vscode.DiagnosticCollection;
let statusBar: vscode.StatusBarItem;
let lastRun: (() => Promise<void>) | undefined;

export function activate(context: vscode.ExtensionContext): void {
  diagnostics = vscode.languages.createDiagnosticCollection("hauksbee");
  statusBar = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 50);
  statusBar.command = "hauksbee.rerunLast";
  statusBar.text = "$(circuit-board) hauksbee";
  statusBar.tooltip = "Hauksbee: no run yet. Click to re-run the last check.";
  statusBar.show();

  context.subscriptions.push(
    diagnostics,
    statusBar,
    vscode.commands.registerCommand("hauksbee.runCiSpec", () => runCiSpec()),
    vscode.commands.registerCommand("hauksbee.checkBoard", () => checkBoard()),
    vscode.commands.registerCommand("hauksbee.rerunLast", () => {
      if (lastRun) return lastRun();
      vscode.window.showInformationMessage(
        "hauksbee: nothing to re-run yet. Run a CI spec or board check first."
      );
      return undefined;
    })
  );
}

export function deactivate(): void {
  // Collections/status bar are disposed via context.subscriptions.
}

// ── binary location ──────────────────────────────────────────────────────────

/** Resolve a binary: explicit setting first, else the bare name (PATH). */
function resolveBinary(settingKey: "path" | "ciPath", fallback: string): string {
  const configured = vscode.workspace.getConfiguration("hauksbee").get<string>(settingKey);
  return configured && configured.trim() !== "" ? configured : fallback;
}

interface ExecResult {
  code: number;
  stdout: string;
  stderr: string;
}

function run(bin: string, args: string[], cwd: string): Promise<ExecResult> {
  return new Promise((resolve, reject) => {
    execFile(
      bin,
      args,
      { cwd, maxBuffer: 64 * 1024 * 1024, timeout: 300_000 },
      (err, stdout, stderr) => {
        if (err && (err as NodeJS.ErrnoException).code === "ENOENT") {
          reject(new Error(`binary '${bin}' not found. ${INSTALL_HINT}`));
          return;
        }
        // Non-zero exit is a RESULT here (findings / failed assertions), not
        // an exec failure.
        const code =
          err && typeof (err as { code?: unknown }).code === "number"
            ? ((err as { code?: number }).code as number)
            : err
              ? 1
              : 0;
        resolve({ code, stdout: stdout ?? "", stderr: stderr ?? "" });
      }
    );
  });
}

// ── commands ─────────────────────────────────────────────────────────────────

async function runCiSpec(): Promise<void> {
  const uri = await pickTarget(
    (p) => p.endsWith(".toml"),
    "**/*.toml",
    "Pick a hauksbee-ci spec TOML"
  );
  if (!uri) return;

  const doRun = async () => {
    const ci = resolveBinary("ciPath", "hauksbee-ci");
    const junitPath = path.join(
      os.tmpdir(),
      `hauksbee-ci-${Date.now()}-${Math.floor(Math.random() * 1e6)}.xml`
    );
    setBusy(`running spec ${path.basename(uri.fsPath)}…`);
    let result: ExecResult;
    try {
      // hauksbee-ci has no --json; its machine format is JUnit XML (--junit).
      result = await run(
        ci,
        ["run", uri.fsPath, "--junit", junitPath, "--quiet"],
        path.dirname(uri.fsPath)
      );
    } catch (e) {
      showBinaryError(e);
      setIdle();
      return;
    }

    let mapped: MappedRun;
    if (result.code === 2 || !fs.existsSync(junitPath)) {
      // Spec/usage error: no XML was produced; surface stderr at file level.
      mapped = singleError(
        result.stderr.trim() || `hauksbee-ci exited ${result.code} with no report`
      );
    } else {
      const xml = fs.readFileSync(junitPath, "utf8");
      const specText = fs.readFileSync(uri.fsPath, "utf8");
      mapped = mapCiJUnit(xml, specText);
      fs.unlinkSync(junitPath);
    }
    publish(uri, mapped, "hauksbee-ci");
  };
  lastRun = doRun;
  await doRun();
}

async function checkBoard(): Promise<void> {
  const uri = await pickTarget(
    (p) => BOARD_EXTS.some((e) => p.endsWith(e)),
    `**/*{${BOARD_EXTS.join(",")}}`,
    "Pick a board file to check"
  );
  if (!uri) return;

  const doRun = async () => {
    const engine = resolveBinary("path", "hauksbee");
    setBusy(`checking ${path.basename(uri.fsPath)}…`);
    let result: ExecResult;
    try {
      result = await run(
        engine,
        ["run", uri.fsPath, "--check", "--json"],
        path.dirname(uri.fsPath)
      );
    } catch (e) {
      showBinaryError(e);
      setIdle();
      return;
    }

    let mapped: MappedRun;
    if (result.stdout.trim() === "") {
      mapped = singleError(
        result.stderr.trim() || `hauksbee exited ${result.code} with no output`
      );
    } else {
      const boardText = uri.fsPath.endsWith(".board")
        ? fs.readFileSync(uri.fsPath, "utf8")
        : null;
      mapped = mapEngineCheck(result.stdout, boardText);
    }
    publish(uri, mapped, "hauksbee");
  };
  lastRun = doRun;
  await doRun();
}

/** Active editor's file if it matches; else a quick-pick over the workspace. */
async function pickTarget(
  matches: (fsPath: string) => boolean,
  glob: string,
  placeholder: string
): Promise<vscode.Uri | undefined> {
  const active = vscode.window.activeTextEditor?.document;
  if (active && active.uri.scheme === "file" && matches(active.uri.fsPath)) {
    if (active.isDirty) await active.save();
    return active.uri;
  }
  const found = await vscode.workspace.findFiles(glob, "**/node_modules/**", 200);
  if (found.length === 0) {
    vscode.window.showErrorMessage(`hauksbee: no matching files (${glob}) in the workspace.`);
    return undefined;
  }
  const picked = await vscode.window.showQuickPick(
    found.map((u) => ({ label: vscode.workspace.asRelativePath(u), uri: u })),
    { placeHolder: placeholder }
  );
  return picked?.uri;
}

// ── rendering ────────────────────────────────────────────────────────────────

function toVsSeverity(s: Sev): vscode.DiagnosticSeverity {
  switch (s) {
    case "error":
      return vscode.DiagnosticSeverity.Error;
    case "warning":
      return vscode.DiagnosticSeverity.Warning;
    default:
      return vscode.DiagnosticSeverity.Information;
  }
}

function publish(uri: vscode.Uri, mapped: MappedRun, source: string): void {
  // Clear-on-rerun: one collection, per-file set replaces the previous run.
  const ds = mapped.diagnostics.map((d) => {
    const range = new vscode.Range(d.line, 0, d.line, Number.MAX_SAFE_INTEGER);
    const diag = new vscode.Diagnostic(range, d.message, toVsSeverity(d.severity));
    diag.source = source;
    diag.code = d.code;
    return diag;
  });
  diagnostics.set(uri, ds);

  const s = mapped.summary;
  const icon = s.passed ? "$(pass)" : "$(error)";
  statusBar.text = `${icon} hauksbee: ${s.label}`;
  statusBar.tooltip = `${source} on ${path.basename(uri.fsPath)} — click to re-run`;
  statusBar.backgroundColor = s.passed
    ? undefined
    : new vscode.ThemeColor("statusBarItem.errorBackground");

  if (s.passed) {
    vscode.window.setStatusBarMessage(`hauksbee: ${s.label}`, 5000);
  } else {
    vscode.window.showWarningMessage(`hauksbee: ${s.label} — see Problems panel.`);
  }
}

function setBusy(msg: string): void {
  statusBar.text = `$(sync~spin) hauksbee: ${msg}`;
  statusBar.backgroundColor = undefined;
}

function setIdle(): void {
  statusBar.text = "$(circuit-board) hauksbee";
}

function showBinaryError(e: unknown): void {
  const msg = e instanceof Error ? e.message : String(e);
  vscode.window.showErrorMessage(`hauksbee: ${msg}`);
}
