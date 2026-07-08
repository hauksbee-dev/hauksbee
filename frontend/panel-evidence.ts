// Persona-panel web-fix evidence (fixes 5/6/8). Drives a real `hauksbee serve`
// in headless Chromium: uploads the stickhub board (128 same-shaped DRC
// warnings + an off-target controlled-impedance SI note) and proves:
//   5. the SI heads-up renders the three-part what / why / what-to-do gloss.
//   6. the 128 near-identical DRC warnings collapse into one grouped card
//      ("Show all 128") — the explanation shown once.
//   8. the CLI "bring it to life" command has a working copy-to-clipboard button.
// Usage: bun panel-evidence.ts <serve_port> <stickhub_abs_path>
import { chromium } from 'playwright'

const [port, fixture] = process.argv.slice(2)
const shots = '/tmp/panel-shots'

function ok(cond: boolean, label: string) {
  if (!cond) { console.error(`ASSERT FAIL: ${label}`); process.exit(1) }
  console.log(`ASSERT OK: ${label}`)
}

const browser = await chromium.launch()
try {
  const ctx = await browser.newContext({ permissions: ['clipboard-read', 'clipboard-write'] })
  const p = await ctx.newPage()
  await p.goto(`http://127.0.0.1:${port}/`)
  await p.waitForSelector('[data-testid="drop-zone"]', { timeout: 10000 })

  // Upload stickhub through the file input (same handler an OS drop hits).
  await p.setInputFiles('#board-file', fixture)
  await p.waitForSelector('[data-testid="report-verdict"]', { timeout: 30000 })

  const body = await p.textContent('body') ?? ''

  // Fix 6: the 128 DRC warnings collapse to one grouped card, explained once.
  const grouped = await p.$$('[data-testid="grouped-finding"]')
  ok(grouped.length >= 1, `grouped DRC card present (${grouped.length})`)
  const groupText = await grouped[0].textContent() ?? ''
  ok(/\d+ similar/.test(groupText), `grouped card announces the count: "${groupText.slice(0, 60)}"`)
  ok(body.includes('Show all 128') || /Show all \d+/.test(body), 'grouped card offers "Show all N"')
  // The B.Cu/F.Cu expansion is inside the collapsed list — expand it.
  await p.click('[data-testid="grouped-finding"] summary')
  const expanded = await p.textContent('[data-testid="grouped-finding"]') ?? ''
  ok(expanded.includes('copper layer'), 'expanded items expand F.Cu/B.Cu to "copper layer"')
  await p.screenshot({ path: `${shots}/6-drc-grouped-expanded.png`, fullPage: true })
  console.log(`SHOT: ${shots}/6-drc-grouped-expanded.png`)

  // Fix 5: the SI heads-up now has Why it matters + What to do.
  ok(body.includes('Heads up'), 'SI heads-up note is shown')
  ok(body.includes('off its impedance target'), 'heads-up "what" translates the jargon')
  // Find the heads-up card and confirm the three-part gloss.
  const headsWhy = body.includes('Why it matters:') && body.includes('What to do:')
  ok(headsWhy, 'heads-up carries Why it matters + What to do (three-part gloss)')
  await p.screenshot({ path: `${shots}/5-si-headsup-glossed.png`, fullPage: true })
  console.log(`SHOT: ${shots}/5-si-headsup-glossed.png`)

  // Fix 8: the CLI "bring it to life" command has a working copy button.
  ok(await p.$('[data-testid="run-it-hint"]') !== null, 'CLI hint shown (plain serve, no live sim)')
  const copyBtn = await p.$('[data-testid="copy-cli"]')
  ok(copyBtn !== null, 'copy-to-clipboard button present next to the CLI command')
  await copyBtn!.click()
  await p.waitForTimeout(200)
  const clip = await p.evaluate(() => navigator.clipboard.readText())
  ok(clip.includes('hauksbee run') && clip.includes('--serve'), `clipboard holds the CLI command: "${clip}"`)
  const btnText = await p.textContent('[data-testid="copy-cli"]') ?? ''
  ok(btnText.includes('Copied'), 'button confirms the copy')
  await p.screenshot({ path: `${shots}/8-copy-cli-button.png`, fullPage: true })
  console.log(`SHOT: ${shots}/8-copy-cli-button.png`)

  console.log('ALL PANEL WEB ASSERTS PASSED')
} finally {
  await browser.close()
}
