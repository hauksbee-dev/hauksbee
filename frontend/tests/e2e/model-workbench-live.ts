#!/usr/bin/env bun
// Real-product browser proof for the board -> model -> co-sim workbench.
// Unlike the visual fixture flows, this starts an actual `hauksbee serve`,
// uploads exact board/firmware bytes, exercises the local model draft, and
// waits for the engine-correlated register-map attachment receipt.
//
// Usage:
//   HAUKSBEE_BIN=target/debug/hauksbee HB_MODEL_BOARD=/path/to/watchy-v1.kicad_pcb \
//     bun run frontend/tests/e2e/model-workbench-live.ts
// Requires Watchy v1 (U4=BMA423, MCU=ESP32PICO-D4) with matching ESP32 firmware.
// The bundled firmware targets v1; the public Watchy v3 sample and blinky are
// not substitutes. HB_MODEL_FIRMWARE may select another matching v1 image.

import { chromium } from 'playwright'
import { accessSync, existsSync, mkdirSync, readFileSync, renameSync, writeFileSync } from 'node:fs'
import { dirname, relative, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { terminateProcess } from './harness'
import { freePort, waitForStartup } from './live-server'

const here = dirname(fileURLToPath(import.meta.url))
const repo = resolve(here, '../../..')
const binary = resolve(repo, process.env.HAUKSBEE_BIN ?? 'target/debug/hauksbee')
const board = process.env.HB_MODEL_BOARD ? resolve(repo, process.env.HB_MODEL_BOARD) : ''
const firmware = resolve(repo, process.env.HB_MODEL_FIRMWARE ?? 'testdata/firmware/watchy_display_init/flash.bin')
const sensorRef = process.env.HB_MODEL_SENSOR_REF ?? 'U4'
const bundledLm75 = resolve(repo, 'testdata/sensor-specs/lm75.toml')
const evidencePath = process.env.HB_MODEL_EVIDENCE
  ? resolve(repo, process.env.HB_MODEL_EVIDENCE)
  : null

function requireFile(path: string, label: string) {
  try { accessSync(path) } catch { throw new Error(`${label} is missing: ${path}`) }
}

function sha256File(path: string): string {
  return new Bun.CryptoHasher('sha256').update(readFileSync(path)).digest('hex')
}

function writeEvidence(path: string, value: unknown) {
  const local = relative(repo, path)
  if (local.startsWith('..') || local === '') throw new Error(`evidence path must stay under the repository: ${path}`)
  if (existsSync(path) && process.env.HB_MODEL_EVIDENCE_FORCE !== '1') {
    throw new Error(`evidence path exists; set HB_MODEL_EVIDENCE_FORCE=1 to replace it: ${path}`)
  }
  mkdirSync(dirname(path), { recursive: true })
  const temporary = `${path}.tmp-${process.pid}`
  writeFileSync(temporary, `${JSON.stringify(value, null, 2)}\n`)
  renameSync(temporary, path)
}

async function main() {
  if (!board) {
    throw new Error('HB_MODEL_BOARD is required: supply a Watchy v1 board (U4=BMA423, ESP32PICO-D4) and matching firmware; the public v3 sample and blinky are not substitutes.')
  }
  const started = performance.now()
  requireFile(binary, 'Hauksbee binary')
  requireFile(board, 'Watchy board')
  requireFile(firmware, 'Watchy firmware')

  const port = await freePort()
  const base = `http://127.0.0.1:${port}`
  const server = Bun.spawn([binary, 'serve', '--port', String(port), '--no-open'], {
    cwd: repo,
    stdout: 'inherit',
    stderr: 'inherit',
  })
  const browser = await chromium.launch({ headless: true })
  try {
    await waitForStartup(base, server)
    const page = await browser.newPage()
    const consoleErrors: string[] = []
    page.on('pageerror', error => consoleErrors.push(error.message))
    page.on('console', message => {
      if (message.type() === 'error' && !/favicon|Failed to load resource/.test(message.text())) {
        consoleErrors.push(message.text())
      }
    })
    await page.goto(base)

    // Exact real input bytes, through the same hidden inputs the visible drop
    // target and firmware jack own.
    await page.locator('#board-file').setInputFiles(board)
    await page.getByTestId(`model-coverage-${sensorRef}`).waitFor({ state: 'visible', timeout: 60_000 })
    await page.locator('#firmware-file').setInputFiles(firmware)
    await page.getByTestId(`model-coverage-${sensorRef}`).waitFor({ state: 'visible', timeout: 60_000 })

    // Read-only draft: exact working BMA behavior is preserved; no Save click.
    await page.getByTestId(`model-author-${sensorRef}`).click()
    const draft = page.getByTestId('write-part-toml')
    await draft.waitFor({ state: 'visible' })
    await page.getByText('Copied the existing executable model. Review the declared gaps, edit, validate, then choose Save.').waitFor()
    const draftText = await draft.inputValue()
    if (!draftText.includes('kind = "register_map"')
      || !draftText.includes('required_high_roles = ["csb"]')
      || !draftText.includes('address_select_role = "sdo"')) {
      throw new Error(`BMA423 browser draft did not preserve strap-aware executable behavior:\n${draftText.slice(0, 2400)}`)
    }
    await page.getByTestId('write-part-close').click()

    // The part already owns a partial register map, so the click path must not
    // offer a duplicate same-address virtual device.
    await page.getByTestId(`model-coverage-${sensorRef}`).locator('button').first().click()
    await page.getByTestId('selection-register-map-owned').waitFor({ state: 'visible' })

    // Launch the actual firmware-backed live session, return to the Checks
    // builder, and add a distinct lab peripheral from the checked-in behavior
    // catalog. This is the ordinary no-LLM path: no local file hunt and no
    // model-library write.
    await page.getByTestId('run-it').click()
    // Presence of the status bar proves the launch callback has mounted and
    // connected the real SimView. It may legitimately begin paused.
    await page.getByTestId('sim-run-state').waitFor({ timeout: 60_000 })
    await page.getByTestId('nav-checks').click()
    await page.getByRole('button', { name: '+ bus device' }).click()
    const sensorRow = page.getByTestId('sensor-1')
    await sensorRow.getByLabel('sensor id').fill('LAB_LM75')
    await sensorRow.getByLabel('bundled sensor behavior').selectOption('lm75')
    await sensorRow.getByText('loaded bundled:lm75').waitFor()
    await sensorRow.getByTestId(/sensor-attach-live-/).click()

    const receipt = sensorRow.getByTestId(/sensor-live-result-/)
    await receipt.waitFor({ state: 'visible' })
    await receipt.getByText('Attached exact register-map bytes for LAB_LM75 to the live co-simulation.').waitFor({ timeout: 15_000 })
    const acceptedMessage = (await receipt.textContent())?.trim() ?? ''

    // A second identical id must be a typed, correlated refusal in this same
    // row—not a hidden console message and not stale earlier success.
    await sensorRow.getByTestId(/sensor-attach-live-/).click()
    await receipt.getByText(/already attached|already exists|duplicate/i).waitFor({ timeout: 15_000 })
    const refusedMessage = (await receipt.textContent())?.trim() ?? ''

    if (consoleErrors.length) {
      throw new Error(`browser console errors: ${consoleErrors.join(' | ')}`)
    }
    if (evidencePath) {
      const version = Bun.spawnSync([binary, '--version'], { cwd: repo }).stdout.toString().trim()
      writeEvidence(evidencePath, {
        schema_version: 1,
        evidence_class: 'source_bound_simulation_workflow',
        assessment: 'browser_workflow_only_not_hardware_validation',
        generated_at_unix_s: Math.floor(Date.now() / 1000),
        elapsed_ms: Number((performance.now() - started).toFixed(3)),
        binary: relative(repo, binary),
        binary_sha256: sha256File(binary),
        hauksbee_version: version,
        board: { path: relative(repo, board), sha256: sha256File(board), component: sensorRef },
        firmware: { path: relative(repo, firmware), sha256: sha256File(firmware) },
        bundled_behavior: { id: 'lm75', source_path: relative(repo, bundledLm75), sha256: sha256File(bundledLm75) },
        assertions: {
          exact_partial_model_draft_preserved: true,
          strap_aware_model_owned_behavior_visible: true,
          bundled_behavior_selected_without_file_picker: true,
          live_attach_accepted: acceptedMessage,
          duplicate_attach_refused: refusedMessage,
          browser_console_errors: consoleErrors,
          llm_used: false,
          model_saved: false,
        },
      })
    }
    console.log('PASS real model workbench: exact draft, strap-aware auto model, bundled no-LLM behavior, firmware live launch, correlated attach and refusal receipts')
  } finally {
    await browser.close()
    await terminateProcess(server)
  }
}

await main().catch(error => {
  console.error(`FAIL model workbench: ${error instanceof Error ? error.message : String(error)}`)
  process.exitCode = 1
})
