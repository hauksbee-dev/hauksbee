// The VS Code layer for spec TOML editing: diagnostics, completions, hovers.
//
// Everything with judgement in it lives in ./spec/* as pure functions over
// (text, position); this file only wires those to the editor, shells out to the
// binaries, and caches. Two independent diagnostic layers land in one
// collection:
//
//   local:  schema + cross-field lint, on every keystroke, no binary needed.
//   loader: the real `hauksbee-ci` loader's own message, on save.
//
// The loader layer exists because those messages are better than anything this
// extension could write, and because they are the ones CI will print. It runs
// only when the local lint is clean (otherwise it would just restate the first
// finding) and under a timeout, because `hauksbee-ci run` has no load-only mode:
// a spec that loads cleanly goes straight on into a full co-simulation. Getting
// past the load phase IS the signal we want, so a timeout is read as "the spec
// loaded" and the child is killed.

import * as vscode from "vscode";
import { spawn } from "child_process";
import * as fs from "fs";
import * as path from "path";
import { findBinary } from "./binaries";
import type { Sev } from "./mapping";
import { hoverAt, NET_KEYS, suggest, type Suggestion } from "./spec/complete";
import { specConfidence } from "./spec/detect";
import { lintSpec, type SpecIssue } from "./spec/lint";
import { mapLoaderStderr } from "./spec/loaderDiag";
import { fixesFor } from "./spec/quickfix";
import { SpecSchema } from "./spec/schemaModel";

const SPEC_SELECTOR: vscode.DocumentSelector = [{ scheme: "file", pattern: "**/*.toml" }];
const SOURCE = "hauksbee-ci";
const DEBOUNCE_MS = 250;
/**
 * Ceiling on concurrent loader shell-outs. The in-flight map is keyed per
 * document, which bounds a burst of saves on ONE spec; restoring a session with
 * eight spec tabs would otherwise start eight co-simulations at once. Beyond
 * this the automatic layer simply skips, and the explicit command still works.
 */
const MAX_CONCURRENT_LOADS = 2;

interface LoaderResult {
  version: number;
  issues: SpecIssue[];
}

export function activateSpecSupport(context: vscode.ExtensionContext): void {
  const schema = loadSchema(context);
  if (!schema) return;

  const collection = vscode.languages.createDiagnosticCollection("hauksbee-spec");
  const status = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 49);
  status.command = "hauksbee.validateSpec";
  const loaderCache = new Map<string, LoaderResult>();
  const netCache = new NetCache();
  const boardCache = new BoardCache();
  const timers = new Map<string, ReturnType<typeof setTimeout>>();
  /** Loader runs currently in flight, so one spec never spawns two co-sims. */
  const inFlight = new Map<string, LoaderRun>();

  context.subscriptions.push({
    dispose: () => {
      for (const t of timers.values()) clearTimeout(t);
      timers.clear();
      for (const run of inFlight.values()) run.cancel();
      inFlight.clear();
    },
  });

  const lint = (doc: vscode.TextDocument): ReturnType<typeof lintSpec> | undefined =>
    specConfidence(doc.uri.fsPath, doc.getText()) === "spec"
      ? lintSpec(doc.getText(), schema)
      : undefined;

  const publish = (doc: vscode.TextDocument): void => {
    const active = vscode.window.activeTextEditor?.document.uri.toString() === doc.uri.toString();
    if (doc.uri.scheme !== "file" || !doc.uri.fsPath.endsWith(".toml")) {
      if (active) status.hide();
      return;
    }
    // A file that only LOOKS positioned like a spec gets completion and hovers,
    // but no verdict: "spec ok" on a half-written file would be a lie.
    if (specConfidence(doc.uri.fsPath, doc.getText()) !== "spec") {
      collection.delete(doc.uri);
      if (active) status.hide();
      return;
    }
    const local = lint(doc)?.issues ?? [];
    const cached = loaderCache.get(doc.uri.toString());
    const fresh = cached?.version === doc.version;
    const all = [...local, ...(fresh ? cached!.issues : [])];
    collection.set(
      doc.uri,
      all.map((i) => toDiagnostic(i))
    );
    refreshStatus(status, doc, all, fresh);
  };

  const schedule = (doc: vscode.TextDocument): void => {
    const key = doc.uri.toString();
    const existing = timers.get(key);
    if (existing) clearTimeout(existing);
    timers.set(
      key,
      setTimeout(() => {
        timers.delete(key);
        publish(doc);
      }, DEBOUNCE_MS)
    );
  };

  const runLoader = async (doc: vscode.TextDocument, force: boolean): Promise<void> => {
    // Running a binary a workspace can point at is code execution, so it only
    // ever happens in a workspace the user has trusted.
    if (!vscode.workspace.isTrusted) return;
    const cfg = vscode.workspace.getConfiguration("hauksbee");
    const mode = cfg.get<string>("spec.loaderCheck", "onSave");
    if (!force && mode !== "onSave") return;
    const analysis = lint(doc);
    if (!analysis) return;
    // The local lint already names the problem, in the loader's own words.
    if (!force && analysis.issues.some((i) => i.severity === "error")) return;
    // A spec that loads cleanly and finishes its co-sim inside the timeout has
    // genuinely RUN, and a completed run writes the artifacts the spec asks for
    // (a `vcd_sink`'s `vcd_path`). Saving a file must not silently rewrite
    // another one, so specs that declare an output stay out of the automatic
    // layer; the explicit command still checks them.
    if (!force && writesArtifacts(analysis.doc.root)) return;

    const bin = findBinary("hauksbee-ci", {
      configured: cfg.get<string>("ciPath") || undefined,
      roots: workspaceRoots(),
    });
    if (!bin) {
      if (force) {
        vscode.window.showErrorMessage(
          "hauksbee: `hauksbee-ci` not found. Build it with `cargo build --release -p hauksbee-ci` " +
            "and set `hauksbee.ciPath`, or put it on PATH."
        );
      }
      return;
    }

    // One run per document at a time: `hauksbee-ci run` goes on into a full
    // co-simulation, so a burst of saves must not stack up co-sims.
    const key = doc.uri.toString();
    if (!force && !inFlight.has(key) && inFlight.size >= MAX_CONCURRENT_LOADS) return;
    inFlight.get(key)?.cancel();
    // The version BEFORE the await: any edit during the run invalidates the
    // ranges the result would be attached to.
    const version = doc.version;
    const run = startLoad(bin, doc.uri.fsPath, cfg.get<number>("spec.loaderTimeoutMs", 6000));
    inFlight.set(key, run);
    let outcome: LoadOutcome;
    try {
      outcome = await run.done;
    } finally {
      if (inFlight.get(key) === run) inFlight.delete(key);
    }
    // `unavailable` means the child never started (ENOENT after a rebuild
    // replaced the binary, EACCES, ETXTBSY). Caching that as a clean result
    // would let the status bar claim "this spec loads clean" for a run that
    // never happened.
    if (outcome.kind === "cancelled" || outcome.kind === "unavailable") return;
    if (doc.version !== version) return;

    const issues =
      outcome.kind === "spec-error" ? mapLoaderStderr(outcome.stderr, analysis.doc) : [];
    loaderCache.set(key, { version, issues });
    publish(doc);

    // Warm the net cache for completions while we are here.
    void netCache.forSpec(doc, workspaceRoots());
  };

  context.subscriptions.push(
    collection,
    status,
    vscode.workspace.onDidOpenTextDocument((doc) => {
      publish(doc);
      void runLoader(doc, false);
    }),
    vscode.workspace.onDidChangeTextDocument((e) => schedule(e.document)),
    vscode.workspace.onDidSaveTextDocument((doc) => {
      publish(doc);
      void runLoader(doc, false);
    }),
    vscode.workspace.onDidCloseTextDocument((doc) => {
      const key = doc.uri.toString();
      // Cancel the pending work FIRST: a debounce timer that fires after the
      // delete would resurrect diagnostics on a file nothing will clear again.
      const timer = timers.get(key);
      if (timer) clearTimeout(timer);
      timers.delete(key);
      inFlight.get(key)?.cancel();
      inFlight.delete(key);
      collection.delete(doc.uri);
      loaderCache.delete(key);
    }),
    vscode.window.onDidChangeActiveTextEditor((editor) => {
      if (editor) publish(editor.document);
      else status.hide();
    }),
    vscode.commands.registerCommand("hauksbee.validateSpec", async () => {
      const doc = vscode.window.activeTextEditor?.document;
      if (!doc) return;
      if (doc.isDirty) await doc.save();
      loaderCache.delete(doc.uri.toString());
      await runLoader(doc, true);
      const count = collection.get(doc.uri)?.length ?? 0;
      if (count === 0) {
        vscode.window.setStatusBarMessage("hauksbee: spec loads clean", 4000);
      }
    }),
    vscode.languages.registerCompletionItemProvider(
      SPEC_SELECTOR,
      {
        provideCompletionItems: (doc, position) => {
          if (specConfidence(doc.uri.fsPath, doc.getText()) === "none") return undefined;
          const nets = netCache.cached(doc);
          const items = suggest(
            doc.getText(),
            { line: position.line, col: position.character },
            schema,
            { nets, boards: boardCache.cached(doc), text: doc.getText() }
          );
          if (nets.length === 0) void netCache.forSpec(doc, workspaceRoots());
          void boardCache.refresh(doc);
          return items.map(toCompletion);
        },
      },
      // Retrigger inside a string (net / enum values) and after `= `.
      '"',
      "'",
      "=",
      "[",
      "."
    ),
    // Quick fixes. Every diagnostic already names its own answer ("did you mean
    // 'duration_ms'?", "add `chemistry = \"liion\"`"), so offering it as an edit
    // costs nothing and saves retyping it by hand.
    vscode.languages.registerCodeActionsProvider(
      SPEC_SELECTOR,
      {
        provideCodeActions: (doc, _range, ctx) => {
          const out: vscode.CodeAction[] = [];
          for (const d of ctx.diagnostics) {
            if (d.source !== SOURCE || typeof d.code !== "string") continue;
            const issue: SpecIssue = {
              span: {
                start: { line: d.range.start.line, col: d.range.start.character },
                end: { line: d.range.end.line, col: d.range.end.character },
              },
              severity: "error",
              message: d.message,
              code: d.code,
            };
            for (const fix of fixesFor(doc.getText(), issue, schema)) {
              const action = new vscode.CodeAction(fix.title, vscode.CodeActionKind.QuickFix);
              action.edit = new vscode.WorkspaceEdit();
              action.edit.replace(
                doc.uri,
                new vscode.Range(
                  fix.edit.span.start.line,
                  fix.edit.span.start.col,
                  fix.edit.span.end.line,
                  fix.edit.span.end.col
                ),
                fix.edit.newText
              );
              action.diagnostics = [d];
              action.isPreferred = fix.preferred;
              out.push(action);
            }
          }
          return out;
        },
      },
      { providedCodeActionKinds: [vscode.CodeActionKind.QuickFix] }
    ),
    vscode.languages.registerHoverProvider(SPEC_SELECTOR, {
      provideHover: (doc, position) => {
        if (specConfidence(doc.uri.fsPath, doc.getText()) === "none") return undefined;
        const info = hoverAt(
          doc.getText(),
          { line: position.line, col: position.character },
          schema
        );
        if (!info) return undefined;
        return new vscode.Hover(
          new vscode.MarkdownString(info.markdown),
          new vscode.Range(
            info.span.start.line,
            info.span.start.col,
            info.span.end.line,
            info.span.end.col
          )
        );
      },
    })
  );

  for (const doc of vscode.workspace.textDocuments) publish(doc);
}

// ── the load-only shell-out ──────────────────────────────────────────────────

type LoadOutcome =
  | { kind: "loaded" }
  | { kind: "spec-error"; stderr: string }
  | { kind: "unavailable" }
  | { kind: "cancelled" };

/** A load in progress, cancellable because the child can outlive its usefulness. */
export interface LoaderRun {
  done: Promise<LoadOutcome>;
  cancel: () => void;
}

/**
 * Ask `hauksbee-ci` to load a spec. There is no load-only subcommand, so this
 * starts `run` and reads the boundary we can observe: exit 2 with a message on
 * stderr means the spec (or its board) was rejected during load; anything else,
 * including the timeout, means load succeeded and the co-sim began.
 *
 * If hauksbee-ci ever grows a `check` / `--diagnose` mode this collapses to one
 * cheap call, and the timeout heuristic goes away.
 */
export function startLoad(bin: string, specPath: string, timeoutMs: number): LoaderRun {
  let cancel = () => {};
  const done = new Promise<LoadOutcome>((resolve) => {
    let child: ReturnType<typeof spawn>;
    try {
      child = spawn(bin, ["run", specPath, "--quiet", "--json"], {
        cwd: path.dirname(specPath),
        // A spec error is printed before any co-sim work; nothing needs a shell.
        shell: false,
      });
    } catch {
      resolve({ kind: "unavailable" });
      return;
    }
    let stderr = "";
    let settled = false;
    let drainScheduled = false;
    const pending: ReturnType<typeof setTimeout>[] = [];
    /**
     * SIGTERM first, because the CLI installs a signal reaper that stops its
     * Renode/QEMU children; SIGKILL only if it has not gone in two seconds, so
     * a wedged co-sim cannot leak.
     */
    const stop = () => {
      child.kill("SIGTERM");
      const hard = setTimeout(() => child.kill("SIGKILL"), 2000);
      hard.unref?.();
      pending.push(hard);
    };
    const finish = (outcome: LoadOutcome) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      resolve(outcome);
    };
    const timer = setTimeout(() => {
      // Past the load phase: the spec is structurally fine and a real run is
      // under way, which is not what we asked for. Stop it.
      stop();
      finish({ kind: "loaded" });
    }, timeoutMs);
    cancel = () => {
      if (settled) return;
      stop();
      finish({ kind: "cancelled" });
    };

    child.stderr?.on("data", (chunk: Buffer) => {
      stderr += chunk.toString();
      // Spec errors are one short message; bail as soon as the whole line is in
      // rather than waiting out the timeout. Only ever armed once.
      if (!drainScheduled && /^hauksbee-ci: /m.test(stderr) && stderr.includes("\n")) {
        drainScheduled = true;
        const t = setTimeout(() => {
          stop();
          finish({ kind: "spec-error", stderr });
        }, 50);
        pending.push(t);
      }
    });
    child.on("error", () => finish({ kind: "unavailable" }));
    child.on("close", (code) => {
      for (const t of pending) clearTimeout(t);
      if (code === 2 && stderr.trim() !== "") finish({ kind: "spec-error", stderr });
      else finish({ kind: "loaded" });
    });
  });
  return { done, cancel: () => cancel() };
}

// ── board-net completions ────────────────────────────────────────────────────

/**
 * `hauksbee run <board> --list-nets --json` output, cached per board file and
 * invalidated by mtime. Fails silent: no engine binary, an unresolvable board,
 * or a board that will not extract just means no net completions.
 */
class NetCache {
  private readonly byBoard = new Map<string, { mtimeMs: number; nets: string[] }>();
  private readonly inFlight = new Set<string>();

  cached(doc: vscode.TextDocument): string[] {
    const board = boardPathOf(doc);
    if (!board) return [];
    const entry = this.byBoard.get(board);
    if (!entry) return [];
    return statMtime(board) === entry.mtimeMs ? entry.nets : [];
  }

  async forSpec(doc: vscode.TextDocument, roots: string[]): Promise<string[]> {
    if (!vscode.workspace.isTrusted) return [];
    const board = boardPathOf(doc);
    if (!board) return [];
    const mtimeMs = statMtime(board);
    if (mtimeMs === undefined) return [];
    const entry = this.byBoard.get(board);
    if (entry?.mtimeMs === mtimeMs) return entry.nets;
    if (this.inFlight.has(board)) return entry?.nets ?? [];
    this.inFlight.add(board);
    try {
      const bin = findBinary("hauksbee", {
        configured: vscode.workspace.getConfiguration("hauksbee").get<string>("path") || undefined,
        roots,
      });
      if (!bin) return [];
      const nets = await listNets(bin, board);
      if (nets) this.byBoard.set(board, { mtimeMs, nets });
      return nets ?? [];
    } finally {
      this.inFlight.delete(board);
    }
  }
}

function listNets(bin: string, board: string): Promise<string[] | undefined> {
  return new Promise((resolve) => {
    let child: ReturnType<typeof spawn>;
    try {
      child = spawn(bin, ["run", board, "--list-nets", "--json"], { shell: false });
    } catch {
      resolve(undefined);
      return;
    }
    let stdout = "";
    const timer = setTimeout(() => {
      child.kill("SIGTERM");
      resolve(undefined);
    }, 20_000);
    child.stdout?.on("data", (c: Buffer) => (stdout += c.toString()));
    child.on("error", () => {
      clearTimeout(timer);
      resolve(undefined);
    });
    child.on("close", () => {
      clearTimeout(timer);
      try {
        const parsed: unknown = JSON.parse(stdout.trim().split("\n").pop() ?? "");
        if (Array.isArray(parsed)) {
          resolve(parsed.filter((n): n is string => typeof n === "string" && n.trim() !== ""));
          return;
        }
      } catch {
        // Not JSON: an older binary, or the board did not extract. Silent.
      }
      resolve(undefined);
    });
  });
}

/**
 * Does a completed run of this spec write into the workspace? Today that is a
 * `[[peripheral]]` with a `vcd_path`. Kept deliberately conservative: the cost
 * of a false positive is one spec checked on demand instead of on save.
 */
function writesArtifacts(root: Record<string, unknown>): boolean {
  const peripherals = root.peripheral;
  if (!Array.isArray(peripherals)) return false;
  return peripherals.some(
    (p) => typeof p === "object" && p !== null && "vcd_path" in (p as object)
  );
}

/**
 * Board files in the workspace, as paths relative to the spec being edited, for
 * completing `board = "…"`. That path is the one field whose value lives on
 * disk, and getting it wrong is the commonest reason a spec does not run.
 */
class BoardCache {
  private static readonly EXTS = ["board", "kicad_pcb", "kicad_sch", "net", "brd", "d356"];
  private readonly byFolder = new Map<string, string[]>();
  private readonly inFlight = new Set<string>();

  cached(doc: vscode.TextDocument): string[] {
    const found = this.byFolder.get(this.key(doc));
    if (!found) return [];
    const here = path.dirname(doc.uri.fsPath);
    return found
      .map((abs) => path.relative(here, abs).split(path.sep).join("/"))
      .sort((a, b) => a.split("/").length - b.split("/").length || a.localeCompare(b));
  }

  async refresh(doc: vscode.TextDocument): Promise<void> {
    const key = this.key(doc);
    if (this.inFlight.has(key)) return;
    this.inFlight.add(key);
    try {
      const found = await vscode.workspace.findFiles(
        `**/*.{${BoardCache.EXTS.join(",")}}`,
        "**/{node_modules,target,.git}/**",
        400
      );
      this.byFolder.set(
        key,
        found.filter((u) => u.scheme === "file").map((u) => u.fsPath)
      );
    } catch {
      // A workspace-less window, or a search that failed: no board completions.
    } finally {
      this.inFlight.delete(key);
    }
  }

  private key(doc: vscode.TextDocument): string {
    return vscode.workspace.getWorkspaceFolder(doc.uri)?.uri.toString() ?? "(none)";
  }
}

/** The spec's `board = "..."` resolved against the spec's own directory. */
function boardPathOf(doc: vscode.TextDocument): string | undefined {
  const m = /^\s*board\s*=\s*["'](.+?)["']/m.exec(doc.getText());
  if (!m) return undefined;
  const rel = m[1];
  const abs = path.isAbsolute(rel) ? rel : path.join(path.dirname(doc.uri.fsPath), rel);
  return fs.existsSync(abs) ? abs : undefined;
}

function statMtime(p: string): number | undefined {
  try {
    return fs.statSync(p).mtimeMs;
  } catch {
    return undefined;
  }
}

// ── glue ─────────────────────────────────────────────────────────────────────

function loadSchema(context: vscode.ExtensionContext): SpecSchema | undefined {
  const file = path.join(
    context.extensionPath,
    "schemas",
    "hauksbee-ci-spec.schema.json"
  );
  try {
    return new SpecSchema(JSON.parse(fs.readFileSync(file, "utf8")));
  } catch (e) {
    // Without the schema there is no spec support, but the board commands must
    // keep working, so this is a note rather than a failed activation.
    console.error(`hauksbee: could not read the spec schema (${file}): ${e}`);
    return undefined;
  }
}

function workspaceRoots(): string[] {
  return (vscode.workspace.workspaceFolders ?? []).map((f) => f.uri.fsPath);
}

function toDiagnostic(issue: SpecIssue): vscode.Diagnostic {
  const { start, end } = issue.span;
  // A zero-width squiggle is invisible, so widen it by one, but only on a
  // single-line span: widening the END column of a multi-line span would move
  // it on a completely different line.
  const endCol = start.line === end.line ? Math.max(end.col, start.col + 1) : end.col;
  const range = new vscode.Range(start.line, start.col, end.line, endCol);
  const d = new vscode.Diagnostic(range, issue.message, severity(issue.severity));
  d.source = SOURCE;
  d.code = issue.code;
  return d;
}

function severity(s: Sev): vscode.DiagnosticSeverity {
  switch (s) {
    case "error":
      return vscode.DiagnosticSeverity.Error;
    case "warning":
      return vscode.DiagnosticSeverity.Warning;
    default:
      return vscode.DiagnosticSeverity.Information;
  }
}

function toCompletion(s: Suggestion, index: number): vscode.CompletionItem {
  const kind =
    s.kind === "table"
      ? vscode.CompletionItemKind.Struct
      : s.kind === "value"
        ? vscode.CompletionItemKind.EnumMember
        : vscode.CompletionItemKind.Property;
  const item = new vscode.CompletionItem(s.label, kind);
  // A snippet puts the cursor on the value, and turns a closed vocabulary into a
  // choice list: accepting `kind` in a `[[supply]]` leaves you picking from
  // `ideal | bench | wall | usb | battery` rather than typing it.
  item.insertText = s.snippet
    ? new vscode.SnippetString(s.insertText)
    : s.insertText;
  item.detail = s.detail;
  if (s.documentation) item.documentation = new vscode.MarkdownString(s.documentation);
  // The range to overwrite. VS Code's own word range stops at `+`, `/` and `.`,
  // so completing `+5V` over a typed `+5` would insert `++5V` and a board path
  // would duplicate its prefix.
  if (s.replace) {
    item.range = new vscode.Range(
      s.replace.start.line,
      s.replace.start.col,
      s.replace.end.line,
      s.replace.end.col
    );
  }
  // `suggest` already ranked the list; preserve that exact order rather than
  // letting VS Code re-sort alphabetically and bury the relevant fields.
  item.sortText = String(index).padStart(4, "0");
  return item;
}

function refreshStatus(
  status: vscode.StatusBarItem,
  doc: vscode.TextDocument,
  issues: SpecIssue[],
  loaderRan: boolean
): void {
  if (vscode.window.activeTextEditor?.document.uri.toString() !== doc.uri.toString()) return;
  const errors = issues.filter((i) => i.severity === "error").length;
  if (errors === 0) {
    status.text = "$(pass) spec ok";
    // Only claim what was actually checked: without a loader run this is the
    // schema and cross-field lint agreeing, which is not the same as the binary
    // having loaded the file.
    status.tooltip = loaderRan
      ? "hauksbee-ci: this spec loads clean. Click to re-check."
      : "hauksbee-ci: schema lint clean. Click to check it against the hauksbee-ci binary.";
    status.backgroundColor = undefined;
  } else {
    status.text = `$(error) spec: ${errors} problem${errors === 1 ? "" : "s"}`;
    status.tooltip = "hauksbee-ci: see the Problems panel. Click to re-check.";
    status.backgroundColor = new vscode.ThemeColor("statusBarItem.errorBackground");
  }
  status.show();
}

export { NET_KEYS };
