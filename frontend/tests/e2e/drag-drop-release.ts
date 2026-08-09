#!/usr/bin/env bun
/** Real-browser release gate for board drag-and-drop journeys.
 *
 * The external cohort supplies exactly five absolute board paths; the corpus
 * cohort supplies every discovered board input. Each
 * file enters through an actual DataTransfer drop (not setInputFiles on the
 * app's picker), then has to produce a useful report, a matching independent
 * JSON export, and a live session whose pause/step/play controls advance the
 * engine clock without browser errors. Redacted wire evidence and screenshots
 * are retained under `HB_E2E_OUT`.
 */

import { chromium } from 'playwright'
import type { ConsoleMessage, Page } from 'playwright'
import { basename, isAbsolute, join } from 'node:path'
import { mkdirSync, realpathSync } from 'node:fs'

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
  live_started: boolean
  sim_time_before_s: number | null
  sim_time_after_s: number | null
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
const cohort = process.env.HB_RELEASE_COHORT ?? 'external'
const output = process.env.HB_E2E_OUT ?? join(import.meta.dir, '../../output/playwright/drag-drop')

if (!base) throw new Error('HB_E2E_BASE must name a running real Hauksbee server')
if (!rawFiles) throw new Error('HB_BOARD_FILES must be a JSON array of absolute paths')
const files = JSON.parse(rawFiles) as unknown
if (cohort !== 'external' && cohort !== 'corpus') {
  throw new Error('HB_RELEASE_COHORT must be external or corpus')
}
if (!Array.isArray(files)
    || files.length === 0
    || (cohort === 'external' && files.length !== 5)
    || !files.every(path => typeof path === 'string')
    || new Set(files).size !== files.length) {
  throw new Error(
    cohort === 'external'
      ? 'HB_BOARD_FILES must contain exactly five distinct path strings'
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

function canonicalJson(value: unknown): string {
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(',')}]`
  if (value !== null && typeof value === 'object') {
    const object = value as Record<string, unknown>
    return `{${Object.keys(object).sort().map(key => `${JSON.stringify(key)}:${canonicalJson(object[key])}`).join(',')}}`
  }
  return JSON.stringify(value)
}

async function independentAnalysis(path: string): Promise<WebReport> {
  const response = await fetch(new URL('/api/analyze', base!), {
    method: 'POST',
    headers: {
      'Content-Type': 'application/octet-stream',
      'X-Board-Filename': basename(path),
    },
    body: Bun.file(path),
  })
  if (!response.ok) throw new Error(`independent analysis returned HTTP ${response.status}`)
  return await response.json() as WebReport
}

function parseSimTime(text: string): number {
  const match = text.trim().match(/^([0-9]+(?:\.[0-9]+)?)\s*(µs|ms|s)$/)
  if (!match) throw new Error(`unrecognised simulation time: ${JSON.stringify(text)}`)
  const value = Number(match[1])
  if (match[2] === 'µs') return value / 1_000_000
  if (match[2] === 'ms') return value / 1_000
  return value
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
  const failures: string[] = []
  const consoleErrors: string[] = []
  const wireEvents: WireEvent[] = []
  watchConsole(page, consoleErrors)
  watchWire(page, wireEvents)
  await page.goto(base!, { waitUntil: 'domcontentloaded' })
  await page.waitForSelector('[data-testid="drop-zone"]', { timeout: 30_000 })

  await prepareDrop(page, path)
  const zone = page.locator('[data-testid="drop-zone"]')
  const armed = await zone.getAttribute('data-active')
  const armedText = await zone.innerText()
  if (armed !== 'true') failures.push(`drag target did not arm (data-active=${armed})`)
  if (!armedText.includes('Drop to analyze')) failures.push('drag target gave no drop feedback')

  const responsePromise = page.waitForResponse(
    response => response.request().method() === 'POST' && response.url().endsWith('/api/analyze'),
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
  if (await page.locator('[data-testid="report-verdict"]').count() === 1
      && await page.locator('[data-testid="export-open"]').count() === 1) {
    await page.click('[data-testid="export-open"]')
    const [download] = await Promise.all([
      page.waitForEvent('download'),
      page.click('[data-testid="export-json"]'),
    ])
    const exportPath = join(output, `${String(index + 1).padStart(2, '0')}-${file}.json`)
    await download.saveAs(exportPath)
    const exportedReport = await Bun.file(exportPath).json() as WebReport
    const independentReport = await independentAnalysis(path)
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

  if (!report?.ok) failures.push(`analysis refused: ${report?.error ?? 'no report'}`)
  if (report && report.num_components <= 0 && report.num_nets <= 0) {
    failures.push('report recovered neither components nor nets')
  }
  if (!report?.headline?.trim()) failures.push('report has no useful headline')
  if (!Array.isArray(report?.sections) || report.sections.length === 0) {
    failures.push('report has no check sections')
  }

  await page.screenshot({
    path: join(output, `${String(index + 1).padStart(2, '0')}-${file}-report.png`),
    fullPage: false,
  })
  let live = { liveStarted: false, before: null as number | null, after: null as number | null }
  try {
    live = await exerciseLiveSimulation(page, file, failures)
  } catch (error) {
    failures.push(`live simulation journey failed: ${error instanceof Error ? error.message : String(error)}`)
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
    live_started: live.liveStarted,
    sim_time_before_s: live.before,
    sim_time_after_s: live.after,
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
        live_started: false,
        sim_time_before_s: null,
        sim_time_after_s: null,
        wire_events: [],
        console_errors: [],
        failures: [`journey crashed: ${error instanceof Error ? error.message : String(error)}`],
      }
    }
    results.push(result)
    console.log(
      `${result.failures.length === 0 ? 'PASS' : 'FAIL'} ${result.file} `
      + `${result.elapsed_ms}ms ${result.report?.num_components ?? 0} parts/`
      + `${result.report?.num_nets ?? 0} nets`,
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
