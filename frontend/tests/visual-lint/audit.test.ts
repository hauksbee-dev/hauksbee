// The audit's own tests: planted defects, in a real browser.
//
// A layout rule that fires on nothing is indistinguishable from a layout rule
// that is broken, and the app being clean is not evidence either way. Each case
// below is a box that IS wrong in one specific way, plus (where it matters) the
// legitimate pattern that looks similar and must stay quiet. When a rule is
// tuned, this is the file that says what the tuning did.

import { expect, test, beforeAll, afterAll } from 'bun:test'
import { chromium } from 'playwright'
import type { Browser, Page } from 'playwright'
import { auditPage, TOLERANCE } from './audit'
import type { Finding } from './audit'

let browser: Browser
let page: Page

beforeAll(async () => {
  browser = await chromium.launch({ headless: true })
  page = await (await browser.newContext({
    viewport: { width: 400, height: 600 },
    reducedMotion: 'reduce',
  })).newPage()
})

afterAll(async () => {
  await browser?.close()
})

/** Render a body fragment and run the real audit over it. */
async function audit(body: string): Promise<Finding[]> {
  await page.setContent(
    `<!doctype html><html><head><style>
       *{box-sizing:border-box} body{margin:0;font:14px system-ui}
     </style></head><body>${body}</body></html>`,
    { waitUntil: 'load' },
  )
  return await page.evaluate(auditPage, TOLERANCE) as Finding[]
}

const rules = (fs: Finding[]) => fs.filter(f => f.severity === 'error').map(f => f.rule)

// ── 1. An inline parent is measured, not skipped ───────────────────────────

test('a child escaping an INLINE parent is caught', async () => {
  // The parent is an inline span whose own bounding rect spans the column; only
  // its line boxes say where it really is. The code is a fixed-width block that
  // starts inside the span and ends well past it.
  const found = await audit(`
    <div style="width:200px;border:1px solid #333;padding:4px">
      <span id="p">tail <code style="display:inline-block;width:320px">x</code></span>
    </div>`)
  expect(rules(found)).toContain('escapes-container')
})

test('a child inside its inline parent is not', async () => {
  const found = await audit(`
    <div style="width:200px;border:1px solid #333;padding:4px">
      <span>ok <code style="display:inline-block;width:40px">x</code></span>
    </div>`)
  expect(rules(found)).not.toContain('escapes-container')
})

// ── 2. The walk reaches past bare layout divs ──────────────────────────────

test('a control escaping a CARD through three bare divs is caught', async () => {
  const found = await audit(`
    <div id="card" style="width:200px;border:1px solid #333;background:#111">
      <div><div><div>
        <button style="width:320px;height:30px">wide</button>
      </div></div></div>
    </div>`)
  const escape = found.find(f => f.rule === 'escapes-container')
  expect(escape).toBeDefined()
  // Named the ancestor it escaped, not the bare div it sits in.
  expect(escape!.detail).toContain('ancestor')
  expect(escape!.detail).toContain('#card')
})

test('the same control inside a SCROLLING ancestor is not', async () => {
  const found = await audit(`
    <div style="width:200px;border:1px solid #333;overflow-x:auto">
      <div><div>
        <button style="width:320px;height:30px">wide</button>
      </div></div>
    </div>`)
  expect(rules(found)).not.toContain('escapes-container')
  expect(rules(found)).not.toContain('escapes-clipper')
})

test('a control CUT OFF by a clipping ancestor is reported as clipped', async () => {
  const found = await audit(`
    <div style="width:200px;border:1px solid #333;overflow:hidden">
      <div><div>
        <button style="width:320px;height:30px">wide</button>
      </div></div>
    </div>`)
  const f = found.find(x => x.rule === 'escapes-clipper')
  expect(f).toBeDefined()
  expect(f!.detail).toContain('clipped')
})

// ── 3. A control whose own label does not fit it ───────────────────────────

test('a fixed-height button whose label wraps out of it is caught', async () => {
  const found = await audit(`
    <div style="width:200px">
      <button style="height:20px;width:120px;padding:0 8px">
        a label far too long for a twenty pixel tall button
      </button>
    </div>`)
  const f = found.find(x => x.rule === 'control-label-overflows')
  expect(f).toBeDefined()
  expect(f!.detail).toContain('in a 120x20 control')
})

test('a clipped fixed-height button says it is clipped', async () => {
  const found = await audit(`
    <div style="width:200px">
      <button style="height:20px;width:120px;overflow:hidden">
        a label far too long for a twenty pixel tall button
      </button>
    </div>`)
  const f = found.find(x => x.rule === 'control-label-overflows')
  expect(f).toBeDefined()
  expect(f!.detail).toContain('clipped by overflow:hidden')
})

test('a nowrap label wider than its fixed-width button is caught', async () => {
  // Rule 3 misses this: the nowrap sits on the inline child, which has no client
  // box of its own to measure against.
  const found = await audit(`
    <div style="width:300px">
      <button style="width:80px;height:28px"><span style="white-space:nowrap">a long single line</span></button>
    </div>`)
  const f = found.find(x => x.rule === 'control-label-overflows')
  expect(f).toBeDefined()
  expect(f!.detail).toContain('in x')
})

test('a comfortable button, and an icon-only one, stay quiet', async () => {
  const found = await audit(`
    <div style="width:300px">
      <button style="height:32px;padding:0 12px">Export</button>
      <button style="height:24px;width:24px"><svg width="12" height="12"></svg></button>
      <button style="padding:6px 10px">a label that wraps onto two lines in an auto-height button</button>
    </div>`)
  expect(rules(found)).not.toContain('control-label-overflows')
})

// ── 4. Out-of-flow boxes are checked, not exempt ───────────────────────────

test('an absolutely positioned panel pushed off the window is caught', async () => {
  const found = await audit(`
    <div style="position:relative;width:300px">
      <button style="position:absolute;left:360px;top:10px;width:200px;height:30px">off</button>
    </div>`)
  const f = found.find(x => x.rule === 'offscreen')
  expect(f).toBeDefined()
  expect(f!.detail).toContain('outside the 400px window')
})

test('a fixed bar wider than the window is caught', async () => {
  const found = await audit(`
    <div style="position:fixed;left:0;top:0;width:400px;height:40px">
      <button style="position:fixed;left:-120px;top:8px;width:100px;height:24px">gone</button>
    </div>`)
  expect(rules(found)).toContain('offscreen')
})

test('an absolute panel CLIPPED by its containing block is caught', async () => {
  const found = await audit(`
    <div style="position:relative;width:120px;height:60px;overflow:hidden;border:1px solid #333">
      <button style="position:absolute;left:0;top:8px;width:300px;height:24px">cut</button>
    </div>`)
  const f = found.find(x => x.rule === 'escapes-offset-parent')
  expect(f).toBeDefined()
  expect(f!.detail).toContain('cut off')
})

test('an anchored dropdown wider than its trigger, fully on screen, stays quiet', async () => {
  // The tuned case, and the reason `escapes-offset-parent` asks whether the
  // containing block CLIPS: being wider than the control you hang off is how a
  // popover works, and nothing here is hidden from anyone.
  const found = await audit(`
    <div style="padding:20px">
      <div style="position:relative;width:60px">
        <button style="width:60px;height:28px">open</button>
        <div role="menu" style="position:absolute;left:0;top:28px;width:280px;border:1px solid #333">
          <button style="width:100%;height:28px">an item</button>
        </div>
      </div>
    </div>`)
  expect(rules(found)).not.toContain('escapes-offset-parent')
  expect(rules(found)).not.toContain('offscreen')
})

// ── 5. Chrome over a control, and the dialog that is allowed to ────────────

test('a fixed bar sitting on a button is still caught', async () => {
  const found = await audit(`
    <button style="position:static;width:120px;height:40px;margin:20px">under</button>
    <div style="position:fixed;left:0;top:0;width:400px;height:120px;background:#222"></div>`)
  expect(rules(found)).toContain('overlay-covers-control')
})

test('an open dialog covering the page is not', async () => {
  const found = await audit(`
    <button style="width:120px;height:40px;margin:20px">under</button>
    <div role="dialog" style="position:fixed;left:0;top:0;width:400px;height:120px;background:#222"></div>`)
  expect(rules(found)).not.toContain('overlay-covers-control')
})

// ── The rules that were already there still work ───────────────────────────

test('page-level horizontal overflow and clipped nowrap text still fire', async () => {
  const found = await audit(`
    <div style="width:900px;height:20px"></div>
    <div style="width:80px;white-space:nowrap;overflow:hidden">a line far wider than eighty pixels</div>`)
  expect(rules(found)).toContain('page-h-overflow')
  expect(rules(found)).toContain('text-clipped')
})

test('deliberate ellipsis truncation is a note, not a failure', async () => {
  const found = await audit(`
    <div style="width:80px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis">a long line here</div>`)
  expect(rules(found)).not.toContain('text-clipped')
  expect(found.some(f => f.rule === 'text-truncated' && f.severity === 'info')).toBe(true)
})
