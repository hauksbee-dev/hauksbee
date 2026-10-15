// What the visual lint looks at: the viewports, and the surfaces reached in
// the app to produce them.
//
// Adding a surface is one entry in APP_SURFACES. `reach` runs on a page that is
// already sitting on the surface BEFORE it in the array (the app keeps views
// mounted, so the chain is cheap and matches how a person actually walks
// through it). Keep each `reach` to the clicks a user would make.

import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'
import type { Page } from 'playwright'
import { composeChecks, openApp, openExportMenu } from '../e2e/harness'

const here = dirname(fileURLToPath(import.meta.url))

export type Viewport = { name: string; width: number; height: number }

export const VIEWPORTS: Viewport[] = [
  // The three real shapes.
  { name: '360x780', width: 360, height: 780 },
  { name: '768x1024', width: 768, height: 1024 },
  { name: '1440x900', width: 1440, height: 900 },
  // Two stress ratios. 320 wide is the narrowest phone still in use and the
  // first width anything fixed-size breaks at; 1920x600 is a short, wide
  // window, where sticky chrome eats the whole viewport.
  { name: '320x568', width: 320, height: 568 },
  { name: '1920x600', width: 1920, height: 600 },
]

export type Surface = {
  /** Stable id: names the screenshot and every violation line. */
  id: string
  /** What a reader of the report should picture. */
  what: string
  /** Walk from the previous surface's state to this one. */
  reach: (page: Page) => Promise<void>
  /** Rules to downgrade to `info` here, with the reason. Use sparingly: the
   *  point of the lint is that the exceptions are written down. */
  allow?: { rule: string; selector: RegExp; why: string }[]
}

const settle = (page: Page, ms = 350) => page.waitForTimeout(ms)

export const APP_SURFACES: Surface[] = [
  {
    id: 'landing',
    what: 'the web front door: drop zone, firmware jack, the three samples',
    async reach(page) {
      await openApp(page)
      await page.waitForSelector('[data-testid="samples"]')
      await settle(page)
    },
  },
  {
    id: 'report',
    what: 'the watchy report: verdict, 2D board map, 50 grouped DRC findings',
    async reach(page) {
      await page.click('[data-testid="sample-watchy"]')
      await page.waitForSelector('[data-testid="report-verdict"]', { timeout: 30_000 })
      // The board map draws on a canvas after the report lands.
      await settle(page, 1200)
    },
  },
  {
    id: 'report-findings-expanded',
    what: 'the report with the grouped "50 similar" DRC card opened',
    async reach(page) {
      // A clean board has neither card, and that is not a lint failure: the
      // surface just has nothing extra to open.
      const summary = page.locator('[data-testid="grouped-finding"] summary').first()
      if (await summary.count() > 0) {
        await summary.click()
        await settle(page)
      }
      for (const sel of ['[data-testid="grouped-finding"]', '[data-testid="finding-card"]']) {
        const first = page.locator(sel).first()
        if (await first.count() > 0) {
          await first.scrollIntoViewIfNeeded()
          break
        }
      }
      await settle(page)
    },
  },
  {
    id: 'datasheet-panel',
    what: 'the "parts with no model" panel: open parts and the drafting jack',
    async reach(page) {
      // The watchy sample is fully bound (no unbound active part), so its
      // report correctly renders no datasheet panel. Upload the engine's own
      // open-active-IC fixture board, whose report is the panel's habitat.
      await openApp(page)
      const board = join(here, '../../../crates/hauksbee-engine/tests/fixtures/plain_check_open_active_ic.kicad_pcb')
      await page.setInputFiles('#board-file', board)
      await page.waitForSelector('[data-testid="report-verdict"]', { timeout: 30_000 })
      await page.locator('[data-testid="datasheet-extract"]').scrollIntoViewIfNeeded()
      await settle(page)
    },
  },
  {
    id: 'datasheet-extract-form',
    what: 'datasheet extraction past the consent gate: part number, kind, file jack',
    async reach(page) {
      // Dispatch directly: the narrow phone layout scrolls and replaces this
      // row as the active card mounts, which can make Playwright keep waiting
      // for a now-detached click target even though the click already landed.
      // The guessed-parts group starts collapsed; open it as a user would.
      const closedParts = page.locator('[data-testid="guessed-parts"]:not([open]) > summary')
      if (await closedParts.count()) await closedParts.click()
      await page.locator('[data-testid^="extract-start-"]').first().dispatchEvent('click')
      // The consent gate renders once /api/models/extract/ready answers, and a
      // real engine answers it by asking codex whether it is logged in, which
      // is slow. Wait for the gate before looking for its button, or the flow
      // silently stops on the consent card.
      await page.waitForSelector(
        '[data-testid="extract-consent"], [data-testid="extract-blocked"], [data-testid="extract-unavailable"]',
        { timeout: 20_000 })
      const accept = page.locator('[data-testid="extract-consent-accept"]')
      if (await accept.count() > 0) {
        await accept.click()
        await page.waitForSelector('[data-testid="extract-part"]')
      }
      await settle(page)
    },
  },
  {
    id: 'write-part-kind',
    what: 'the "Model U1" surface opening on Write it, an identity-only scaffold that validates',
    async reach(page) {
      const close = page.locator('[data-testid="extract-close"]')
      if (await close.count() > 0) await close.first().click()
      await page.locator('[data-testid="write-part-open"]').scrollIntoViewIfNeeded()
      const opener = await page.locator('[data-testid="write-part-open"]').textContent()
      if (!opener?.includes('Model U1')) {
        throw new Error(`manual model editor did not name its selected part: ${opener}`)
      }
      await page.click('[data-testid="write-part-open"]')
      // The editor opens on a scaffold that already validates (identity only),
      // with the kind picker above it for a model that simulates.
      await page.waitForSelector('[data-testid="kind-picker"]')
      await page.waitForSelector('[data-testid="write-part-status"]:has-text("Valid")', { timeout: 10_000 })
      await page.locator('[data-testid="kind-picker"]').scrollIntoViewIfNeeded()
      await settle(page)
    },
  },
  {
    id: 'write-part',
    what: 'the "write a part yourself" editor, TOML with the live validator',
    async reach(page) {
      await page.selectOption('[data-testid="kind-select"]', 'digital')
      // The scaffold is requested from the server, and the box is busy (and
      // still holds the generic starter) until it lands.
      await page.waitForSelector('[data-testid="write-part-toml"]:not([aria-busy="true"])')
      const name = await page.locator('[data-testid="write-part-name"]').inputValue()
      const starter = await page.locator('[data-testid="write-part-toml"]').inputValue()
      if (name !== 'XLOGIC9999' || !starter.includes('id = "u1_xlogic9999"') ||
          !starter.includes('kind = "digital"') || !starter.includes('value_re = "^XLOGIC9999$"')) {
        throw new Error(`manual model editor did not scaffold the selected U1: name=${name}, starter=${starter}`)
      }
      // The validator is debounced 400ms; wait for its verdict to land so the
      // status row is measured with real text in it.
      await page.waitForSelector('[data-testid="write-part-status"]')
      await settle(page, 700)
    },
  },
  {
    id: 'write-part-spice',
    what: 'the same editor on its SPICE tab',
    async reach(page) {
      await page.selectOption('[data-testid="kind-select"]', 'spice')
      await settle(page, 700)
    },
  },
  {
    id: 'checks-builder',
    what: 'the Checks spec builder with two rules added and the spec.toml pane',
    async reach(page) {
      const close = page.locator('[data-testid="write-part-close"]')
      if (await close.count() > 0) await close.click()
      // The second rule puts a net field and numeric inputs in the layout.
      await composeChecks(page, 350)
    },
  },
  {
    id: 'checks-results',
    what: 'the Checks panel after a run, with per-row pass/fail annotations',
    async reach(page) {
      await page.click('[data-testid="run-checks"]')
      await page.waitForSelector('[data-testid="run-details-toggle"], [data-testid="builder-validation"]', { timeout: 30_000 })
      await settle(page)
    },
  },
  {
    id: 'ci-setup',
    what: 'the GitHub CI setup: the workflow YAML and both download buttons',
    async reach(page) {
      const panel = page.locator('[data-testid="ci-setup-panel"]')
      if (!(await panel.isVisible())) {
        // The report's Next card keeps its own, hidden "Set up GitHub CI" on
        // the Board view, so take the one a user can see.
        await page.locator('button:visible', { hasText: 'Set up GitHub CI' }).first().click()
      }
      await panel.waitFor({ state: 'visible' })
      await panel.scrollIntoViewIfNeeded()
      await settle(page)
    },
  },
  {
    id: 'environment-deps',
    what: 'the Environment page: backend/oracle cards, install buttons, versions',
    async reach(page) {
      await page.click('[data-testid="nav-env"]')
      await page.waitForSelector('[data-testid="deps-panel"], [data-testid="deps-unavailable"]', { timeout: 20_000 })
      await settle(page, 600)
    },
  },
  {
    id: 'export-menu',
    what: 'the Export menu open in the result panel action bar: six files, each named',
    async reach(page) {
      await page.click('[data-testid="nav-board"]')
      await openExportMenu(page)
      await settle(page)
    },
  },
  {
    id: 'session-switcher',
    what: 'the saved-session switcher: rows with rename/delete, and the caveat',
    async reach(page) {
      // Escape first: the export menu is still open, and its own dismissal is
      // part of what this chain is checking.
      await page.keyboard.press('Escape')
      await page.click('[data-testid="session-indicator"]')
      await page.waitForSelector('[data-testid="session-switcher"]')
      await page.waitForSelector('[data-testid="session-row"]')
      await settle(page)
    },
  },
  {
    id: 'session-switcher-renaming',
    what: 'the same panel with a row in its rename state (input + two buttons)',
    async reach(page) {
      await page.locator('[data-testid="session-rename"]').first().click()
      await page.waitForSelector('[data-testid="session-rename-input"]')
      await settle(page)
    },
  },
  {
    id: 'session-resume',
    what: 'the landing page on a revisit: the resume offer above the drop zone',
    // LAST in the chain on purpose: it reloads, which throws away every state
    // the surfaces above walked to.
    async reach(page) {
      await page.goto('/', { waitUntil: 'domcontentloaded' })
      await page.waitForSelector('[data-testid="session-resume"]', { timeout: 20_000 })
      await settle(page)
    },
  },
]

/** The marketing site. Optional and tolerant on purpose: it is a separate
 *  build, it is edited independently of the app, and a missing or broken site
 *  build must not turn the app's lint red. */
export const SITE_SURFACES: Surface[] = [
  {
    id: 'site-top',
    what: 'hauksbee.dev masthead, hero and the first plates',
    async reach(page) {
      await page.goto('/', { waitUntil: 'load' })
      // Fonts change every measurement in here, so wait for them.
      await page.evaluate(() => document.fonts.ready)
      await settle(page, 800)
    },
  },
]
