// Mocha entry point, run inside the VS Code extension host.

import * as fs from "fs";
import * as path from "path";
import Mocha from "mocha";

export function run(): Promise<void> {
  const mocha = new Mocha({ ui: "tdd", color: true, timeout: 60_000 });
  const here = __dirname;
  for (const file of fs.readdirSync(here)) {
    if (file.endsWith(".test.js")) mocha.addFile(path.join(here, file));
  }
  return new Promise((resolve, reject) => {
    mocha.run((failures) =>
      failures > 0 ? reject(new Error(`${failures} test(s) failed`)) : resolve()
    );
  });
}
