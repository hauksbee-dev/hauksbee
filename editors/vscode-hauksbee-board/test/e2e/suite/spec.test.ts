// End-to-end, inside a real VS Code: open the workspace's specs and read the
// diagnostics VS Code actually holds, the completions the providers actually
// return, and the hover a user would actually see.

import * as assert from "assert";
import * as path from "path";
import * as vscode from "vscode";

// Compiled to out-e2e/suite/, so the package root is two levels up.
const WORKSPACE = path.resolve(__dirname, "..", "..", "test", "e2e", "workspace");
const EXTENSION_ID = "hauksbee-dev.hauksbee-board";

function file(rel: string): vscode.Uri {
  return vscode.Uri.file(path.join(WORKSPACE, rel));
}

async function open(rel: string): Promise<vscode.TextDocument> {
  const doc = await vscode.workspace.openTextDocument(file(rel));
  await vscode.window.showTextDocument(doc);
  return doc;
}

/** Poll until `predicate` holds, so we do not race the debounce or the shell-out. */
async function until<T>(
  what: string,
  produce: () => T,
  predicate: (v: T) => boolean,
  timeoutMs = 30_000
): Promise<T> {
  const deadline = Date.now() + timeoutMs;
  let last = produce();
  while (Date.now() < deadline) {
    if (predicate(last)) return last;
    await new Promise((r) => setTimeout(r, 200));
    last = produce();
  }
  assert.fail(`timed out waiting for ${what}; last value: ${JSON.stringify(last, null, 2)}`);
}

const specDiagnostics = (uri: vscode.Uri): vscode.Diagnostic[] =>
  vscode.languages.getDiagnostics(uri).filter((d) => d.source === "hauksbee-ci");

const summarise = (ds: vscode.Diagnostic[]) =>
  ds.map((d) => ({
    code: String(d.code),
    line: d.range.start.line,
    message: d.message.split("\n")[0],
  }));

suite("hauksbee spec support in a real VS Code", () => {
  suiteSetup(async () => {
    const ext = vscode.extensions.getExtension(EXTENSION_ID);
    assert.ok(ext, `extension ${EXTENSION_ID} is not present`);
    await ext!.activate();
    assert.ok(ext!.isActive, "extension did not activate");
  });

  test("a broken spec produces located diagnostics", async () => {
    const doc = await open("ci/broken.toml");
    const found = await until(
      "diagnostics on ci/broken.toml",
      () => summarise(specDiagnostics(doc.uri)),
      (ds) => ds.length >= 3
    );

    const line = (needle: string) =>
      doc
        .getText()
        .split("\n")
        .findIndex((l) => l.includes(needle));

    // 1. the typo'd assertion kind, on the value, with the did-you-mean hint
    const kind = found.find((d) => d.code === "spec/bad-enum");
    assert.ok(kind, `no bad-enum diagnostic in ${JSON.stringify(found)}`);
    assert.match(kind!.message, /'voltag' is not a valid kind.*did you mean 'voltage'\?/);
    assert.strictEqual(kind!.line, line('kind = "voltag"'));

    // 2. the bench supply with no volts, on its `kind` line
    const volts = found.find((d) => d.code === "spec/supply-needs-volts");
    assert.ok(volts, `no supply-needs-volts diagnostic in ${JSON.stringify(found)}`);
    assert.match(volts!.message, /supply on '\+5V': `bench` needs an explicit `volts`/);
    assert.strictEqual(volts!.line, line('kind = "bench"'));

    // 3. the inverted window, on its `min` line
    const window = found.find((d) => d.code === "spec/assert-inverted-window");
    assert.ok(window, `no inverted-window diagnostic in ${JSON.stringify(found)}`);
    assert.match(window!.message, /min \(5\) is greater than max \(1\)/);
    assert.strictEqual(window!.line, line("min = 5.0"));

    // Every one is an Error and carries the exact span, not the whole line.
    for (const d of specDiagnostics(doc.uri)) {
      assert.strictEqual(d.severity, vscode.DiagnosticSeverity.Error, d.message);
      assert.ok(d.range.end.character > d.range.start.character, `zero-width range: ${d.message}`);
    }
  });

  test("editing away a mistake clears its diagnostic", async () => {
    const doc = await open("ci/broken.toml");
    const editor = await vscode.window.showTextDocument(doc);
    await until(
      "the initial bad-enum diagnostic",
      () => specDiagnostics(doc.uri),
      (ds) => ds.some((d) => d.code === "spec/bad-enum")
    );

    const lineNo = doc
      .getText()
      .split("\n")
      .findIndex((l) => l.includes('kind = "voltag"'));
    await editor.edit((b) =>
      b.replace(doc.lineAt(lineNo).range, 'kind = "voltage"')
    );
    await until(
      "the bad-enum diagnostic to clear",
      () => summarise(specDiagnostics(doc.uri)),
      (ds) => !ds.some((d) => d.code === "spec/bad-enum")
    );
    // The other two are untouched.
    const still = summarise(specDiagnostics(doc.uri)).map((d) => d.code);
    assert.ok(still.includes("spec/supply-needs-volts"), JSON.stringify(still));
    assert.ok(still.includes("spec/assert-inverted-window"), JSON.stringify(still));
  });

  test("the loader layer really shells out: it catches a missing board", async () => {
    // ci/no_board.toml is structurally perfect. The schema lint has nothing to
    // say about it, so a diagnostic here can only have come from running the
    // real `hauksbee-ci` binary and parsing its stderr.
    const doc = await open("ci/no_board.toml");
    await vscode.commands.executeCommand("hauksbee.validateSpec");
    const found = await until(
      "the loader's board-missing diagnostic",
      () => summarise(specDiagnostics(doc.uri)),
      (ds) => ds.some((d) => d.code === "spec/board-missing")
    );
    const missing = found.find((d) => d.code === "spec/board-missing")!;
    assert.match(missing.message, /no board file at .*not_here\.kicad_pcb/);
    assert.strictEqual(
      missing.line,
      doc
        .getText()
        .split("\n")
        .findIndex((l) => l.startsWith("board ="))
    );
    // A board that is not checked out is not a broken spec: it must not gate.
    const raw = specDiagnostics(doc.uri).find((d) => d.code === "spec/board-missing")!;
    assert.strictEqual(raw.severity, vscode.DiagnosticSeverity.Information);
  });

  test("a valid spec, checked by the real loader, has no errors", async () => {
    const doc = await open("ci/valid.toml");
    // Force the loader layer (not just the schema lint), then give it time to
    // come back and publish: passing instantly would prove nothing.
    await vscode.commands.executeCommand("hauksbee.validateSpec");
    await new Promise((r) => setTimeout(r, 8000));
    const all = specDiagnostics(doc.uri);
    // An Information note is allowed: a hauksbee built without the `avr` feature
    // cannot run this board's firmware, and that is about the machine rather than
    // about the spec. Anything at Error severity is a claim about the file.
    const errors = all.filter((d) => d.severity === vscode.DiagnosticSeverity.Error);
    assert.deepStrictEqual(summarise(errors), []);
    for (const d of all) {
      assert.strictEqual(String(d.code), "spec/environment", d.message);
      assert.match(d.message, /not about this spec/);
    }
  });

  test("other people's TOML is left completely alone", async () => {
    const doc = await open("Cargo.toml");
    await new Promise((r) => setTimeout(r, 1500));
    assert.deepStrictEqual(summarise(specDiagnostics(doc.uri)), []);
  });

  test("completions come from the schema, on a plain .toml document", async () => {
    const doc = await open("ci/valid.toml");
    const kindLine = doc
      .getText()
      .split("\n")
      .findIndex((l) => l.includes('kind = "bench"'));
    // Just inside the opening quote of `kind = "bench"`.
    const inside = new vscode.Position(kindLine, 'kind = "'.length);
    const list = await vscode.commands.executeCommand<vscode.CompletionList>(
      "vscode.executeCompletionItemProvider",
      doc.uri,
      inside
    );
    const labels = (list?.items ?? []).map((i) =>
      typeof i.label === "string" ? i.label : i.label.label
    );
    for (const want of ["ideal", "bench", "wall", "usb", "battery"]) {
      assert.ok(labels.includes(want), `missing supply kind '${want}' in ${JSON.stringify(labels)}`);
    }
  });

  test("board nets are offered inside a net string", async () => {
    const doc = await open("ci/valid.toml");
    const netLine = doc
      .getText()
      .split("\n")
      .findIndex((l) => l.trim().startsWith('net = "+5V"'));
    const inside = new vscode.Position(netLine, 'net = "'.length);
    // The net list comes from `hauksbee run --list-nets` in the background, so
    // the first request can legitimately come back without it. Retry.
    let labels: string[] = [];
    for (let attempt = 0; attempt < 40; attempt++) {
      const list = await vscode.commands.executeCommand<vscode.CompletionList>(
        "vscode.executeCompletionItemProvider",
        doc.uri,
        inside
      );
      labels = (list?.items ?? []).map((i) =>
        typeof i.label === "string" ? i.label : i.label.label
      );
      if (labels.includes("LED_A")) break;
      await new Promise((r) => setTimeout(r, 500));
    }
    assert.ok(labels.includes("LED_A"), `expected blinky's nets, got ${JSON.stringify(labels)}`);
    assert.ok(labels.includes("+5V"), `expected blinky's nets, got ${JSON.stringify(labels)}`);
  });

  test("a quick fix repairs the typo, and the diagnostic goes away", async () => {
    // Its own fixture: `broken.toml` is edited by an earlier test, and an unsaved
    // edit persists for the session.
    const doc = await open("ci/typo.toml");
    await until(
      "the bad-enum diagnostic",
      () => specDiagnostics(doc.uri),
      (ds) => ds.some((d) => d.code === "spec/bad-enum")
    );
    const diagnostic = specDiagnostics(doc.uri).find((d) => d.code === "spec/bad-enum")!;
    const actions = await vscode.commands.executeCommand<vscode.CodeAction[]>(
      "vscode.executeCodeActionProvider",
      doc.uri,
      diagnostic.range,
      vscode.CodeActionKind.QuickFix.value
    );
    const fix = (actions ?? []).find((a) => a.title === "Change to `voltage`");
    assert.ok(
      fix,
      `no rename fix; offered: ${JSON.stringify((actions ?? []).map((a) => a.title))}`
    );
    assert.strictEqual(fix!.isPreferred, true);
    assert.ok(fix!.edit, "the fix carries no edit");

    assert.ok(await vscode.workspace.applyEdit(fix!.edit!), "applyEdit failed");
    await until(
      "the bad-enum diagnostic to clear after the fix",
      () => specDiagnostics(doc.uri).map((d) => String(d.code)),
      (codes) => !codes.includes("spec/bad-enum")
    );
    assert.match(doc.getText(), /kind = "voltage"/);

    // The supply's missing `volts` is fixable too, with the loader's own value.
    const volts = specDiagnostics(doc.uri).find((d) => d.code === "spec/supply-needs-volts")!;
    const more = await vscode.commands.executeCommand<vscode.CodeAction[]>(
      "vscode.executeCodeActionProvider",
      doc.uri,
      volts.range,
      vscode.CodeActionKind.QuickFix.value
    );
    const add = (more ?? []).find((a) => a.title === "Add `volts = 3.3`");
    assert.ok(add, `no add-volts fix; offered: ${JSON.stringify((more ?? []).map((a) => a.title))}`);
    assert.ok(await vscode.workspace.applyEdit(add!.edit!), "applyEdit failed");
    await until(
      "the supply diagnostic to clear",
      () => specDiagnostics(doc.uri).map((d) => String(d.code)),
      (codes) => !codes.includes("spec/supply-needs-volts")
    );
    assert.match(doc.getText(), /volts = 3\.3/);
  });

  test("a key completion arrives as a snippet with a choice list", async () => {
    const doc = await open("ci/valid.toml");
    const supplyLine = doc
      .getText()
      .split("\n")
      .findIndex((l) => l.startsWith("[[supply]]"));
    // An empty position inside the supply block.
    const editor = await vscode.window.showTextDocument(doc);
    await editor.edit((b) => b.insert(new vscode.Position(supplyLine + 1, 0), "\n"));
    const list = await vscode.commands.executeCommand<vscode.CompletionList>(
      "vscode.executeCompletionItemProvider",
      doc.uri,
      new vscode.Position(supplyLine + 1, 0)
    );
    const item = (list?.items ?? []).find((i) =>
      (typeof i.label === "string" ? i.label : i.label.label) === "chemistry"
    );
    assert.ok(item, "no `chemistry` completion");
    assert.ok(
      item!.insertText instanceof vscode.SnippetString,
      `insertText is not a SnippetString: ${typeof item!.insertText}`
    );
    assert.match(
      (item!.insertText as vscode.SnippetString).value,
      /chemistry = "\$\{1\|liion,/
    );
  });

  test("hovering a key shows the field's doc comment", async () => {
    const doc = await open("ci/valid.toml");
    const line = doc
      .getText()
      .split("\n")
      .findIndex((l) => l.startsWith("duration_ms"));
    const hovers = await vscode.commands.executeCommand<vscode.Hover[]>(
      "vscode.executeHoverProvider",
      doc.uri,
      new vscode.Position(line, 3)
    );
    const text = (hovers ?? [])
      .flatMap((h) => h.contents)
      .map((c) => (typeof c === "string" ? c : (c as vscode.MarkdownString).value))
      .join("\n");
    assert.match(text, /duration_ms/);
    assert.match(text, /Simulated duration in milliseconds/);
  });
});
