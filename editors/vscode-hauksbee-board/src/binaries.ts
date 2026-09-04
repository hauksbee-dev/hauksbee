// Locating the `hauksbee` / `hauksbee-ci` binaries.
//
// Same order as the documented pre-commit hook and the KiCad plugin
// (integrations/kicad-plugin/hauksbee_ci_core.py: find_binary), so a machine set
// up for one integration is set up for all of them:
//
//   1. the explicit VS Code setting (`hauksbee.path` / `hauksbee.ciPath`)
//   2. the env var (`HAUKSBEE_BIN` / `HAUKSBEE_CI_BIN`)
//   3. PATH
//   4. a local build: `<workspace>/target/release/<name>`, then `target/debug`
//
// Step 4 is what makes the extension work in the hauksbee repo itself, and for
// anyone who ran `cargo build --release` once without touching PATH.

import * as fs from "fs";
import * as path from "path";

export type BinaryName = "hauksbee" | "hauksbee-ci";

export const ENV_VAR: Record<BinaryName, string> = {
  hauksbee: "HAUKSBEE_BIN",
  "hauksbee-ci": "HAUKSBEE_CI_BIN",
};

export interface DiscoveryInput {
  /** The `hauksbee.path` / `hauksbee.ciPath` setting, when non-empty. */
  configured?: string;
  env?: Record<string, string | undefined>;
  /** Workspace folder paths, searched for `target/release`. */
  roots?: string[];
  /** Injectable for tests. Must answer "is this an executable file?". */
  isExecutable?: (p: string) => boolean;
}

export function isExecutableFile(p: string): boolean {
  try {
    if (!fs.statSync(p).isFile()) return false;
    fs.accessSync(p, fs.constants.X_OK);
    return true;
  } catch {
    return false;
  }
}

/**
 * The binary's path, or undefined when nothing was found. Callers should show
 * the build hint rather than shelling out to a name that does not exist.
 */
export function findBinary(name: BinaryName, input: DiscoveryInput = {}): string | undefined {
  const env = input.env ?? process.env;
  const exec = input.isExecutable ?? isExecutableFile;

  for (const explicit of [input.configured, env[ENV_VAR[name]]]) {
    if (explicit && explicit.trim() !== "" && exec(explicit)) return explicit;
  }

  const exeSuffix = process.platform === "win32" ? ".exe" : "";
  for (const dir of (env.PATH ?? "").split(path.delimiter)) {
    if (dir === "") continue;
    const candidate = path.join(dir, name + exeSuffix);
    if (exec(candidate)) return candidate;
  }

  for (const root of input.roots ?? []) {
    for (const profile of ["release", "debug"]) {
      const candidate = path.join(root, "target", profile, name + exeSuffix);
      if (exec(candidate)) return candidate;
    }
  }
  return undefined;
}
