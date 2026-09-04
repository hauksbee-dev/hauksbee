// Headless VS Code run: downloads (once, into .vscode-test/) a real VS Code,
// installs THIS extension into it, opens test/e2e/workspace, and runs the mocha
// suite in ./suite inside the extension host.
//
// This is the only layer that proves the wiring: that the providers are
// registered on plain `.toml` documents, that diagnostics reach the Problems
// panel with the ranges the unit tests describe, and that the loader shell-out
// finds the binary and lands its message on the right line.

import * as fs from "fs";
import * as os from "os";
import * as path from "path";
import { runTests } from "@vscode/test-electron";

/**
 * VS Code's single-instance lock is a unix socket under --user-data-dir, and a
 * unix socket path cannot exceed ~103 characters. The default location
 * (.vscode-test/user-data inside this package, itself inside a repo path) blows
 * through that with `listen EINVAL`, so the data dir has to be short and
 * therefore outside the repo. Override with HAUKSBEE_VSCODE_TEST_DIR.
 */
function shortDataDir(): string {
  const base =
    process.env.HAUKSBEE_VSCODE_TEST_DIR ??
    (process.platform === "win32" ? path.join(os.tmpdir(), "hbx") : "/tmp/hb-vscode-test");
  fs.mkdirSync(base, { recursive: true });
  return base;
}

async function main(): Promise<void> {
  // Compiled to out-e2e/runTest.js, so the package root is one level up.
  const extensionDevelopmentPath = path.resolve(__dirname, "..");
  const extensionTestsPath = path.resolve(__dirname, "suite", "index");
  const workspace = path.join(extensionDevelopmentPath, "test", "e2e", "workspace");
  const repoRoot = path.resolve(extensionDevelopmentPath, "..", "..");

  await runTests({
    extensionDevelopmentPath,
    extensionTestsPath,
    launchArgs: [
      workspace,
      "--disable-extensions",
      "--disable-gpu",
      `--user-data-dir=${path.join(shortDataDir(), "user-data")}`,
      `--extensions-dir=${path.join(shortDataDir(), "extensions")}`,
    ],
    extensionTestsEnv: {
      // The extension's own discovery order puts an installed binary on PATH
      // ahead of a local build, and an installed one is routinely older than
      // this checkout. Pin the loader layer to the workspace build.
      HAUKSBEE_CI_BIN: path.join(repoRoot, "target", "release", "hauksbee-ci"),
      HAUKSBEE_BIN: path.join(repoRoot, "target", "release", "hauksbee"),
    },
  });
}

main().catch((err) => {
  console.error("headless VS Code run failed:", err);
  process.exit(1);
});
