import { expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import { renderToStaticMarkup } from 'react-dom/server'
import { chromium } from 'playwright'
import type { BoardSession } from '../src/hooks/useBoardSession'
import type { WebReport } from '../src/types/report'
import { reportVerdictHeadline, reportVerdictTone } from '../src/lib/report-verdict'
import { visibleImportMarkers } from '../src/lib/board-renderer'

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
  report.refusal = {
    claim: 'timing-sensitive firmware and electrical conclusions',
    missing_prerequisite: 'PWL replay refused on net /CLK: transition budget exceeded',
    valid_partial_conclusions: ['Static board analysis remains valid.'],
    next_action: 'reduce transitions per solver chunk, then rerun',
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
    schematicFile: null,
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
    handleSchematic: noop,
    clearFirmware: noop,
    clearSchematic: noop,
    reanalyzeCurrent: noop,
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
    onOpenChecks={() => {}}
    onDriveLive={() => {}}
    simMounted={false}
    engineVersion="0.1.0"
    spec={null}
    checks={null}
    sessionName={null}
  />)

  expect(html).toContain('Timing coverage')
  expect(html).toContain('pulses &gt;= 2.000 us guaranteed')
  expect(html).toContain('Analysis could not make this claim')
  expect(html).toContain('PWL replay refused on net /CLK')
  expect(html).toContain('Fallback-qualified windows')
  expect(html).toContain('0.012 V')
  expect(html).toContain('does not prove powered behavior')
  expect(html).toContain('Set up checks')

  const browser = await chromium.launch({ headless: true })
  try {
    const page = await browser.newPage()
    await page.setContent(html)
    expect(await page.getByText('TIMING INVALID').count()).toBe(0)
    expect(await page.getByText(/PWL replay refused on net \/CLK/).count()).toBe(1)
    expect(await page.getByText('Fallback-qualified windows').count()).toBe(1)
  } finally {
    await browser.close()
  }
})

test('typed co-sim invalidity and faults cannot retain a green verdict card', () => {
  const refused = realFrontdoorReport()
  refused.serious = 0
  refused.total = 0
  refused.sections = []
  refused.evidence = []
  expect(reportVerdictTone(refused)).toBe('warning')
  expect(reportVerdictHeadline(refused)).toContain('Analysis invalid')
  expect(reportVerdictHeadline(refused)).not.toContain('Looks healthy')

  const faulted: WebReport = {
    ...refused,
    refusal: null,
    cosim: {
      ...refused.cosim!,
      timing_refusals: [],
      fallback_windows: [],
      findings: [{ level: 'serious', what: 'driver contention', why: 'two outputs fight', fix: 'remove the conflict' }],
    },
  }
  expect(reportVerdictTone(faulted)).toBe('error')
})

test('a report-only restored session does not offer model saves it cannot re-analyze', async () => {
  Object.assign(globalThis, { __APP_VERSION__: '0.1.0' })
  const { BoardView } = await import('../src/components/BoardView')
  const restored = session(realFrontdoorReport())
  restored.restoredFrom = {
    sessionName: 'saved bench',
    boardName: 'blinky.kicad_pcb',
    firmwareName: null,
    schematicName: null,
  }
  const html = renderToStaticMarkup(<BoardView
    session={restored}
    onQueueCheck={() => {}}
    onOpenChecks={() => {}}
    onDriveLive={() => {}}
    simMounted={false}
    engineVersion="0.1.0"
    spec={null}
    checks={null}
    sessionName="saved bench"
  />)
  expect(html).toContain('data-testid="restored-notice"')
  expect(html).not.toContain('data-testid="datasheet-extract"')
  expect(html).not.toContain('data-testid="write-part-open"')
})

test('import diagnostics expose recovered, partial, unplaced and split-net guidance without inventing coordinates', async () => {
  Object.assign(globalThis, { __APP_VERSION__: '0.1.0' })
  const { BoardView } = await import('../src/components/BoardView')
  const report = realFrontdoorReport()
  report.import_diagnostics = {
    format: 'Gerber / Excellon reconstruction',
    recovered: 1,
    partial: 1,
    unplaced: 1,
    missing_or_refused: 1,
    objects: [
      { id: 'U1', status: 'recovered', confidence: 'high', x: 12, y: 9, explanation: 'location recovered', nets: ['VCC'] },
      { id: 'U2', status: 'partial', confidence: 'low', explanation: 'the source supplied no board coordinate', nets: ['NET_1'] },
    ],
    issues: [{
      kind: 'split_net',
      title: 'Possible split-net boundary',
      explanation: 'NET_1 may be split because the drill declaration was absent.',
      suggested_fix: 'Supply IPC-D-356 connectivity.',
      net: 'NET_1',
    }],
  }
  const html = renderToStaticMarkup(<BoardView
    session={session(report)}
    onQueueCheck={() => {}}
    onOpenChecks={() => {}}
    onDriveLive={() => {}}
    simMounted={false}
    engineVersion="0.1.0"
    spec={null}
    checks={null}
    sessionName={null}
  />)

  expect(html).toContain('data-testid="import-diagnostics"')
  expect(html).toContain('1 recovered · 1 partial · 1 unplaced · 1 missing/refused limit')
  expect(html).toContain('Possible split-net boundary')
  expect(html).toContain('Inspect NET_1')
  expect(html).toContain('not placeable')
  expect(html).toContain('They are not drawn at guessed coordinates')
  expect(html).toContain('Show recovered / partial on board')

  const markers = [
    { x: 12, y: 9, status: 'recovered' as const, nets: ['VCC'] },
    { x: 18, y: 9, status: 'partial' as const, nets: ['NET_1'] },
  ]
  expect(visibleImportMarkers(markers, new Set())).toHaveLength(2)
  expect(visibleImportMarkers(markers, new Set(['NET_1']))).toEqual([markers[1]])
})

test('a parser refusal renders only its localized excerpt and suggested fix', async () => {
  Object.assign(globalThis, { __APP_VERSION__: '0.1.0' })
  const { BoardView } = await import('../src/components/BoardView')
  const failed: WebReport = {
    ok: false,
    error: 'Board-as-Code parse error at line 2',
    board_name: '',
    file_name: 'broken.board',
    num_components: 0,
    num_nets: 0,
    headline: 'Could not read the file.',
    serious: 0,
    total: 0,
    sections: [],
    components: [],
    import_failure: {
      stage: 'Board-as-Code compiler',
      excerpt: 'line 2: this is not valid board code',
      suggested_fix: 'Edit the exact line shown below, then rerun.',
    },
  }
  const html = renderToStaticMarkup(<BoardView
    session={session(failed)}
    onQueueCheck={() => {}}
    onOpenChecks={() => {}}
    onDriveLive={() => {}}
    simMounted={false}
    engineVersion="0.1.0"
    spec={null}
    checks={null}
    sessionName={null}
  />)
  expect(html).toContain('data-testid="import-failure"')
  expect(html).toContain('Import stopped at Board-as-Code compiler')
  expect(html).toContain('line 2: this is not valid board code')
  expect(html).toContain('Suggested fix:')
})

test('collapsed navigation keeps an accessible name for every icon button', async () => {
  const { Sidebar } = await import('../src/components/Sidebar')
  const html = renderToStaticMarkup(<Sidebar
    nav={{
      view: 'board',
      setView: () => {},
      checksEnabled: true,
      simEnabled: true,
      simRunning: false,
      faultCount: 0,
    }}
    report={null}
    boardLabel={null}
    analyzedAt={null}
    theme="dark"
    onToggleTheme={() => {}}
  />)
  for (const label of ['Board', 'Checks', 'Live Sim', 'Environment']) {
    expect(html).toContain(`aria-label="${label}"`)
  }
})

test('a permissive build does not advertise the unavailable AVR sample', async () => {
  const { UploadView } = await import('../src/components/UploadView')
  const html = renderToStaticMarkup(<UploadView
    session={session(realFrontdoorReport())}
    avrAvailable={false}
  />)
  expect(html).toContain('Watchy')
  expect(html).toContain('Blinky')
  expect(html).not.toContain('Boot gate + firmware')
})
