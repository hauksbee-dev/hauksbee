#!/usr/bin/env bun
// End-to-end verification against the real hauksbee-server.
// Usage: bun run verify.ts

import { chromium } from 'playwright'

const BASE = 'http://127.0.0.1:3001'
const SHOTS = './screenshots'

async function main() {
  console.log('Launching browser...')
  const browser = await chromium.launch({ headless: true })
  const ctx = await browser.newContext({ viewport: { width: 1400, height: 900 } })
  const page = await ctx.newPage()

  // Capture console errors
  page.on('console', msg => {
    if (msg.type() === 'error') console.error('[page error]', msg.text())
  })

  // ── Screenshot 1: Initial board + connected ──
  console.log('Loading page...')
  await page.goto(BASE, { waitUntil: 'networkidle' })
  await page.waitForTimeout(2000)

  // Wait for "connected" status
  const connEl = await page.waitForSelector('text=connected', { timeout: 10000 })
  console.log('Connected:', !!connEl)

  await page.screenshot({ path: `${SHOTS}/live-1.png`, fullPage: false })
  console.log('Screenshot 1 saved: live-1.png (initial board + connected)')

  // ── Screenshot 2: After Play, type 'i', see ident response ──
  // Click on Serial tab
  await page.click('text=Serial')
  await page.waitForTimeout(300)

  // Press Space to play
  await page.keyboard.press('Space')
  await page.waitForTimeout(500)

  // Type 'i' in serial input
  const serialInput = await page.$('input[placeholder="type here, Enter to send..."]')
  if (serialInput) {
    await serialInput.click()
    await serialInput.type('i')
    await page.keyboard.press('Enter')
    await page.waitForTimeout(1500)  // wait for MCU response
  } else {
    console.warn('Serial input not found')
  }

  await page.screenshot({ path: `${SHOTS}/live-2.png`, fullPage: false })
  console.log('Screenshot 2 saved: live-2.png (after Play + serial i)')

  // Check that ident response appeared
  const pageText = await page.textContent('body')
  const hasIdent = pageText?.includes('hauksbee-demo') ?? false
  console.log(`Ident response visible: ${hasIdent}`)

  // ── Screenshot 3: Probe scope showing D13_LED square wave ──
  // Click on Scope tab
  await page.click('text=Scope')
  await page.waitForTimeout(300)

  // Click D13_LED to add probe
  const d13Button = await page.$('button:has-text("D13_LED")')
  if (d13Button) {
    await d13Button.click()
    console.log('Added D13_LED probe')
  } else {
    console.warn('D13_LED probe button not found')
  }

  // Wait for data to accumulate (LED blinks at 5Hz)
  await page.waitForTimeout(3000)

  await page.screenshot({ path: `${SHOTS}/live-3.png`, fullPage: false })
  console.log('Screenshot 3 saved: live-3.png (probe scope with D13_LED square wave)')

  await browser.close()

  console.log('\n=== Verification complete ===')
  console.log('Screenshots saved to ./screenshots/')
  console.log(`Ident response: ${hasIdent ? 'PASS' : 'FAIL (server may not have responded in time)'}`)
}

main().catch(err => {
  console.error('Verification failed:', err)
  process.exit(1)
})
