#!/usr/bin/env bun
/** Real-browser release gate for board drag-and-drop journeys.
 *
 * The external cohort supplies exactly five absolute board paths; the corpus
 * cohort supplies every discovered board input; the smoke cohort supplies one
 * named board for a native platform/front-door proof. Each
 * file enters through an actual DataTransfer drop (not setInputFiles on the
 * app's picker), then has to produce a useful report, a matching independent
 * JSON export, and a live session whose pause/step/play controls advance the
 * engine clock without browser errors.
 *
 * The exception is an input the corpus declares `unreadable-by-design`, named
 * in `HB_REFUSAL_FILES`: it has to produce the opposite, an honest refusal, and
 * the two expectations fail equally loudly when inverted. Redacted wire evidence
 * and screenshots are retained under `HB_E2E_OUT`.
 *
 * A board named in `HB_FIRMWARE_FILES` additionally carries a firmware image.
 * It is staged into the app's firmware slot BEFORE the board drop, so the
 * analysis runs through `/api/analyze-with-firmware` exactly as a user's would,
 * and the journey then holds the report to the co-simulation the manifest asked
 * for. What the run did or did not observe on the pins is recorded here and
 * graded by qc/value_grading.py rather than judged in the browser.
 */

import { chromium } from 'playwright'
import type { ConsoleMessage, Page } from 'playwright'
import { basename, isAbsolute, join } from 'node:path'
import { mkdirSync, realpathSync } from 'node:fs'
// The app's own strip of the engine's trailing "Supported: ..." clause. The
// refusal card renders the diagnostic through it, so the journey has to compare
// against the same transform rather than a second copy of the rule.
import { withoutEngineFormatList } from '../../src/lib/board-formats'
import {
  canonicalJson,
  cosimDrovePin,
  cosimFailures,
  cosimPrinted,
  parseFirmwarePlan,
  parseSimTime,
  type CosimSectionLike,
  type FirmwarePlan,
} from './value-signals'

interface WebReport {
  ok: boolean
  error: string | null
  board_name: string
  file_name: string
  num_components: number
  num_nets: number
  headline: string
  total: number
  serious: number
  sections: unknown[]
  notes?: unknown[]
  bind?: unknown
  cosim?: CosimSectionLike | null
}

/** What the journey observed about a board's paired firmware. The value
 *  contract in qc/value_grading.py grades these; the journey only records
 *  them, plus the few that are outright contract violations. */
interface FirmwareResult {
  staged: boolean
  /** The app's firmware slot accepted the image and named it back. */
  loaded: boolean
  expect: FirmwarePlan['expect']
  file: string
  /** What the inspect panel said the bytes are; the load-only evidence. */
  detail: string | null
  cosim_ran: boolean
  cosim_seconds: number | null
  /** The co-sim reported at least one DRIVEN gpio net. Serial output is a
   *  separate observation and is not folded in here. */
  pin_activity: boolean
  serial_activity: boolean
  /** Null when there was no pin activity to render. False means the co-sim
   *  claimed driven pins and the page showed none, which is a result nobody
   *  can see. */
  pin_activity_rendered: boolean | null
  analog_valid: boolean | null
}

interface BoardResult {
  /** The staged input path exactly as the gate supplied it. The release
   * validator matches this field against its staged board set before it
   * will accept the run, then redacts it in retained evidence. */
  path: string
  file: string
  input_sha256: string
  elapsed_ms: number
  response_status: number
  response_capture_error: string | null
  report: WebReport | null
  exported: boolean
  /** The corpus declared this input a format hauksbee does not read, so the
   * journey demands an honest refusal from it instead of a report. */
  expected_refusal: boolean
  /** The front door refused this input: no report, no export, and a rendered
   * message carrying what the server said. */
  refused: boolean
  refusal_message: string | null
  live_started: boolean
  sim_time_before_s: number | null
  sim_time_after_s: number | null
  /** Null where the manifest paired no firmware with this board. */
  firmware: FirmwareResult | null
  wire_events: WireEvent[]
  console_errors: string[]
  failures: string[]
}

interface WireEvent {
  direction: 'sent' | 'received'
  type: string
  t?: number
  sim_time?: number
  running?: boolean
  message?: string
}

const base = process.env.HB_E2E_BASE
const rawFiles = process.env.HB_BOARD_FILES
// Inputs the corpus declares hauksbee does not read (`unreadable-by-design`).
// They are staged deliberately: the only way to know a refusal is honest on a
// real file is to drop a real file and watch what the front door says. The
// journey holds them to the OPPOSITE contract from every other input, and
// holds it in both directions, so neither a refusal that should have been a
// report nor a report that should have been a refusal can pass.
const rawRefusals = process.env.HB_REFUSAL_FILES ?? '[]'
const cohort = process.env.HB_RELEASE_COHORT ?? 'external'
const output = process.env.HB_E2E_OUT ?? join(import.meta.dir, '../../output/playwright/drag-drop')

if (!base) throw new Error('HB_E2E_BASE must name a running real Hauksbee server')
if (!rawFiles) throw new Error('HB_BOARD_FILES must be a JSON array of absolute paths')
const files = JSON.parse(rawFiles) as unknown
if (cohort !== 'external' && cohort !== 'corpus' && cohort !== 'smoke') {
  throw new Error('HB_RELEASE_COHORT must be external, corpus or smoke')
}
if (!Array.isArray(files)
    || files.length === 0
    || (cohort === 'external' && files.length !== 5)
    || (cohort === 'smoke' && files.length !== 1)
    || !files.every(path => typeof path === 'string')
    || new Set(files).size !== files.length) {
  throw new Error(
    cohort === 'external'
      ? 'HB_BOARD_FILES must contain exactly five distinct path strings'
      : cohort === 'smoke'
        ? 'HB_BOARD_FILES must contain exactly one smoke-test path string'
        : 'HB_BOARD_FILES must contain every distinct corpus path',
  )
}
if (!files.every(isAbsolute)) throw new Error('every HB_BOARD_FILES entry must be an absolute path')
// The guard above proves the shape, but its narrowing does not reach the
// journey functions below; carry the proven type explicitly.
const boardFilePaths = files as string[]
const resolvedFiles = boardFilePaths.map(path => realpathSync(path))
// Distinctness, not a count: the external cohort's exactly-five contract is
// enforced by the guard above; the corpus cohort passes every discovered
// input, so these two checks only refuse duplicates smuggled via symlinks or
// identical file contents.
if (new Set(resolvedFiles).size !== resolvedFiles.length) throw new Error('HB_BOARD_FILES must contain distinct files')
const fileDigests = await Promise.all(resolvedFiles.map(async path => (
  new Bun.CryptoHasher('sha256').update(await Bun.file(path).arrayBuffer()).digest('hex')
)))
if (new Set(fileDigests).size !== fileDigests.length) {
  throw new Error('HB_BOARD_FILES must contain distinct board contents')
}
const refusalInput = JSON.parse(rawRefusals) as unknown
if (!Array.isArray(refusalInput) || !refusalInput.every(path => typeof path === 'string' && isAbsolute(path))) {
  throw new Error('HB_REFUSAL_FILES must be a JSON array of absolute paths')
}
const expectedRefusals = new Set((refusalInput as string[]).map(path => realpathSync(path)))
// A refusal expectation that names a file the gate is not dropping would sit
// there unexercised and read as coverage. Every entry must be one of the boards
// this run actually drops.
const unstagedRefusal = [...expectedRefusals].find(path => !resolvedFiles.includes(path))
if (unstagedRefusal !== undefined) {
  throw new Error(`HB_REFUSAL_FILES names a file this run does not drop: ${unstagedRefusal}`)
}

// Optional, and parallel to HB_BOARD_FILES. A board with a plan is dropped
// WITH its firmware, so the journey exercises the firmware-hardware
// interaction the board gates otherwise never touch. Absent or `[]` means no
// board in this run carries firmware and every journey behaves exactly as it
// did before.
const firmwarePlans = parseFirmwarePlan(process.env.HB_FIRMWARE_FILES ?? '[]', files.length)
for (const [index, plan] of firmwarePlans.entries()) {
  if (plan !== null && expectedRefusals.has(resolvedFiles[index])) {
    throw new Error('HB_FIRMWARE_FILES pairs firmware with a board expected to be refused')
  }
}

mkdirSync(output, { recursive: true })

function watchConsole(page: Page, errors: string[]) {
  page.on('console', (message: ConsoleMessage) => {
    if (message.type() === 'error') errors.push(message.text())
  })
  page.on('pageerror', error => errors.push(`pageerror: ${error.message}`))
}

function watchWire(page: Page, events: WireEvent[]) {
  page.on('websocket', socket => {
    const record = (direction: WireEvent['direction'], raw: string | Buffer) => {
      try {
        const value = JSON.parse(String(raw)) as Record<string, unknown>
        if (typeof value.type !== 'string') return
        events.push({
          direction,
          type: value.type,
          ...(typeof value.t === 'number' ? { t: value.t } : {}),
          ...(typeof value.sim_time === 'number' ? { sim_time: value.sim_time } : {}),
          ...(typeof value.running === 'boolean' ? { running: value.running } : {}),
          ...(typeof value.message === 'string' ? { message: value.message } : {}),
        })
      } catch {
        // Binary or non-JSON frames carry no Hauksbee protocol evidence.
      }
    }
    socket.on('framesent', event => record('sent', event.payload))
    socket.on('framereceived', event => record('received', event.payload))
  })
}

/** Repeat the analysis outside the browser, through the same endpoint the app
 *  itself would have used for this board: raw bytes when it carries no
 *  firmware, multipart when it does. Comparing a bare repeat against a
 *  firmware run would always differ and would say nothing. */
async function independentAnalysis(path: string, plan: FirmwarePlan | null): Promise<WebReport> {
  let response: Response
  if (plan === null) {
    response = await fetch(new URL('/api/analyze', base!), {
      method: 'POST',
      headers: {
        'Content-Type': 'application/octet-stream',
        'X-Board-Filename': basename(path),
      },
      body: Bun.file(path),
    })
  } else {
    const form = new FormData()
    form.append('board', await Bun.file(path).arrayBuffer().then(b => new Blob([b])), basename(path))
    form.append(
      'firmware',
      await Bun.file(plan.path).arrayBuffer().then(b => new Blob([b])),
      basename(plan.path),
    )
    response = await fetch(new URL('/api/analyze-with-firmware', base!), {
      method: 'POST',
      body: form,
    })
  }
  if (!response.ok) throw new Error(`independent analysis returned HTTP ${response.status}`)
  return await response.json() as WebReport
}

async function waitForSimTimeAbove(page: Page, floor: number, timeoutMs = 10_000) {
  const deadline = Date.now() + timeoutMs
  const time = page.locator('[data-testid="transport-time"]')
  while (Date.now() < deadline) {
    // Ensure the foreground page gets a paint opportunity. Live messages are
    // deliberately coalesced into requestAnimationFrame by useSimulation.
    await page.evaluate(() => new Promise<void>(resolve => requestAnimationFrame(() => resolve())))
    const value = parseSimTime(await time.innerText())
    if (value > floor) return value
    await page.waitForTimeout(25)
  }
  throw new Error(
    `simulation time did not advance beyond ${floor}s `
      + `(rendered=${JSON.stringify(await time.innerText())})`,
  )
}

async function exerciseLiveSimulation(page: Page, expectedFile: string, failures: string[]) {
  let liveStarted = false
  let before: number | null = null
  let after: number | null = null
  const run = page.locator('[data-testid="run-it"]')
  if (await run.count() !== 1) {
    failures.push('successful board report offered no live simulation action')
    return { liveStarted, before, after }
  }

  const launchResponse = page.waitForResponse(response => (
    response.request().method() === 'POST' && response.url().endsWith('/api/live/launch')
  ), { timeout: 30_000 })
  await run.click()
  await page.waitForFunction(() => Boolean(
    document.querySelector('[data-testid="live-replace-confirm"]')
      || document.querySelector('[data-testid="transport-speed"]')
      || document.querySelector('[data-testid="live-launch-error"]'),
  ), null, { timeout: 180_000 })
  if (await page.locator('[data-testid="live-replace-confirm"]').count() === 1) {
    await page.locator('[data-testid="confirm-replace-live"]').click()
  }

  const launched = await launchResponse
  const launchBody = await launched.json() as { ok?: boolean; board_name?: string; error?: string }
  if (!launched.ok() || launchBody.ok !== true || launchBody.board_name !== expectedFile) {
    failures.push(
      `live launch identity mismatch: HTTP ${launched.status()} ${JSON.stringify(launchBody)}`,
    )
    return { liveStarted, before, after }
  }
  const liveStatus = await page.evaluate(async () => {
    const response = await fetch('/api/live/status')
    return await response.json() as { active?: boolean; board_name?: string }
  })
  if (liveStatus.active !== true || liveStatus.board_name !== expectedFile) {
    failures.push(`live status identity mismatch: ${JSON.stringify(liveStatus)}`)
    return { liveStarted, before, after }
  }

  await page.waitForFunction(() => Boolean(
    document.querySelector('[data-testid="transport-speed"]')
      || document.querySelector('[data-testid="live-launch-error"]'),
  ), null, { timeout: 180_000 })
  const launchError = page.locator('[data-testid="live-launch-error"]')
  if (await launchError.count() === 1) {
    failures.push(`live simulation refused: ${(await launchError.innerText()).trim()}`)
    return { liveStarted, before, after }
  }

  await page.getByText('connected', { exact: true }).waitFor({ state: 'visible', timeout: 30_000 })
  liveStarted = true
  const time = page.locator('[data-testid="transport-time"]')
  if (await time.count() !== 1) {
    failures.push('live simulation exposes no measurable simulation clock')
    return { liveStarted, before, after }
  }

  // Hold the loop, step exactly once, then resume it. A connected canvas that
  // never advances is not tangible simulation benefit, and the three controls
  // are the shortest real proof that the browser and engine are bidirectional.
  const pause = page.getByRole('button', { name: 'Pause', exact: true })
  if (await pause.count() === 1) await pause.click()
  const play = page.getByRole('button', { name: 'Play', exact: true })
  await play.waitFor({ state: 'visible' })
  before = parseSimTime(await time.innerText())
  await page.getByRole('button', { name: 'Step one millisecond', exact: true }).click()
  const stepped = await waitForSimTimeAbove(page, before)
  await play.click()
  await pause.waitFor({ state: 'visible' })
  after = await waitForSimTimeAbove(page, stepped)
  await pause.click()
  return { liveStarted, before, after }
}

/** Audit the refusal an `unreadable-by-design` input must produce.
 *
 * The corpus stages these formats precisely because a refusal is only known to
 * be honest on a real file. Honest here means four things at once: the front
 * door says the input was not read, it says it in words that came from the
 * server rather than from a client-side guess, it offers the way forward, and
 * it offers NONE of the affordances that would imply it had analysed something
 * (a JSON export, a board inventory, a live launch). A refusal that quietly
 * hands back an exportable empty report is the failure this guards.
 *
 * Returns the rendered refusal text and the refusal payload to retain. Like the
 * readable path, the independent repeat analysis stands in when Chromium evicted
 * the drop's own response body, so a retained refusal row always carries the
 * server's words and the release validator can insist on them instead of
 * trusting a journey-authored boolean.
 */
async function auditRefusal(
  page: Page,
  path: string,
  captured: WebReport | null,
  failures: string[],
): Promise<{ shown: string | null; report: WebReport | null }> {
  for (const [testid, complaint] of [
    ['export-open', 'a refused input still offered a JSON export'],
    ['report-inventory', 'a refused input still claimed a parts/nets inventory'],
    ['run-it', 'a refused input still offered a live simulation action'],
  ] as const) {
    if (await page.locator(`[data-testid="${testid}"]`).count() !== 0) failures.push(complaint)
  }
  // Visible, not merely present: an inert or hidden retry element is the same
  // dead end as no retry element.
  if (!await page.locator('[data-testid="try-another-file"]').isVisible().catch(() => false)) {
    failures.push('the refusal is a dead end: no visible way to try another file')
  }

  // Server-derived and repeatable, not a one-off client-side verdict.
  const independent = await independentAnalysis(path, null)
  const serverError = independent.error?.trim() ?? ''
  if (independent.ok !== false || serverError === '') {
    failures.push('an independent repeat analysis of a refused input did not refuse')
  }
  if (captured?.ok === true) {
    failures.push('an input the corpus declares unreadable produced a board report')
  } else if (captured !== null && (captured.error?.trim() ?? '') !== serverError) {
    failures.push('the rendered refusal disagrees with an independent repeat analysis')
  }
  const report = captured ?? independent
  // A refusal must be empty of board content. An "unreadable" payload that
  // still carries parts, nets or check sections is a partial read wearing a
  // refusal's clothes, and it is exactly what a permissive reader claiming
  // binary noise produces.
  if (report.num_components > 0 || report.num_nets > 0 || (report.sections?.length ?? 0) > 0) {
    failures.push('a refused input still came back carrying board content')
  }

  const verdict = page.locator('[data-testid="report-verdict"]')
  if (await verdict.count() !== 1) {
    failures.push('a refused input explained nothing about why')
    return { shown: null, report }
  }
  const shown = (await verdict.innerText()).trim()
  if (serverError !== '' && !shown.includes(withoutEngineFormatList(serverError))) {
    failures.push('the rendered refusal does not carry what the server said')
  }
  return { shown, report }
}

/** Stage the paired firmware BEFORE the board is dropped.
 *
 * Order matters and is the whole reason this is its own step: the app runs the
 * analysis on the board drop, carrying whatever firmware is staged at that
 * instant. Staging afterwards would re-run the board and leave the journey
 * auditing the second run while holding the first run's response.
 *
 * Returns what the app then showed about the image, which is the evidence the
 * `load-only` expectation rests on.
 */
async function stageFirmware(
  page: Page,
  plan: FirmwarePlan,
  failures: string[],
): Promise<{ loaded: boolean; detail: string | null }> {
  const zone = page.locator('[data-testid="firmware-zone"]')
  if (await zone.count() !== 1) {
    failures.push('the intake offered no firmware slot')
    return { loaded: false, detail: null }
  }
  await page.locator('#firmware-file').setInputFiles(plan.path)
  try {
    await zone.locator('[data-testid="firmware-inspect"]').waitFor({ state: 'visible', timeout: 30_000 })
  } catch {
    failures.push('the staged firmware never filled the firmware slot')
    return { loaded: false, detail: null }
  }
  const named = await zone.innerText()
  if (!named.includes(basename(plan.path))) {
    failures.push('the firmware slot does not name the image that was staged')
  }
  // Inspecting is how a user confirms the right bytes landed, and for a
  // `load-only` image it is the only place the app says what it is. The panel
  // only carries this test id once the read finishes, so waiting for it is
  // also waiting out the "reading the image…" placeholder.
  await zone.locator('[data-testid="firmware-inspect"]').click()
  const panel = page.locator('[data-testid="firmware-detail"]')
  let detail: string | null = null
  try {
    await panel.waitFor({ state: 'visible', timeout: 30_000 })
    detail = (await panel.innerText()).trim()
  } catch {
    failures.push('the firmware slot would not say what the image is')
  }
  return { loaded: true, detail }
}

async function prepareDrop(page: Page, path: string) {
  await page.evaluate(() => {
    const old = document.querySelector<HTMLInputElement>('[data-hb-release-file]')
    old?.remove()
    const input = document.createElement('input')
    input.type = 'file'
    input.hidden = true
    input.dataset.hbReleaseFile = 'true'
    document.body.append(input)
  })
  const scratch = page.locator('[data-hb-release-file]')
  await scratch.setInputFiles(path)
  await page.evaluate(() => {
    const input = document.querySelector<HTMLInputElement>('[data-hb-release-file]')
    const zone = document.querySelector<HTMLElement>('[data-testid="drop-zone"]')
    const file = input?.files?.[0]
    if (!zone || !file) throw new Error('could not prepare release drop')
    const transfer = new DataTransfer()
    transfer.items.add(file)
    zone.dispatchEvent(new DragEvent('dragenter', {
      bubbles: true,
      cancelable: true,
      dataTransfer: transfer,
    }))
    zone.dispatchEvent(new DragEvent('dragover', {
      bubbles: true,
      cancelable: true,
      dataTransfer: transfer,
    }))
    ;(window as typeof window & { __hbReleaseTransfer?: DataTransfer }).__hbReleaseTransfer = transfer
  })
}

async function finishDrop(page: Page) {
  await page.evaluate(() => {
    const zone = document.querySelector<HTMLElement>('[data-testid="drop-zone"]')
    const holder = window as typeof window & { __hbReleaseTransfer?: DataTransfer }
    if (!zone || !holder.__hbReleaseTransfer) throw new Error('release drop was not prepared')
    zone.dispatchEvent(new DragEvent('drop', {
      bubbles: true,
      cancelable: true,
      dataTransfer: holder.__hbReleaseTransfer,
    }))
    delete holder.__hbReleaseTransfer
  })
}

async function runBoard(page: Page, path: string, index: number): Promise<BoardResult> {
  const file = basename(path)
  const inputSha256 = fileDigests[resolvedFiles.indexOf(path)]
  const expectedRefusal = expectedRefusals.has(path)
  const failures: string[] = []
  const consoleErrors: string[] = []
  const wireEvents: WireEvent[] = []
  watchConsole(page, consoleErrors)
  watchWire(page, wireEvents)
  const plan = firmwarePlans[index]
  await page.goto(base!, { waitUntil: 'domcontentloaded' })
  await page.waitForSelector('[data-testid="drop-zone"]', { timeout: 30_000 })

  let firmware: FirmwareResult | null = null
  if (plan !== null) {
    const staged = await stageFirmware(page, plan, failures)
    firmware = {
      staged: true,
      loaded: staged.loaded,
      expect: plan.expect,
      file: basename(plan.path),
      detail: staged.detail,
      cosim_ran: false,
      cosim_seconds: null,
      pin_activity: false,
      serial_activity: false,
      pin_activity_rendered: null,
      analog_valid: null,
    }
  }

  await prepareDrop(page, path)
  const zone = page.locator('[data-testid="drop-zone"]')
  const armed = await zone.getAttribute('data-active')
  const armedText = await zone.innerText()
  if (armed !== 'true') failures.push(`drag target did not arm (data-active=${armed})`)
  if (!armedText.includes('Drop to check')) failures.push('drag target gave no drop feedback')

  // A board carrying firmware goes through the multipart endpoint instead.
  const analyzeEndpoint = plan === null ? '/api/analyze' : '/api/analyze-with-firmware'
  const responsePromise = page.waitForResponse(
    response => response.request().method() === 'POST' && response.url().endsWith(analyzeEndpoint),
    { timeout: 180_000 },
  )
  const started = performance.now()
  await finishDrop(page)
  const response = await responsePromise
  let responseCaptureError: string | null = null
  let report: WebReport | null = null
  try {
    report = await response.json() as WebReport
  } catch (error) {
    // Chromium can evict a large response body from the inspector cache after
    // the page itself consumes it. The downloaded JSON report below is the
    // authoritative fallback and must match what the UI rendered.
    responseCaptureError = String(error)
  }

  await page.waitForSelector('[data-testid="report-verdict"], [data-testid="upload-error"]', {
    timeout: 180_000,
  })
  const elapsed = Math.round(performance.now() - started)
  if (!response.ok()) failures.push(`analysis returned HTTP ${response.status()}`)

  let exported = false
  let refused = false
  let refusalMessage: string | null = null
  let live = { liveStarted: false, before: null as number | null, after: null as number | null }
  if (expectedRefusal) {
    // The other side of the same contract: this input must NOT analyse. The
    // audit fails just as loudly when the front door hands back a report as
    // when a readable board is refused, so the expectation cannot be satisfied
    // by inverting the behaviour.
    const before = failures.length
    const audit = await auditRefusal(page, path, report, failures)
    refusalMessage = audit.shown
    report = audit.report
    refused = failures.length === before
  } else if (await page.locator('[data-testid="report-verdict"]').count() === 1
      && await page.locator('[data-testid="export-open"]').count() === 1) {
    await page.click('[data-testid="export-open"]')
    const [download] = await Promise.all([
      page.waitForEvent('download'),
      page.click('[data-testid="export-json"]'),
    ])
    const exportPath = join(output, `${String(index + 1).padStart(2, '0')}-${file}.json`)
    await download.saveAs(exportPath)
    const exportedReport = await Bun.file(exportPath).json() as WebReport
    const independentReport = await independentAnalysis(path, plan)
    if (report !== null && canonicalJson(exportedReport) !== canonicalJson(report)) {
      failures.push('JSON export differs from the captured /api/analyze response')
    }
    if (canonicalJson(exportedReport) !== canonicalJson(independentReport)) {
      failures.push('JSON export differs from an independent repeat analysis')
    }
    if (report === null) report = independentReport
    const inventory = await page.locator('[data-testid="report-inventory"]').innerText()
    const expectedInventory = `${exportedReport.board_name || exportedReport.file_name} · `
      + `${exportedReport.num_components} ${exportedReport.num_components === 1 ? 'part' : 'parts'} · `
      + `${exportedReport.num_nets} ${exportedReport.num_nets === 1 ? 'net' : 'nets'}`
    const verdict = await page.locator('[data-testid="report-verdict"]').innerText()
    if (inventory.trim() !== expectedInventory) failures.push('rendered board inventory differs from JSON export')
    if (!verdict.includes(exportedReport.headline)) failures.push('rendered headline differs from JSON export')
    exported = canonicalJson(exportedReport) === canonicalJson(independentReport)
      && inventory.trim() === expectedInventory
      && verdict.includes(exportedReport.headline)

    // Export feedback must remain readable without obscuring the next report
    // section. This caught an absolute-positioned toast overlapping the first
    // line of the "Parts with no model" panel on a real 1440 x 900 journey.
    const confirmation = page.locator('[data-testid="export-done"]')
    await confirmation.waitFor({ state: 'visible' })
    const nextSection = page.locator('[data-testid="datasheet-extract"]')
    if (await nextSection.count() === 1) {
      const [confirmationBox, sectionBox] = await Promise.all([
        confirmation.boundingBox(),
        nextSection.boundingBox(),
      ])
      if (!confirmationBox || !sectionBox) {
        failures.push('could not measure export confirmation layout')
      } else {
        const overlaps = confirmationBox.x < sectionBox.x + sectionBox.width
          && confirmationBox.x + confirmationBox.width > sectionBox.x
          && confirmationBox.y < sectionBox.y + sectionBox.height
          && confirmationBox.y + confirmationBox.height > sectionBox.y
        if (overlaps) failures.push('export confirmation overlaps the next report section')
      }
    }
  } else {
    failures.push('JSON export is unavailable after a successful drop')
  }

  if (!expectedRefusal) {
    if (!report?.ok) failures.push(`analysis refused: ${report?.error ?? 'no report'}`)
    if (report && report.num_components <= 0 && report.num_nets <= 0) {
      failures.push('report recovered neither components nor nets')
    }
    if (!report?.headline?.trim()) failures.push('report has no useful headline')
    if (!Array.isArray(report?.sections) || report.sections.length === 0) {
      failures.push('report has no check sections')
    }
  }

  if (firmware !== null && firmware.loaded) {
    // The report has to have co-simulated the image, and the page has to show
    // it: a `cosim` payload with no rendered section is a co-sim the user
    // never sees, which is worth nothing at a bench.
    const cosim = report?.cosim ?? null
    for (const complaint of cosimFailures(cosim, firmware.expect)) failures.push(complaint)
    const section = page.locator('[data-testid="cosim-section"]')
    if (cosim?.ran === true && await section.count() !== 1) {
      failures.push('the report co-simulated firmware but rendered no co-sim section')
    }
    firmware.cosim_ran = cosim?.ran === true
    firmware.cosim_seconds = typeof cosim?.seconds_simulated === 'number'
      ? cosim.seconds_simulated
      : null
    firmware.pin_activity = cosim !== null && cosimDrovePin(cosim)
    firmware.serial_activity = cosim !== null && cosimPrinted(cosim)
    firmware.analog_valid = typeof cosim?.analog_valid === 'boolean' ? cosim.analog_valid : null
    // Driven pins are the observable half of firmware-hardware interaction, so
    // a driven row has to be on screen and legible as driven. Visibility, not
    // presence: a collapsed, hidden or zero-height row is in the DOM and cannot
    // be read, which is the same dead end as no row at all.
    firmware.pin_activity_rendered = firmware.pin_activity
      ? await section
          .getByText('driven', { exact: true })
          .first()
          .isVisible()
          .catch(() => false)
      : null
  }

  await page.screenshot({
    path: join(output, `${String(index + 1).padStart(2, '0')}-${file}-report.png`),
    fullPage: false,
  })
  // A refused input has no session to drive: there is nothing to launch, and
  // `auditRefusal` has already established that the app offers no launch.
  if (!expectedRefusal) {
    try {
      live = await exerciseLiveSimulation(page, file, failures)
    } catch (error) {
      failures.push(`live simulation journey failed: ${error instanceof Error ? error.message : String(error)}`)
    }
  }

  if (consoleErrors.length > 0) failures.push(`${consoleErrors.length} browser console error(s)`)
  await Bun.write(
    join(output, `${String(index + 1).padStart(2, '0')}-${file}.response.json`),
    JSON.stringify(report, null, 2),
  )
  await page.screenshot({
    path: join(output, `${String(index + 1).padStart(2, '0')}-${file}.png`),
    fullPage: false,
  })
  return {
    path: boardFilePaths[index],
    file,
    input_sha256: inputSha256,
    elapsed_ms: elapsed,
    response_status: response.status(),
    response_capture_error: responseCaptureError,
    report,
    exported,
    expected_refusal: expectedRefusal,
    refused,
    refusal_message: refusalMessage,
    live_started: live.liveStarted,
    sim_time_before_s: live.before,
    sim_time_after_s: live.after,
    firmware,
    wire_events: wireEvents,
    console_errors: consoleErrors,
    failures,
  }
}

const browser = await chromium.launch({ headless: true })
const results: BoardResult[] = []
try {
  for (const [index, path] of resolvedFiles.entries()) {
    const context = await browser.newContext({
      baseURL: base,
      viewport: { width: 1440, height: 900 },
      acceptDownloads: true,
      reducedMotion: 'reduce',
    })
    const page = await context.newPage()
    page.setDefaultTimeout(30_000)
    let result: BoardResult
    try {
      result = await runBoard(page, path, index)
    } catch (error) {
      // One malformed or hung board must not erase the other four journeys.
      // Preserve a complete five-row result artifact and fail at the end.
      result = {
        path: boardFilePaths[index],
        file: basename(path),
        input_sha256: fileDigests[index],
        elapsed_ms: 0,
        response_status: 0,
        response_capture_error: null,
        report: null,
        exported: false,
        expected_refusal: expectedRefusals.has(path),
        refused: false,
        refusal_message: null,
        live_started: false,
        sim_time_before_s: null,
        sim_time_after_s: null,
        firmware: firmwarePlans[index] === null ? null : {
          staged: true,
          loaded: false,
          expect: firmwarePlans[index]!.expect,
          file: basename(firmwarePlans[index]!.path),
          detail: null,
          cosim_ran: false,
          cosim_seconds: null,
          pin_activity: false,
          serial_activity: false,
          pin_activity_rendered: null,
          analog_valid: null,
        },
        wire_events: [],
        console_errors: [],
        failures: [`journey crashed: ${error instanceof Error ? error.message : String(error)}`],
      }
    }
    results.push(result)
    console.log(
      `${result.failures.length === 0 ? 'PASS' : 'FAIL'} ${result.file} `
      + `${result.elapsed_ms}ms `
      + (result.expected_refusal
        ? `refused honestly=${result.refused}`
        : `${result.report?.num_components ?? 0} parts/${result.report?.num_nets ?? 0} nets`)
      + (result.firmware === null
        ? ''
        : ` firmware=${result.firmware.file} cosim=${result.firmware.cosim_ran}`
          + ` pins=${result.firmware.pin_activity}`
          + ` serial=${result.firmware.serial_activity}`),
    )
    for (const failure of result.failures) console.log(`  - ${failure}`)
    await context.close()
  }
} finally {
  await browser.close()
}

await Bun.write(join(output, 'results.json'), JSON.stringify({ base, cohort, results }, null, 2))
const failed = results.filter(result => result.failures.length > 0)
if (failed.length > 0) {
  throw new Error(`${failed.length} of ${files.length} drag-and-drop journeys failed; see ${output}/results.json`)
}
