#!/usr/bin/env bun
// Dropping a SPICE deck in a real browser, against either oracle — they differ
// only in where the expected CSV comes from, so it is one driver:
//   bun run tests/e2e/deck-sim.ts                    # fixture server + fixture CSV
//   HB_E2E_BASE=http://127.0.0.1:3001 bun run …      # an app already running
//   HAUKSBEE_BIN=target/debug/hauksbee bun run …     # real serve + `hauksbee sim`
//
// What this proves that no unit test can: a `.cir` dropped on the BOARD zone
// reaches the deck panel and not the board extractor, the deck's own analysis is
// named, the sweep paints with a readable axis, the export is the server's CSV
// bytes unaltered, and the page does not scroll sideways at 360px.

import { browserFlow, ok, openApp, terminateProcess, withFixture } from './harness'
import { freePort, waitForStartup } from './live-server'
import { readFileSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const here = dirname(fileURLToPath(import.meta.url))
const repo = resolve(here, '../../..')
const OUT = process.env.HB_E2E_OUT ?? join(here, '../../test-results/e2e-deck-sim')
const DECK = resolve(repo, 'crates/hauksbee-solve/tests/decks/rc_ac_bode.cir')
const BIN = process.env.HAUKSBEE_BIN ? resolve(repo, process.env.HAUKSBEE_BIN) : null

/** The exact CSV the export must equal: the captured fixture, or the CLI's
 *  stdout taken before the browser starts. */
let expectedCsv = ''

/** Hands `run` a base URL, having set `expectedCsv` for that target. */
async function withTarget(run: (base: string) => Promise<void>) {
  if (!BIN) {
    const fixture = join(here, '../visual-lint/fixtures/sim-ac.json')
    expectedCsv = (JSON.parse(readFileSync(fixture, 'utf8')) as { csv: string }).csv
    return withFixture(run)
  }
  const cli = Bun.spawnSync([BIN, 'sim', DECK], { cwd: repo })
  if (cli.exitCode !== 0) throw new Error(`hauksbee sim failed (${cli.exitCode}): ${cli.stderr.toString()}`)
  expectedCsv = cli.stdout.toString()
  const port = await freePort()
  const server = Bun.spawn([BIN, 'serve', '--port', String(port), '--no-open'], { cwd: repo, stdout: 'inherit', stderr: 'inherit' })
  try {
    const base = `http://127.0.0.1:${port}`
    await waitForStartup(base, server, 60_000)
    await run(base)
  } finally {
    await terminateProcess(server)
  }
}

await browserFlow({
  out: OUT,
  viewport: { width: 1280, height: 900 },
  timeout: BIN ? 60_000 : 20_000,
  ignore: /Failed to load resource|net::ERR_/,
  acceptDownloads: true,
  host: withTarget,
}, async page => {
  await openApp(page)
  await page.locator('#board-file').setInputFiles(DECK)

  await page.waitForSelector('[data-testid="deck-run"]')
  ok('a .cir dropped on the board zone opens the deck panel, not the board extractor', true)
  await page.click('[data-testid="deck-run"]')
  await page.waitForSelector('[data-testid="deck-result"]')

  ok('the analysis is named from the deck itself',
    (await page.locator('[data-testid="deck-analysis"]').textContent())?.includes('AC') === true)
  // The deck names its own `.print`, so the engine's one note is the nominal-values
  // caveat (the old "no `.print` card" note is gone); both oracles carry it.
  ok("the solver's notes are shown beside the numbers",
    (await page.locator('[data-testid="deck-notes"]').textContent())?.includes('no uncertainty interval') === true)
  const firstCell = await page.locator('[data-testid="deck-result"] tbody tr').first().locator('td').first().textContent()
  ok("the table carries the sweep's own first x value",
    Number.parseFloat(firstCell ?? '') === Number.parseFloat(expectedCsv.split('\n')[1].split(',')[0]), firstCell ?? '')
  ok('both AC panels (magnitude and phase) painted',
    await page.locator('[data-testid="deck-result"] canvas').count() === 2)
  // The plot is dB and the table is the CLI's linear magnitude; each says so.
  ok('the dB plot and the V/V table are each labelled',
    (await page.locator('[data-testid="plot-y-caption"]').first().textContent())?.includes('dB') === true
    && (await page.locator('[data-testid="deck-result"] thead').textContent())?.includes('magnitude, V/V') === true)
  await page.screenshot({ path: join(OUT, '01-deck-ac-1280.png') })

  const download = await Promise.all([
    page.waitForEvent('download'),
    page.click('[data-testid="deck-export"]'),
  ]).then(([d]) => d)
  ok('the export is named after the deck', download.suggestedFilename() === 'rc_ac_bode.csv',
    download.suggestedFilename())
  const bytes = readFileSync(await download.path(), 'utf8')
  ok("and is the server's own CSV bytes, unaltered", bytes === expectedCsv,
    `${bytes.length} bytes vs ${expectedCsv.length}`)

  await page.setViewportSize({ width: 360, height: 780 })
  await page.waitForTimeout(400)
  const overflow = await page.evaluate(() =>
    document.documentElement.scrollWidth - document.documentElement.clientWidth)
  ok('no horizontal page scroll at 360px', overflow <= 1, `${overflow}px`)
  await page.screenshot({ path: join(OUT, '02-deck-ac-360.png'), fullPage: true })
})
