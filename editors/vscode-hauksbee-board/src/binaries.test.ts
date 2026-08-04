// Binary discovery. The order is a contract with the other integrations
// (pre-commit hook, KiCad plugin), so it is pinned here.

import { describe, expect, test } from "bun:test";
import * as path from "path";
import { ENV_VAR, findBinary } from "./binaries";

const sep = path.delimiter;

/** A fake filesystem where only the listed paths are executable. */
const only = (...paths: string[]) => (p: string) => paths.includes(p);

describe("findBinary", () => {
  test("the explicit setting wins over everything", () => {
    const found = findBinary("hauksbee-ci", {
      configured: "/opt/ci",
      env: { HAUKSBEE_CI_BIN: "/env/ci", PATH: `/bin${sep}/usr/bin` },
      roots: ["/repo"],
      isExecutable: only("/opt/ci", "/env/ci", "/bin/hauksbee-ci", "/repo/target/release/hauksbee-ci"),
    });
    expect(found).toBe("/opt/ci");
  });

  test("then the env var the other integrations already use", () => {
    expect(ENV_VAR["hauksbee-ci"]).toBe("HAUKSBEE_CI_BIN");
    expect(ENV_VAR.hauksbee).toBe("HAUKSBEE_BIN");
    const found = findBinary("hauksbee-ci", {
      env: { HAUKSBEE_CI_BIN: "/env/ci", PATH: "/bin" },
      isExecutable: only("/env/ci", "/bin/hauksbee-ci"),
    });
    expect(found).toBe("/env/ci");
  });

  test("then PATH, in order", () => {
    const found = findBinary("hauksbee", {
      env: { PATH: `/first${sep}/second` },
      isExecutable: only("/first/hauksbee", "/second/hauksbee"),
    });
    expect(found).toBe("/first/hauksbee");
  });

  test("then a local cargo build: release before debug", () => {
    const release = path.join("/repo", "target", "release", "hauksbee-ci");
    const debug = path.join("/repo", "target", "debug", "hauksbee-ci");
    expect(
      findBinary("hauksbee-ci", {
        env: { PATH: "/bin" },
        roots: ["/repo"],
        isExecutable: only(release, debug),
      })
    ).toBe(release);
    expect(
      findBinary("hauksbee-ci", {
        env: { PATH: "/bin" },
        roots: ["/repo"],
        isExecutable: only(debug),
      })
    ).toBe(debug);
  });

  test("a configured path that does not exist is skipped, not shelled out to", () => {
    const found = findBinary("hauksbee-ci", {
      configured: "/gone/ci",
      env: { PATH: "/bin" },
      isExecutable: only("/bin/hauksbee-ci"),
    });
    expect(found).toBe("/bin/hauksbee-ci");
  });

  test("nothing found is undefined, so the caller can show the build hint", () => {
    expect(
      findBinary("hauksbee-ci", { env: { PATH: "/bin" }, roots: ["/repo"], isExecutable: () => false })
    ).toBeUndefined();
  });

  test("an empty PATH entry is not treated as the current directory", () => {
    expect(
      findBinary("hauksbee", {
        env: { PATH: `${sep}/bin` },
        isExecutable: only("hauksbee", "/bin/hauksbee"),
      })
    ).toBe("/bin/hauksbee");
  });
});
