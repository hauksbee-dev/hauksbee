import { afterAll, beforeAll, expect, test } from 'bun:test'
import { existsSync, readFileSync } from 'node:fs'
import { renderToStaticMarkup } from 'react-dom/server'
import { chromium, type Browser, type Page } from 'playwright'
import type { BoardSession } from '../src/hooks/useBoardSession'
import type { WebReport } from '../src/types/report'
import { reportVerdictHeadline, reportVerdictTone } from '../src/lib/report-view'
import { visibleImportMarkers } from '../src/lib/board-renderer'
import { timingEvidence, timingRefusal } from './timing-fixture'

// The real frontdoor report lives in the repository's demo/ tree; a checkout
// without it skips the tests that need it rather than failing on a missing file.
const FRONTDOOR_REPORT = new URL('../../demo/sessions/blinky/report-nominal.json', import.meta.url)
const hasFrontdoorReport = existsSync(FRONTDOOR_REPORT)

// Cold browser and first-page setup do not belong in the 5s DOM assertion budget.
let browser: Browser | undefined
let page: Page
beforeAll(async () => {
  if (!hasFrontdoorReport) return
  browser = await chromium.launch({ headless: true })
  page = await browser.newPage()
}, 30_000)
afterAll(async () => { await browser?.close() })

function realFrontdoorReport(): WebReport {
  const startup = JSON.parse(readFileSync(FRONTDOOR_REPORT, 'utf8')) as { report: WebReport }
  const report = startup.report
  report.cosim = {
    ...report.cosim!,
    ...timingEvidence,
  }
  report.refusal = timingRefusal
  return report
}

function session(report: WebReport): BoardSession {
  const noop = () => {}
  return {
    report,
    busy: null,
    loadProgress: null,
    uploadError: null,
    uploadNotice: null,
    dismissNotice: noop,
    firmwareFile: null,
    schematicFile: null,
    supplementalFiles: { bom: null, placement: null, variant: null, asbuilt: null, models: [] },
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
    highlightNets: null,
    setHighlightNets: noop,
    handleBoard: noop,
    handleFirmware: noop,
    handleSchematic: noop,
    handleBom: noop,
    handlePlacement: noop,
    handleVariant: noop,
    handleAsbuilt: noop,
    handleModels: noop,
    clearFirmware: noop,
    clearSchematic: noop,
    reanalyzeCurrent: noop,
    runSample: noop,
    resetFlow: noop,
    replaceCosim: noop,
    restoreReport: noop,
    restoredFrom: null,
    runEpoch: 1,
    launchLive: noop,
    onEmptyBoard: noop,
  }
}

/** The BoardView as the app renders it. Every case below differs only in the
 *  session it is given (and, where a session was restored, its name), so the
 *  full prop list lives here once rather than in each test. */
async function boardHtml(session: BoardSession, sessionName: string | null = null): Promise<string> {
  const { BoardView } = await import('../src/components/BoardView')
  return renderToStaticMarkup(<BoardView
    session={session}
    onQueue={() => {}}
    onOpenChecks={() => {}}
    onDriveLive={() => {}}
    simMounted={false}
    engineVersion="0.1.0"
    spec={null}
    checks={null}
    sessionName={sessionName}
  />)
}

test.skipIf(!hasFrontdoorReport)('the browser renders every structured timing qualification from a frontdoor report', async () => {
  const html = await boardHtml(session(realFrontdoorReport()))

  expect(html).toContain('Timing coverage')
  expect(html).toContain('pulses &gt;= 2.000 us guaranteed')
  expect(html).toContain('Analysis could not make this claim')
  expect(html).toContain('PWL replay refused on net /CLK')
  expect(html).toContain('Fallback-qualified windows')
  expect(html).toContain('0.012 V')
  // The firmware ran, so the Next card offers its observations as checks; it no longer asks for firmware.
  expect(html).toContain('Turn what it did into checks')
  expect(html).not.toMatch(/add firmware/i)
  expect(html).toContain('Set up checks')

  const errors: string[] = []
  page.on('pageerror', error => errors.push(error.message))
  page.on('console', message => { if (message.type() === 'error') errors.push(message.text()) })
  await page.setContent(html)
  expect(await page.getByText('TIMING INVALID').count()).toBe(0)
  expect(await page.getByText(/PWL replay refused on net \/CLK/).count()).toBe(1)
  expect(await page.getByText('Fallback-qualified windows').count()).toBe(1)
  expect(errors).toEqual([])
})

test.skipIf(!hasFrontdoorReport)('typed co-sim invalidity and faults cannot retain a green verdict card', () => {
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

test.skipIf(!hasFrontdoorReport)('model coverage is a clickable human workflow, not an agent-only report', async () => {
  const report = realFrontdoorReport()
  report.model_coverage = {
    schema_version: 1,
    summary: {
      active_connected: 1, identified: 1, executable_available: 1,
      unresolved: 0, identity_only: 0, executable_scope_unspecified: 0,
      executable_partial: 1, executable_declared: 0, ignored: 0,
      actionable_behavior_gaps: 1, authoring_targets: 1,
      requirements_total: 0, requirements_met: 0, requirements_unmet: 0,
    },
    components: [{
      reference: 'U4', value: 'W25Q128JVS', lib_id: 'Memory_Flash:W25Q128JVS',
      footprint: 'Package_SO:SOIC-8', stage: 'executable_partial',
      actionable_behavior_gap: true, model_id: 'w25q128jvs', model_kind: 'spi_nor',
      layer: 'builtin', origin: 'digital',
      source: { tier: 'vendor', layer: 'builtin', origin: 'digital', validation: 'datasheet-checked' },
      references: [{ title: 'W25Q128JV datasheet', url: 'https://example.test/w25q.pdf', locator: 'sections 7 and 8', sha256: 'a'.repeat(64) }],
      implements: ['jedec_id', 'read', 'deep_power_down_current_max'],
      missing: ['program_erase_timing'],
      pins: [{ number: '8', function: 'VCC', kind: 'power_in', net: '+3V3', position_mm: [12, 8] }],
    }],
    authoring_targets: [],
  }
  const html = await boardHtml(session(report))

  expect(html).toContain('1/1 identified')
  expect(html).toContain('1/1 executable')
  expect(html).toContain('data-testid="model-author-U4"')
  expect(html).toContain('This list is deterministic and does not require an LLM')
})

test('a live trace click offers a scope probe and repeatable checks together', async () => {
  const { SelectionCard } = await import('../src/components/SelectionCard')
  const html = renderToStaticMarkup(<SelectionCard
    net="+3V3"
    component={null}
    onAddProbe={() => {}}
    onQueue={() => {}}
    onClose={() => {}}
  />)
  expect(html).toContain('data-testid="selection-add-probe"')
  expect(html).toContain('Show +3V3 on the scope')
  expect(html).toContain('NET · LIVE')
  // Checks come from the assertion catalogue (see check-catalog.test.ts), which
  // the static render has not loaded; the card itself never opens a dialog.
  expect(html).not.toContain('constraint-modal')
  expect(html).not.toContain('aria-modal')
  // Driving the net sits behind one disclosure, not three loose buttons.
  expect(html).toContain('data-testid="selection-drive"')
  expect(html).toContain('data-testid="selection-add-stimulus"')
  expect(html).toContain('Apply a signal now and save it as a test')
  expect(html).toContain('data-testid="selection-add-supply"')
  expect(html).toContain('Power this net at 3.3 V now and save the supply')

  const staticHtml = renderToStaticMarkup(<SelectionCard
    net="+3V3"
    component={null}
    onQueue={() => {}}
    onClose={() => {}}
  />)
  expect(staticHtml).not.toContain('NET · LIVE')
  expect(staticHtml).not.toContain('selection-add-probe')
  expect(staticHtml).toContain('Apply a signal to this net in a test')
  expect(staticHtml).toContain('Attach a pushbutton to this net')
  expect(staticHtml).toContain('Power this net at 3.3 V in a test')

  const liveHtml = renderToStaticMarkup(<SelectionCard
    net="+3V3"
    component={null}
    onQueue={() => {}}
    peripheralMode="live-and-scenario"
    onClose={() => {}}
  />)
  expect(liveHtml).toContain('Attach a pushbutton now and save it')
})

test('a part card says its model state in plain words, never a raw id', async () => {
  const { SelectionCard } = await import('../src/components/SelectionCard')
  const html = renderToStaticMarkup(<SelectionCard
    net={null}
    component={{ ref: 'U3', value: 'AMS1117-3.3', lib_id: 'Package_TO_SOT_SMD:SOT-223-3_TabPin2' }}
    boundKind="vreg"
    modelCoverage={{
      reference: 'U3', value: 'AMS1117-3.3', lib_id: 'Package_TO_SOT_SMD:SOT-223-3_TabPin2', footprint: 'SOT-223',
      stage: 'executable_scope_unspecified', actionable_behavior_gap: true, model_id: 'ams1117_3v3',
      model_kind: 'vreg', layer: 'builtin', origin: 'vreg',
      source: { tier: 'datasheet-derived', layer: 'builtin', origin: 'vreg', validation: 'datasheet-curves' },
      implements: [], missing: ['board_crystal_frequency_binding', 'usb_device_controller', 'silicon_timing_and_errata', 'silicon_power_current_profile'], pins: [],
    }}
    onQueue={() => {}}
    onAuthorModel={() => {}}
    onClose={() => {}}
  />)
  const [visible] = html.split('data-testid="selection-technical"')
  expect(visible).toContain('Voltage regulator')
  expect(visible).toContain('Coverage not listed')
  expect(visible).toContain('4 gaps')
  expect(visible).toContain('Crystal frequency from the board')
  expect(visible).toContain('Checked against datasheet curves')
  expect(visible).toContain('Extend U3&#x27;s model')
  for (const raw of ['vreg', 'datasheet-curves', 'ams1117_3v3', 'board_crystal_frequency_binding', 'SOT-223-3_TabPin2', 'executable_scope_unspecified']) {
    expect(visible).not.toContain(raw)
  }
  // The raw ids live under Technical details, for the developer who wants them.
  expect(html).toContain('ams1117_3v3')
  // One primary action on the card.
  expect(html.split('hb-btn-primary').length - 1).toBe(1)
})

test('visual interaction builder round-trips a real stimulus and timeline', async () => {
  const { buildToml } = await import('../src/lib/check-spec')
  const { tomlToBuilder } = await import('../src/lib/spec-model')
  const peripheral = {
    rowId: 1, id: 'STIM_IN', kind: 'stimulus' as const, net: '/SENSE', to: 'GND',
    waveform: 'sine' as const, offset: '1.65', amplitude: '0.25', freq_hz: '1000',
    bounce_ms: '', initial: '0', events: [{ t_ms: '25', value: '0.75' }],
  }
  const toml = buildToml('interactive board', '100', [], [peripheral], [])
  expect(toml).toContain('[[peripheral]]')
  expect(toml).toContain('type = "stimulus"')
  expect(toml).toContain('net = "/SENSE"')
  expect(toml).toContain('waveform = "sine"')
  expect(toml).toContain('[[peripheral.event]]')
  expect(tomlToBuilder(toml)?.peripherals).toEqual([peripheral])
})

test('visual bus-device builder embeds exact local spec bytes and round-trips inputs', async () => {
  const { buildToml } = await import('../src/lib/check-spec')
  const { tomlToBuilder } = await import('../src/lib/spec-model')
  const sensor = {
    rowId: 1,
    id: 'U7_ACCEL',
    componentRef: 'U7',
    modelId: 'bma423',
    specName: 'bma423.toml',
    spec: '[sensor]\nname = "BMA423"\nbus = "i2c"\ni2c_address = 0x18\n[[sensor.register]]\naddr = 0x00\nconst = [0x13]\n[sensor.protocol]\nstyle = "i2c_pointer"\n',
    controller: '',
    csNet: '',
    inputs: [{ rowId: 1, name: 'accel_x_g', value: '0.25' }],
  }
  const toml = buildToml('bus device', '100', [], [], [], [sensor])
  expect(toml).toContain('[[sensor]]')
  expect(toml).toContain('id = "U7_ACCEL"')
  expect(toml).toContain('[sensor.inputs]')
  expect(toml).toContain('"accel_x_g" = 0.25')
  const parsed = tomlToBuilder(toml)?.sensors[0]
  expect(parsed?.id).toBe('U7_ACCEL')
  expect(parsed?.spec).toBe(sensor.spec)
  expect(parsed?.inputs).toEqual(sensor.inputs)
})

test('a component with a bus-model gap offers explicit register-map attachment', async () => {
  const { SelectionCard } = await import('../src/components/SelectionCard')
  /** The same BMA423 card, differing only in what its model implements. */
  const card = (owns: string[], missing: string[]) => renderToStaticMarkup(<SelectionCard
    net={null}
    component={{ ref: 'U7', value: 'BMA423', lib_id: 'Sensor:BMA423' }}
    modelCoverage={{
      reference: 'U7', value: 'BMA423', lib_id: 'Sensor:BMA423', footprint: 'LGA-12',
      stage: 'executable_partial', actionable_behavior_gap: true, model_id: 'bma423',
      model_kind: 'digital', layer: 'builtin', origin: 'digital',
      source: { tier: 'vendor', layer: 'builtin', origin: 'digital', validation: 'datasheet-checked' },
      implements: owns, missing, pins: [],
    }}
    onQueue={() => {}}
    onClose={() => {}}
  />)
  const html = card(['pin_roles'], ['i2c_spi_register_map'])
  expect(html).toContain('data-testid="selection-add-sensor"')
  expect(html).toContain('Simulate its registers with a behavior builder')

  const modelOwned = card(['register_map_subset', 'chip_id_read'], ['full_register_map'])
  expect(modelOwned).toContain('data-testid="selection-register-map-owned"')
  expect(modelOwned).toContain('already auto-attaches model-owned register behavior')
  expect(modelOwned).not.toContain('selection-add-sensor')
})

test('live input sliders require an explicit engine source, not an input-looking net name', async () => {
  const { InputSourcesPanel } = await import('../src/components/InputSourcesPanel')
  const base = {
    type: 'BoardInfo' as const, name: 'b', board_url: '/b', num_components: 0,
    num_nets: 1, nets: ['A0'], component_kinds: {}, mcus: [] as [string, string][],
  }
  const guessed = renderToStaticMarkup(<InputSourcesPanel boardInfo={base} send={() => {}} />)
  expect(guessed).toContain('No buttons or signals are attached yet')
  // Without an attach handler (the demo) there is no button to offer; with one,
  // the attach is a button, not an instruction.
  expect(guessed).not.toContain('Attach input')
  const attachable = renderToStaticMarkup(<InputSourcesPanel boardInfo={base} send={() => {}} onAttach={() => {}} />)
  expect(attachable).toContain('data-testid="attach-input-net"')
  expect(guessed).not.toContain('type="range"')

  const declared = renderToStaticMarkup(<InputSourcesPanel
    boardInfo={{
      ...base,
      input_sources: [{ id: 'A0', kind: 'voltage', min: 0, max: 3.3, initial: 1.65, unit: 'V' }],
    }}
    send={() => {}}
  />)
  expect(declared).toContain('type="range"')
  expect(declared).toContain('1.65 V')
  expect(declared).toContain('3.3 V')
})

test.skipIf(!hasFrontdoorReport)('a report-only restored session does not offer model saves it cannot re-analyze', async () => {
  const restored = session(realFrontdoorReport())
  restored.restoredFrom = {
    sessionName: 'saved bench',
    boardName: 'blinky.kicad_pcb',
    firmwareName: null,
    schematicName: null,
  }
  const html = await boardHtml(restored, 'saved bench')
  expect(html).toContain('data-testid="restored-notice"')
  expect(html).not.toContain('data-testid="datasheet-extract"')
  expect(html).not.toContain('data-testid="write-part-open"')
})

test.skipIf(!hasFrontdoorReport)('import diagnostics expose recovered, partial, unplaced and split-net guidance without inventing coordinates', async () => {
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
  const html = await boardHtml(session(report))

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
  const failed: WebReport = {
    ok: false,
    error: 'Netlist parse error at line 2',
    board_name: '',
    file_name: 'broken.net',
    num_components: 0,
    num_nets: 0,
    headline: 'Could not read the file.',
    serious: 0,
    total: 0,
    sections: [],
    components: [],
    import_failure: {
      stage: 'KiCad netlist parser',
      excerpt: 'line 2: this is not a valid netlist',
      suggested_fix: 'Edit the exact line shown below, then rerun.',
    },
  }
  const html = await boardHtml(session(failed))
  expect(html).toContain('data-testid="import-failure"')
  expect(html).toContain('Import stopped at KiCad netlist parser')
  expect(html).toContain('line 2: this is not a valid netlist')
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

test.skipIf(!hasFrontdoorReport)('a permissive build does not advertise the unavailable AVR sample', async () => {
  const { UploadView } = await import('../src/components/UploadView')
  const html = renderToStaticMarkup(<UploadView
    session={session(realFrontdoorReport())}
    avrAvailable={false}
  />)
  expect(html).toContain('Watchy')
  expect(html).toContain('Blinky')
  expect(html).not.toContain('Boot gate + firmware')
})
