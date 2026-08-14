#!/usr/bin/env bun
// Re-record the fixtures the visual lint's server replays, from a real engine.
//
// Run it whenever a response shape changes (a new report field, a new dep, a
// new extraction kind). The lint's DOM is only as honest as these files.
//
//   ./target/release/hauksbee serve --port 3001 --quiet   # in the repo root
//   bun run tests/visual-lint/capture-fixtures.ts 3001    # in frontend/
//
// Two fixtures are hand-shaped after capture, on purpose, and the script
// reapplies both so a re-record does not quietly undo them:
//   - deps.json: absolute paths are rewritten to /home/runner, and renode +
//     esp-qemu are forced absent-and-installable, so the install buttons and
//     the "missing" badges are in the layout under test. On a developer machine
//     everything is present and those controls never render.
//   - the datasheet flow is never actually run; extraction talks to an LLM.

import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { writeFileSync } from 'node:fs'

const port = process.argv[2] ?? '3001'
const base = `http://127.0.0.1:${port}`
const here = dirname(fileURLToPath(import.meta.url))
const out = join(here, 'fixtures')
const board = join(here, '../../public/samples/watchy.kicad_pcb')

const write = (name: string, value: unknown) => {
  writeFileSync(join(out, name), `${JSON.stringify(value, null, 1)}\n`)
  console.log(`wrote ${name}`)
}

async function get(path: string) {
  const res = await fetch(base + path)
  if (!res.ok) throw new Error(`GET ${path} -> ${res.status}`)
  return res.json()
}

const startup = await get('/api/startup')
write('startup.json', startup)
write('live-status.json', await get('/api/live/status'))
write('extract-ready.json', await get('/api/models/extract/ready'))

// The report. The lint always analyzes the bundled watchy sample: a real
// fabricated board with 82 parts and 50 DRC findings, so the report surface
// under test is a full one rather than "looks healthy".
const bytes = await Bun.file(board).arrayBuffer()
const analyze = await fetch(`${base}/api/analyze`, {
  method: 'POST',
  headers: { 'X-Board-Filename': 'watchy.kicad_pcb', 'Content-Type': 'application/octet-stream' },
  body: bytes,
})
write('analyze-watchy.json', await analyze.json())

// A second report fixture for the datasheet-extraction surfaces. The watchy
// sample no longer has any unbound ACTIVE part (its last two ICs got curated
// models), so its report correctly renders no "parts with no model" panel; the
// panel's layout is tested against a synthetic board whose whole point is an
// open active IC. The board lives with the engine's own tests, which pin the
// same premise, so both cannot drift apart silently.
const openBoard = join(here, '../../../crates/hauksbee-engine/tests/fixtures/plain_check_open_active_ic.kicad_pcb')
const openBytes = await Bun.file(openBoard).arrayBuffer()
const analyzeOpen = await fetch(`${base}/api/analyze`, {
  method: 'POST',
  headers: { 'X-Board-Filename': 'open_active_ic.kicad_pcb', 'Content-Type': 'application/octet-stream' },
  body: openBytes,
})
write('analyze-openparts.json', await analyzeOpen.json())

// One check run: the two assertions the lint's builder surface composes.
const spec = 'name = "watchy checks"\nduration_ms = 50\n\n'
  + '[[assert]]\nkind = "no_faults"\n\n'
  + '[[assert]]\nkind = "voltage"\nnet = "GND"\nmin = -0.1\nmax = 0.1\n'
const fd = new FormData()
fd.append('board', new Blob([bytes]), 'watchy.kicad_pcb')
fd.append('spec', new Blob([spec], { type: 'text/plain' }), 'spec.toml')
write('check-run.json', await (await fetch(`${base}/api/check`, { method: 'POST', body: fd })).json())

// The model validator, on the editor's own starter TOML.
const starter = '[[models]]\nid = "my_resistor"\nkind = "passive"\n'
  + 'description = "what this part is, in a few words"\n\n'
  + '[models.match]\nvalue_re = "^10k$"\n'
write('models-check.json', await (await fetch(`${base}/api/models/check`, {
  method: 'POST',
  headers: { 'content-type': 'application/json' },
  body: JSON.stringify({ toml: starter, format: 'toml' }),
})).json())

// Environment page, then the two edits described at the top of this file.
type Dep = { id: string; path: string | null; present: boolean; installable: boolean; version: string | null }
const deps = await get('/api/deps') as { deps: Dep[] }
for (const dep of deps.deps) {
  if (dep.path) dep.path = dep.path.replace(/\/Users\/[^/;]+/g, '/home/runner')
  if (dep.id === 'renode' || dep.id === 'esp-qemu') {
    dep.present = false
    dep.installable = true
    dep.path = null
    dep.version = null
  }
}
write('deps.json', deps)
