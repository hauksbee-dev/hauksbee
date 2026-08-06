#!/usr/bin/env bun
/** Real-browser release gate for five board drag-and-drop journeys.
 *
 * The caller supplies exactly five absolute board paths as a JSON array in
 * `HB_BOARD_FILES` and a running real Hauksbee server in `HB_E2E_BASE`. Each
 * file enters through an actual DataTransfer drop (not setInputFiles on the
 * app's picker), then has to produce a useful report and a matching JSON
 * export without browser errors. Raw response evidence and screenshots are
 * retained under `HB_E2E_OUT`.
 */

import { chromium } from 'playwright'
import type { ConsoleMessage, Page } from 'playwright'
import { basename, join } from 'node:path'
import { mkdirSync } from 'node:fs'

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
  path: string
  file: string
  elapsed_ms: number
  response_status: number
  response_capture_error: string | null
  report: WebReport | null
  exported: boolean
  console_errors: string[]
  failures: string[]
}

const base = process.env.HB_E2E_BASE
const rawFiles = process.env.HB_BOARD_FILES
const output = process.env.HB_E2E_OUT ?? join(import.meta.dir, '../../output/playwright/drag-drop')

if (!base) throw new Error('HB_E2E_BASE must name a running real Hauksbee server')
if (!rawFiles) throw new Error('HB_BOARD_FILES must be a JSON array of five absolute paths')
const files = JSON.parse(rawFiles) as unknown
if (!Array.isArray(files) || files.length !== 5 || !files.every(path => typeof path === 'string')) {
  throw new Error('HB_BOARD_FILES must contain exactly five path strings')
}

mkdirSync(output, { recursive: true })

function watchConsole(page: Page, errors: string[]) {
  page.on('console', (message: ConsoleMessage) => {
    if (message.type() === 'error') errors.push(message.text())
  })
  page.on('pageerror', error => errors.push(`pageerror: ${error.message}`))
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
  const failures: string[] = []
  const consoleErrors: string[] = []
  watchConsole(page, consoleErrors)
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
    if (report === null) report = exportedReport
    exported = exportedReport.file_name === report.file_name
      && exportedReport.num_components === report.num_components
      && exportedReport.num_nets === report.num_nets
    if (!exported) failures.push('JSON export does not match the report that was displayed')

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
    path,
    file,
    elapsed_ms: elapsed,
    response_status: response.status(),
    response_capture_error: responseCaptureError,
    report,
    exported,
    console_errors: consoleErrors,
    failures,
  }
}

const browser = await chromium.launch({ headless: true })
const results: BoardResult[] = []
try {
  for (const [index, path] of files.entries()) {
    const context = await browser.newContext({
      baseURL: base,
      viewport: { width: 1440, height: 900 },
      acceptDownloads: true,
      reducedMotion: 'reduce',
    })
    const page = await context.newPage()
    page.setDefaultTimeout(30_000)
    const result = await runBoard(page, path, index)
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

await Bun.write(join(output, 'results.json'), JSON.stringify({ base, results }, null, 2))
const failed = results.filter(result => result.failures.length > 0)
if (failed.length > 0) {
  throw new Error(`${failed.length} of 5 drag-and-drop journeys failed; see ${output}/results.json`)
}
