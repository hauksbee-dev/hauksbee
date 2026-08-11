import { expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import { renderToStaticMarkup } from 'react-dom/server'
import { chromium } from 'playwright'
import type { BoardSession } from '../src/hooks/useBoardSession'
import type { WebReport } from '../src/types/report'

function realFrontdoorReport(): WebReport {
  const startup = JSON.parse(readFileSync(
    new URL('../../demo/sessions/blinky/report-nominal.json', import.meta.url),
    'utf8',
  )) as { report: WebReport }
  const report = startup.report
  report.cosim = {
    ...report.cosim!,
    timing_coverage: [{
      mcu_ref: 'U1', backend: 'simavr:atmega328p', cycle_exact: true,
      timestamp_precision_s: 0.000001, minimum_guaranteed_pulse_s: 0.000002,
      chunk_s: 0.001,
    }],
    timing_refusals: ['PWL replay refused on net /CLK: transition budget exceeded'],
    fallback_windows: [{
      start_s: 0.001, end_s: 0.002, method: 'backward-euler',
      fidelity_note: 'first-order and numerically dissipative', error_estimate_v: 0.012,
    }],
  }
  return report
}

function session(report: WebReport): BoardSession {
  const noop = () => {}
  return {
    report,
    busy: null,
    uploadError: null,
    uploadNotice: null,
    dismissNotice: noop,
    firmwareFile: null,
    boardFile: null,
    boardLabel: report.file_name,
    boardUrl: null,
    analyzedAt: 0,
    launch: { phase: 'idle' },
    liveMode: 'none',
    serverLive: null,
    refreshLiveStatus: noop,
    confirmReplace: noop,
    cancelLaunch: noop,
    forceLaunch: noop,
    selectedNet: null,
    selectedComponent: null,
    setSelectedNet: noop,
    setSelectedComponent: noop,
    handleBoard: noop,
    handleFirmware: noop,
    clearFirmware: noop,
    runSample: noop,
    resetFlow: noop,
    restoreReport: noop,
    restoredFrom: null,
    runEpoch: 1,
    launchLive: noop,
    onEmptyBoard: noop,
  }
}

test('the browser renders every structured timing qualification from a frontdoor report', async () => {
  Object.assign(globalThis, { __APP_VERSION__: '0.1.0' })
  const { BoardView } = await import('../src/components/BoardView')
  const html = renderToStaticMarkup(<BoardView
    session={session(realFrontdoorReport())}
    onQueueCheck={() => {}}
    onDriveLive={() => {}}
    simMounted={false}
    engineVersion="0.1.0"
    spec={null}
    checks={null}
    sessionName={null}
  />)

  expect(html).toContain('Timing coverage')
  expect(html).toContain('pulses &gt;= 2.000 us guaranteed')
  expect(html).toContain('TIMING INVALID')
  expect(html).toContain('PWL replay refused on net /CLK')
  expect(html).toContain('Fallback-qualified windows')
  expect(html).toContain('0.012 V')

  const browser = await chromium.launch({ headless: true })
  try {
    const page = await browser.newPage()
    await page.setContent(html)
    expect(await page.getByText('TIMING INVALID').count()).toBe(1)
    expect(await page.getByText('Fallback-qualified windows').count()).toBe(1)
  } finally {
    await browser.close()
  }
})
