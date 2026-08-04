#!/usr/bin/env bun
/**
 * The embed's interaction cache recorder.
 *
 * frontend/capture/record-demo-sessions.ts records the SIM TIMELINE (the /ws
 * message stream) for each demo board. This records everything else the app's
 * real surfaces ask a server for, so the embedded widget can run the untouched
 * frontend against a cache instead of an engine:
 *
 *   POST /api/analyze                the board's report (no firmware)
 *   POST /api/analyze-with-firmware  the report with the co-sim
 *   POST /api/check                  one recorded run per check rule, both with
 *                                    and without firmware, plus the curated
 *                                    presets (the full ladder, the red run)
 *   GET  /api/live/status            the live-session probe
 *
 * Every response is what a real `hauksbee serve` on this machine answered to a
 * real multipart upload of the real board bytes. Nothing is synthesised; a
 * recording that fails to come back is dropped from the cache and the embed
 * then does not offer that interaction (demo/EMBED.md, "nothing dead-ends").
 *
 * Output: demo/sessions/cache/<board>.json, plus demo/sessions/cache/index.json.
 *
 * Usage: bun demo/capture/record-embed-cache.ts [board-id ...]
 * Env:   HAUKSBEE_BIN, HAUKSBEE_CI_BIN, HAUKSBEE_QEMU_XTENSA, HB_CACHE_PORT
 */

import { execFileSync } from 'node:child_process'
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs'
import { homedir } from 'node:os'
import { basename, relative, resolve } from 'node:path'
import { buildToml, defaultSpecName } from '../shared/checks-spec'
import type { CheckRowSeed, SupplyRow } from '../shared/checks-spec'
import { canonicalizeSpec, assertKey, specKey } from '../shared/spec-key'

const REPO = resolve(import.meta.dir, '../..')
const BIN = process.env.HAUKSBEE_BIN ?? resolve(REPO, 'target/release/hauksbee')
const CI_BIN = process.env.HAUKSBEE_CI_BIN ?? resolve(REPO, 'target/release/hauksbee-ci')
const QEMU_XTENSA = process.env.HAUKSBEE_QEMU_XTENSA
  ?? resolve(homedir(), '.hauksbee-qemu-esp/qemu/bin/qemu-system-xtensa')
const PORT = Number(process.env.HB_CACHE_PORT ?? 3530)
const OUT = resolve(REPO, 'demo/sessions/cache')
/** A single check run that takes longer than this is not worth a demo. */
const CHECK_TIMEOUT_MS = Number(process.env.HB_CHECK_TIMEOUT_MS ?? 240_000)

const wait = (ms: number) => new Promise(r => setTimeout(r, ms))

// ── What each board offers ───────────────────────────────────────────────────

interface RuleDef {
  /** Stable id, used by the embed only for logging. */
  id: string
  /** The builder row, exactly as the panel would hold it. */
  row: CheckRowSeed
  /** Record this rule only with the firmware attached (a co-sim rule). */
  firmwareOnly?: boolean
}

interface PresetDef {
  id: string
  label: string
  /** One-line story for the embed's preset chip. */
  note: string
  rows: CheckRowSeed[]
  withFirmware: boolean
}

interface BoardDef {
  id: string
  /** Human title for the embed's board switcher. */
  title: string
  /** One line under the title. */
  tagline: string
  boardPath: string
  firmwarePath?: string
  /** The recorded sim session this board's live surface replays. */
  sessionId: string
  /** Milliseconds every recorded run uses (one value per board: the panel has
   *  one duration field, and a rule recorded at another duration would not be
   *  the rule the user is looking at). */
  durationMs: string
  /** Nets the invitation may point at, most interesting first. */
  featureNets: string[]
  /** Parts worth clicking on the map. */
  featureParts: string[]
  rules: RuleDef[]
  presets: PresetDef[]
  /** Which preset the panel opens on. */
  defaultPreset: string
  /** Hand the firmware bytes to the embedded app (so the checks panel can run
   *  the co-sim rules). False where the image is too large to lazy-load over
   *  someone else's landing page: the co-sim story is then told by the recorded
   *  session on the live surface, and the checks surface stays analog. */
  stageFirmware?: boolean
}

const BOARDS: BoardDef[] = [
  {
    id: 'watchy',
    title: 'Watchy v1.5',
    tagline: 'An ESP32-S3 e-paper smartwatch someone actually fabricated: 86 footprints, two rails, a charger and a boost converter.',
    boardPath: resolve(REPO, 'crates/hauksbee-ci/examples/boards/watchy.kicad_pcb'),
    firmwarePath: resolve(REPO, 'demo/firmware/watchy_display_init_s3/flash.bin'),
    sessionId: 'watchy-display_init',
    durationMs: '200',
    featureNets: ['+3V3', 'RES', 'VBUS', '+BATT', 'BUSY', 'SCK'],
    featureParts: ['U4', 'U3', 'L2', 'D5', 'R17', 'J3'],
    rules: [
      { id: 'no-faults', row: { kind: 'no_faults' } },
      { id: 'rail-3v3', row: { kind: 'voltage', net: '+3V3', min: '3.0', max: '3.6', after_ms: '50' } },
      { id: 'rail-vbus', row: { kind: 'voltage', net: 'VBUS', min: '4.75' } },
      { id: 'rail-window-3v3', row: { kind: 'rail_window', net: '+3V3', dip_below: '3.0', for_max_ms: '5' } },
      { id: 'charger-current', row: { kind: 'max_current', ref: 'R17', amps: '0.5' } },
      { id: 'charger-temp', row: { kind: 'max_temp', ref: 'U3', celsius: '85' } },
      // The Watchy story: the e-paper reset line has no pull resistor, so the
      // board alone cannot hold it high; only the firmware can.
      { id: 'display-res', row: { kind: 'boot-coverage', net: 'RES', min: '2.6', deadline_ms: '150' } },
      { id: 'vibe-toggle', row: { kind: 'toggle', net: 'VIB_PWM', min_toggles: '1' } },
    ],
    presets: [
      {
        id: 'ladder',
        label: 'The power-up ladder',
        note: 'Nothing over-stressed, both rails in tolerance, the charger inside its ratings.',
        withFirmware: true,
        rows: [
          { kind: 'no_faults' },
          { kind: 'voltage', net: '+3V3', min: '3.0', max: '3.6', after_ms: '50' },
          { kind: 'voltage', net: 'VBUS', min: '4.75' },
          { kind: 'max_current', ref: 'R17', amps: '0.5' },
          { kind: 'max_temp', ref: 'U3', celsius: '85' },
        ],
      },
      {
        id: 'vibe-never-pulses',
        label: 'The haptic line, asked too early',
        note: 'The same ladder plus "the vibration motor line must move". It does not, inside a 200 ms power-up window, and the run says so.',
        withFirmware: true,
        rows: [
          { kind: 'no_faults' },
          { kind: 'voltage', net: '+3V3', min: '3.0', max: '3.6', after_ms: '50' },
          { kind: 'toggle', net: 'VIB_PWM', min_toggles: '1' },
        ],
      },
    ],
    defaultPreset: 'ladder',
    // The S3 flash image is 4.2 MB; the widget will not pull that over a
    // landing page. Watchy's co-sim evidence is the recorded session.
    stageFirmware: false,
  },
  {
    id: 'boot_gate',
    title: 'Boot gate + firmware',
    tagline: 'An ATmega328P driving a MOSFET gate that has no pull resistor. Whether that is a bug depends on the firmware, so the firmware runs.',
    boardPath: resolve(REPO, 'crates/hauksbee-ci/examples/boards/boot_gate.kicad_pcb'),
    firmwarePath: resolve(REPO, 'testdata/firmware/boot_gate_a/boot_gate.hex'),
    sessionId: 'boot_gate-nominal',
    durationMs: '50',
    featureNets: ['GATE_CTRL', '+5V', 'LOAD_DRAIN', 'GND'],
    featureParts: ['U1', 'Q1', 'R1'],
    rules: [
      { id: 'no-faults', row: { kind: 'no_faults' } },
      { id: 'rail-5v', row: { kind: 'voltage', net: '+5V', min: '4.75' } },
      { id: 'gate-driven', row: { kind: 'boot-coverage', net: 'GATE_CTRL', min: '3.0', deadline_ms: '20' } },
      { id: 'load-current', row: { kind: 'max_current', ref: 'R1', amps: '0.1' } },
      { id: 'mosfet-temp', row: { kind: 'max_temp', ref: 'Q1', celsius: '150' } },
    ],
    presets: [
      {
        id: 'gate-with-firmware',
        label: 'Gate check, firmware loaded',
        note: 'The gate must be actively driven high within 20 ms of reset. Variant A firmware does that.',
        withFirmware: true,
        rows: [
          { kind: 'boot-coverage', net: 'GATE_CTRL', min: '3.0', deadline_ms: '20' },
          { kind: 'no_faults' },
        ],
      },
      {
        id: 'gate-no-firmware',
        label: 'Gate check, no firmware',
        note: 'The same check with the firmware unstaged: nothing drives the gate, and the check says so.',
        withFirmware: false,
        rows: [
          { kind: 'boot-coverage', net: 'GATE_CTRL', min: '3.0', deadline_ms: '20' },
          { kind: 'no_faults' },
        ],
      },
    ],
    defaultPreset: 'gate-with-firmware',
  },
  {
    id: 'blinky',
    title: 'Blinky',
    tagline: 'The simplest board here: an ATmega328P, an LED on D13, a divider into ADC0. The firmware blinks and prints.',
    boardPath: resolve(REPO, 'crates/hauksbee-ci/examples/boards/blinky.kicad_pcb'),
    firmwarePath: resolve(REPO, 'testdata/firmware/demo/demo.hex'),
    sessionId: 'blinky-nominal',
    durationMs: '1000',
    featureNets: ['D13', '+5V', 'LED_A', 'ADC0', 'GND'],
    featureParts: ['D1', 'U1', 'R1', 'R2'],
    rules: [
      { id: 'no-faults', row: { kind: 'no_faults' } },
      { id: 'rail-5v', row: { kind: 'voltage', net: '+5V', min: '4.75' } },
      { id: 'banner', row: { kind: 'uart', contains: 'hauksbee-demo v1' } },
      { id: 'led-blink', row: { kind: 'toggle', net: 'D13', freq_hz: '5', tolerance: '0.4' } },
      { id: 'led-current', row: { kind: 'max_current', ref: 'R1', amps: '0.02' } },
      // Deliberately red: the firmware never prints this.
      { id: 'wrong-banner', row: { kind: 'uart', contains: 'bootloader ready' } },
    ],
    presets: [
      {
        id: 'bench-sanity',
        label: 'The bench sanity pass',
        note: 'The rail holds, the banner prints, the LED blinks at 5 Hz, nothing over-stressed.',
        withFirmware: true,
        rows: [
          { kind: 'voltage', net: '+5V', min: '4.75' },
          { kind: 'uart', contains: 'hauksbee-demo v1' },
          { kind: 'toggle', net: 'D13', freq_hz: '5', tolerance: '0.4' },
          { kind: 'no_faults' },
        ],
      },
      {
        id: 'wrong-string',
        label: 'A check that is wrong',
        note: 'The same pass asking for a string the firmware never prints. This is what red looks like.',
        withFirmware: true,
        rows: [
          { kind: 'voltage', net: '+5V', min: '4.75' },
          { kind: 'uart', contains: 'bootloader ready' },
          { kind: 'no_faults' },
        ],
      },
    ],
    defaultPreset: 'bench-sanity',
  },
  {
    id: 'button_pullup',
    title: 'Button + pull-up',
    tagline: 'Three nets and one resistor. The smallest thing the engine can be asked about.',
    boardPath: resolve(REPO, 'testdata/boards/button_pullup.kicad_pcb'),
    sessionId: 'button_pullup-nominal',
    durationMs: '100',
    featureNets: ['+5V', 'BTN', 'GND'],
    featureParts: ['R1'],
    rules: [
      { id: 'no-faults', row: { kind: 'no_faults' } },
      { id: 'rail-5v', row: { kind: 'voltage', net: '+5V', min: '4.75' } },
      { id: 'btn-high', row: { kind: 'voltage', net: 'BTN', min: '4.5' } },
      { id: 'pullup-current', row: { kind: 'max_current', ref: 'R1', amps: '0.01' } },
      { id: 'pullup-temp', row: { kind: 'max_temp', ref: 'R1', celsius: '125' } },
      // Deliberately red: nothing pulls BTN down, so it cannot sit near zero.
      { id: 'btn-low', row: { kind: 'voltage', net: 'BTN', max: '0.5' } },
    ],
    presets: [
      {
        id: 'rail-and-pullup',
        label: 'The rail and the pull-up',
        note: 'The rail holds, the pull-up holds BTN high, the resistor stays inside its rating.',
        withFirmware: false,
        rows: [
          { kind: 'voltage', net: '+5V', min: '4.75' },
          { kind: 'voltage', net: 'BTN', min: '4.5' },
          { kind: 'max_current', ref: 'R1', amps: '0.01' },
          { kind: 'no_faults' },
        ],
      },
      {
        id: 'btn-pressed',
        label: 'As if the button were held',
        note: 'Asking BTN to sit near ground when nothing is pulling it down. Red, for the right reason.',
        withFirmware: false,
        rows: [
          { kind: 'voltage', net: 'BTN', max: '0.5' },
          { kind: 'no_faults' },
        ],
      },
    ],
    defaultPreset: 'rail-and-pullup',
  },
]

// ── The recorder ─────────────────────────────────────────────────────────────

const base = `http://127.0.0.1:${PORT}`

/** The running `hauksbee serve`. Held module-level because a co-sim check can
 *  take the server down (a QEMU run on a big board has), and a recording run
 *  that lost its server must restart it rather than write a cache full of
 *  "SKIPPED" where a real verdict exists. */
let server: { kill: () => void } | null = null

const serverUp = async () => {
  try { return (await fetch(`${base}/api/startup`)).ok } catch { return false }
}

/** Restart the server if it has gone away. Returns false if it cannot be
 *  brought back, and the caller then records nothing rather than guessing. */
async function ensureServerUp(): Promise<boolean> {
  if (await serverUp()) return true
  console.log('  (server went away; restarting it)')
  try { server?.kill() } catch { /* already gone */ }
  await wait(500)
  try {
    server = await startServer()
    return true
  } catch (e) {
    console.error(`  could not restart the server: ${e instanceof Error ? e.message : String(e)}`)
    return false
  }
}

async function startServer() {
  const env: Record<string, string | undefined> = { ...process.env, HAUKSBEE_CI_BIN: CI_BIN }
  if (existsSync(QEMU_XTENSA)) env.HAUKSBEE_QEMU_XTENSA = QEMU_XTENSA
  const proc = Bun.spawn([BIN, 'serve', '--port', String(PORT), '--no-open', '--quiet'], {
    cwd: REPO, env, stdout: 'inherit', stderr: 'inherit',
  })
  const deadline = Date.now() + 60_000
  for (;;) {
    try {
      const res = await fetch(`${base}/api/startup`)
      if (res.ok) break
    } catch { /* not up yet */ }
    if (Date.now() > deadline) { proc.kill(); throw new Error(`serve never came up on :${PORT}`) }
    await wait(250)
  }
  return proc
}

interface WebReportLike {
  ok?: boolean
  file_name: string
  board_name?: string
  num_components: number
  num_nets: number
  nets?: string[]
  components?: { reference: string; value: string }[]
  supplies?: { net: string; volts: number }[]
  serious?: number
  total?: number
  headline?: string
  sections?: { title: string; findings?: unknown[] }[]
}

/** POST /api/analyze exactly as the app does: raw body + the filename header. */
async function analyze(boardBlob: Blob, boardName: string): Promise<WebReportLike> {
  const res = await fetch(`${base}/api/analyze`, {
    method: 'POST',
    headers: { 'X-Board-Filename': boardName, 'Content-Type': 'application/octet-stream' },
    body: boardBlob,
  })
  const text = await res.text()
  if (!res.ok) throw new Error(`/api/analyze ${res.status}: ${text.slice(0, 300)}`)
  return JSON.parse(text) as WebReportLike
}

async function analyzeWithFirmware(
  board: Blob, boardName: string, fw: Blob, fwName: string,
): Promise<WebReportLike> {
  const fd = new FormData()
  fd.append('board', board, boardName)
  fd.append('firmware', fw, fwName)
  const res = await fetch(`${base}/api/analyze-with-firmware`, { method: 'POST', body: fd })
  const text = await res.text()
  if (!res.ok) throw new Error(`/api/analyze-with-firmware ${res.status}: ${text.slice(0, 300)}`)
  return JSON.parse(text) as WebReportLike
}

interface RunResponse {
  ok: boolean
  error?: string
  passed?: boolean
  exit_code?: number
  analog_abort?: boolean
  coverage?: string | null
  substitutions?: string[]
  coverage_warnings?: string[]
  results?: { label: string; kind: string; passed: boolean; invalid: boolean; detail: string }[]
}

/** POST /api/check exactly as the panel does. */
async function runCheck(
  toml: string, board: Blob, boardName: string, fw: { blob: Blob; name: string } | null,
): Promise<{ response: RunResponse; ms: number }> {
  const fd = new FormData()
  fd.append('board', board, boardName)
  if (fw) fd.append('firmware', fw.blob, fw.name)
  fd.append('spec', new Blob([toml], { type: 'text/plain' }), 'spec.toml')
  const t0 = Date.now()
  const ctrl = new AbortController()
  const timer = setTimeout(() => ctrl.abort(), CHECK_TIMEOUT_MS)
  try {
    const res = await fetch(`${base}/api/check`, { method: 'POST', body: fd, signal: ctrl.signal })
    const text = await res.text()
    const ms = Date.now() - t0
    try {
      return { response: JSON.parse(text) as RunResponse, ms }
    } catch {
      return { response: { ok: false, error: text.trim().slice(0, 400) }, ms }
    }
  } finally {
    clearTimeout(timer)
  }
}

const verdictOf = (r: RunResponse): 'passed' | 'failed' | 'invalid' | 'error' => {
  if (!r.ok) return 'error'
  const results = r.results ?? []
  if (results.length === 0) return r.passed ? 'passed' : 'failed'
  if (results.some(x => !x.invalid && !x.passed)) return 'failed'
  if (results.some(x => x.invalid)) return 'invalid'
  return 'passed'
}

interface RecordedRun {
  id: string
  label: string
  note: string
  /** Cache key: canonical spec + firmware name. */
  key: string
  /** For a single-rule recording, the per-assertion key the composer uses. */
  assert_keys: string[]
  kind: 'rule' | 'preset'
  rule_id?: string
  rows: CheckRowSeed[]
  toml: string
  firmware: string | null
  verdict: 'passed' | 'failed' | 'invalid' | 'error'
  wall_ms: number
  response: RunResponse
}

async function recordBoard(def: BoardDef, engine: { commit: string; version: string }) {
  console.log(`\n=== ${def.id} ===`)
  const boardName = basename(def.boardPath)
  const boardBytes = new Uint8Array(readFileSync(def.boardPath))
  const boardBlob = new Blob([boardBytes])
  const fwName = def.firmwarePath ? basename(def.firmwarePath) : null
  const fwBlob = def.firmwarePath ? new Blob([new Uint8Array(readFileSync(def.firmwarePath))]) : null

  const reportNoFw = await analyze(boardBlob, boardName)
  console.log(`  /api/analyze              ${reportNoFw.num_components} parts, ${reportNoFw.num_nets} nets, ${reportNoFw.total ?? 0} findings`)
  let reportWithFw: WebReportLike | null = null
  if (fwBlob && fwName) {
    reportWithFw = await analyzeWithFirmware(boardBlob, boardName, fwBlob, fwName)
    console.log(`  /api/analyze-with-firmware  ${reportWithFw.total ?? 0} findings, co-sim ran`)
  }
  const report = def.stageFirmware === false ? reportNoFw : (reportWithFw ?? reportNoFw)

  const liveStatus = await (await fetch(`${base}/api/live/status`)).json()

  // The supplies the panel seeds itself with, from the report the panel holds.
  const supplies: SupplyRow[] = (report.supplies ?? []).map(s => ({ net: s.net, volts: String(s.volts) }))
  const specName = defaultSpecName(report)

  const runs: RecordedRun[] = []
  const record = async (
    id: string, label: string, note: string, rows: CheckRowSeed[],
    withFirmware: boolean, kind: 'rule' | 'preset', ruleId?: string,
  ) => {
    if (withFirmware && (!fwBlob || !fwName)) return
    if (!(await ensureServerUp())) {
      console.log(`  ${id.padEnd(28)} SKIPPED (no server)`)
      return
    }
    const toml = buildToml(specName, def.durationMs, supplies, rows)
    const fw = withFirmware && fwBlob && fwName ? { blob: fwBlob, name: fwName } : null
    const fwKeyName = fw ? fw.name : null
    let out: { response: RunResponse; ms: number }
    try {
      out = await runCheck(toml, boardBlob, boardName, fw)
    } catch (first) {
      // One retry, behind a restart: the failure mode seen in practice is the
      // server dying mid-co-sim, and the run itself is reproducible.
      if (!(await ensureServerUp())) {
        console.log(`  ${id.padEnd(28)} SKIPPED (${first instanceof Error ? first.message : String(first)})`)
        return
      }
      try {
        out = await runCheck(toml, boardBlob, boardName, fw)
      } catch (e) {
        console.log(`  ${id.padEnd(28)} SKIPPED (${e instanceof Error ? e.message : String(e)})`)
        return
      }
    }
    const canon = canonicalizeSpec(toml)
    const verdict = verdictOf(out.response)
    runs.push({
      id, label, note, kind, rule_id: ruleId, rows, toml,
      firmware: fwKeyName,
      key: specKey(canon, fwKeyName),
      assert_keys: canon.asserts.map(a => assertKey(canon, a, fwKeyName)),
      verdict, wall_ms: out.ms, response: out.response,
    })
    const detail = out.response.ok
      ? (out.response.results ?? []).map(r => (r.invalid ? '?' : r.passed ? '+' : '-')).join('')
      : (out.response.error ?? '').slice(0, 80)
    console.log(`  ${id.padEnd(28)} ${verdict.padEnd(7)} ${String(out.ms).padStart(6)}ms  ${detail}`)
  }

  // One run per rule, with and without the firmware: the two-sided story (a
  // co-sim check that goes red the moment the firmware is unstaged) is only
  // tellable if both sides were actually recorded.
  for (const rule of def.rules) {
    if (!rule.firmwareOnly) {
      await record(`rule:${rule.id}`, rule.id, '', [rule.row], false, 'rule', rule.id)
    }
    if (fwBlob && def.stageFirmware !== false) {
      await record(`rule:${rule.id}+fw`, rule.id, '', [rule.row], true, 'rule', rule.id)
    }
  }
  for (const p of def.presets) {
    // A preset can only ask for the firmware if the embed will actually stage
    // it; otherwise the recorded run would answer a spec the panel cannot send.
    const withFw = p.withFirmware && def.stageFirmware !== false
    await record(`preset:${p.id}`, p.label, p.note, p.rows, withFw, 'preset')
  }

  const cache = {
    id: def.id,
    title: def.title,
    tagline: def.tagline,
    session_id: def.sessionId,
    engine_commit: engine.commit,
    engine_version: engine.version,
    recorded_at: new Date().toISOString(),
    board_file: `sessions/${def.id}/${boardName}`,
    board_name: boardName,
    firmware_file: fwName && def.stageFirmware !== false ? `sessions/${def.id}/${fwName}` : null,
    firmware_name: def.stageFirmware === false ? null : fwName,
    firmware_source: def.firmwarePath ? relative(REPO, def.firmwarePath) : null,
    board_source: relative(REPO, def.boardPath),
    duration_ms: def.durationMs,
    spec_name: specName,
    supplies,
    feature_nets: def.featureNets.filter(n => (report.nets ?? []).includes(n)),
    feature_parts: def.featureParts.filter(r => (report.components ?? []).some(c => c.reference === r)),
    nets: report.nets ?? [],
    parts: (report.components ?? []).map(c => ({ ref: c.reference, value: c.value })),
    default_preset: def.defaultPreset,
    analyze: reportNoFw,
    analyze_with_firmware: reportWithFw,
    live_status: liveStatus,
    checks: runs,
  }

  mkdirSync(OUT, { recursive: true })
  const path = resolve(OUT, `${def.id}.json`)
  writeFileSync(path, `${JSON.stringify(cache)}\n`)
  const kb = (Buffer.byteLength(JSON.stringify(cache)) / 1024).toFixed(0)
  console.log(`  wrote ${relative(REPO, path)} (${kb} KiB, ${runs.length} recorded runs)`)

  // The board and firmware bytes the embed feeds the app, next to the session.
  const dir = resolve(REPO, 'demo/sessions', def.id)
  mkdirSync(dir, { recursive: true })
  writeFileSync(resolve(dir, boardName), boardBytes)
  if (def.firmwarePath && fwName && def.stageFirmware !== false) {
    writeFileSync(resolve(dir, fwName), new Uint8Array(readFileSync(def.firmwarePath)))
  }

  return {
    id: def.id,
    title: def.title,
    tagline: def.tagline,
    cache: `sessions/cache/${def.id}.json`,
    session_id: def.sessionId,
    board_file: cache.board_file,
    firmware_file: cache.firmware_file,
    recorded_runs: runs.length,
    verdicts: {
      passed: runs.filter(r => r.verdict === 'passed').length,
      failed: runs.filter(r => r.verdict === 'failed').length,
      invalid: runs.filter(r => r.verdict === 'invalid').length,
      error: runs.filter(r => r.verdict === 'error').length,
    },
  }
}

async function main() {
  if (!existsSync(BIN)) throw new Error(`hauksbee binary not found at ${BIN}; build release first`)
  if (!existsSync(CI_BIN)) throw new Error(`hauksbee-ci binary not found at ${CI_BIN}; build release first`)
  const version = execFileSync(BIN, ['--version']).toString().trim()
  // The commit that produced THIS BINARY. `git rev-parse HEAD` is where the
  // tree sits now, which stops being the same thing the moment anything lands
  // after the build; a recording belongs to the binary that made it.
  const commit = /git ([0-9a-f]{7,40})/.exec(version)?.[1]
    ?? execFileSync('git', ['rev-parse', 'HEAD'], { cwd: REPO }).toString().trim()

  const only = process.argv.slice(2)
  const list = only.length > 0 ? BOARDS.filter(b => only.includes(b.id)) : BOARDS
  if (list.length === 0) {
    throw new Error(`no board matches ${only.join(', ')}; known: ${BOARDS.map(b => b.id).join(', ')}`)
  }

  server = await startServer()
  const entries: NonNullable<Awaited<ReturnType<typeof recordBoard>>>[] = []
  try {
    for (const def of list) entries.push(await recordBoard(def, { commit, version }))
  } finally {
    try { server?.kill() } catch { /* already gone */ }
  }

  // Partial runs merge, exactly like the session recorder's manifest.
  const indexPath = resolve(OUT, 'index.json')
  let existing: { boards?: typeof entries } = {}
  if (only.length > 0 && existsSync(indexPath)) {
    try { existing = JSON.parse(readFileSync(indexPath, 'utf8')) } catch { /* rebuilt */ }
  }
  const merged = [
    ...(existing.boards ?? []).filter(e => !entries.some(n => n.id === e.id)),
    ...entries,
  ]
  merged.sort((a, b) => BOARDS.findIndex(x => x.id === a.id) - BOARDS.findIndex(x => x.id === b.id))
  writeFileSync(indexPath, `${JSON.stringify({
    generated_at: new Date().toISOString(),
    engine_commit: commit,
    engine_version: version,
    boards: merged,
  }, null, 2)}\n`)
  console.log(`\nCache index: ${relative(REPO, indexPath)} (${merged.length} boards)`)
}

main().catch(e => { console.error(e); process.exit(1) })
