// W6 §1 browser evidence: drives the unified web experience end-to-end in real
// headless Chromium (Playwright). Four flows:
//   1. `hauksbee serve` lands on the drop-zone landing (screenshot).
//   2. Uploading a fixture board through the landing's file input renders the
//      plain-language report with real section data (screenshot).
//   3. `hauksbee run <fixture> --serve` lands preloaded on that board's report
//      with the "run it" affordance (screenshot).
//   4. Clicking "run it" transitions to the live-sim view (transport bar, board
//      canvas, WebSocket-connected) (screenshot).
// Usage: bun w6-evidence.ts <serve_port> <run_serve_port> <fixture_abs_path>
import { chromium } from 'playwright'

const [servePort, runServePort, fixture] = process.argv.slice(2)
const shots = '/tmp/w6-shots'

function ok(cond: boolean, label: string) {
  if (!cond) {
    console.error(`ASSERT FAIL: ${label}`)
    process.exit(1)
  }
  console.log(`ASSERT OK: ${label}`)
}

const browser = await chromium.launch()
try {
  // ---- Flow 1: serve -> drop-zone landing ----
  const p1 = await browser.newPage()
  await p1.goto(`http://127.0.0.1:${servePort}/`)
  await p1.waitForSelector('[data-testid="drop-zone"]', { timeout: 10000 })
  const body1 = await p1.textContent('body') ?? ''
  ok(body1.includes('Click to choose a board file, or drop one here'), 'serve landing shows the drop zone')
  ok(body1.includes('Nothing leaves your machine'), 'serve landing keeps the local-only promise')
  ok(await p1.$('[data-testid="report"]') === null, 'serve landing starts with no report (no preloaded board)')
  await p1.screenshot({ path: `${shots}/1-serve-dropzone.png`, fullPage: true })
  console.log(`SHOT: ${shots}/1-serve-dropzone.png`)

  // ---- Flow 2: upload a fixture through the landing (file input; a synthetic
  // OS drag-drop is impractical headless, the input drives the same handler) ----
  await p1.setInputFiles('#board-file', fixture)
  await p1.waitForSelector('[data-testid="report-verdict"]', { timeout: 30000 })
  const verdict2 = await p1.textContent('[data-testid="report-verdict"]') ?? ''
  const body2 = await p1.textContent('body') ?? ''
  ok(verdict2.trim().length > 0, `upload produced a verdict: "${verdict2.trim().slice(0, 90)}"`)
  ok(body2.includes('Copper spacing (DRC)'), 'report renders the DRC section')
  ok(body2.includes('Connectivity & wiring'), 'report renders the connectivity section')
  ok(body2.includes('Signal integrity'), 'report renders the SI section')
  ok(body2.includes('Why it matters:') || body2.includes('Looks healthy'), 'findings render the what/why/fix cards (or a clean verdict)')
  ok(await p1.$('[data-testid="run-it-hint"]') !== null, 'drop-path report shows the CLI hint (no live sim on serve)')
  await p1.screenshot({ path: `${shots}/2-serve-report.png`, fullPage: true })
  console.log(`SHOT: ${shots}/2-serve-report.png`)
  await p1.close()

  // ---- Flow 3: run --serve -> preloaded report view ----
  const p2 = await browser.newPage()
  await p2.goto(`http://127.0.0.1:${runServePort}/`)
  await p2.waitForSelector('[data-testid="report-verdict"]', { timeout: 30000 })
  const body3 = await p2.textContent('body') ?? ''
  ok(body3.includes('parts') && body3.includes('nets'), 'preloaded report shows board meta (parts/nets)')
  ok(body3.includes('Copper spacing (DRC)'), 'preloaded report renders real section data')
  ok(await p2.$('[data-testid="run-it"]') !== null, 'preloaded report shows the "run it" affordance')
  await p2.screenshot({ path: `${shots}/3-runserve-preloaded-report.png`, fullPage: true })
  console.log(`SHOT: ${shots}/3-runserve-preloaded-report.png`)

  // ---- Flow 4: "run it" expands into the live-sim view ----
  await p2.click('[data-testid="run-it"]')
  await p2.waitForSelector('canvas', { timeout: 20000 })
  const body4 = await p2.textContent('body') ?? ''
  ok(await p2.$('canvas') !== null, 'sim view renders the board canvas')
  ok(body4.toLowerCase().includes('sim:'), 'sim view shows the transport/status bar (sim time)')
  // The sim WebSocket actually connected: BoardInfo populates the comp/nets footer.
  await p2.waitForFunction(() => /\d+ comp · \d+ nets/.test(document.body.textContent ?? ''), undefined, { timeout: 15000 })
  const footer = (await p2.textContent('body') ?? '').match(/\d+ comp · \d+ nets/)?.[0]
  ok(!!footer, `sim WebSocket delivered BoardInfo: "${footer}"`)
  await p2.screenshot({ path: `${shots}/4-run-it-live-sim.png`, fullPage: true })
  console.log(`SHOT: ${shots}/4-run-it-live-sim.png`)
  await p2.close()

  console.log('ALL FLOWS PASSED')
} finally {
  await browser.close()
}
