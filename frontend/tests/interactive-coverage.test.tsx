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
    restoreReport: noop,
    restoredFrom: null,
    runEpoch: 1,
    launchLive: noop,
    onEmptyBoard: noop,
  }
}

test('the browser renders every structured timing qualification from a frontdoor report', async () => {
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

test('model coverage is a clickable human workflow, not an agent-only report', async () => {
  const { BoardView } = await import('../src/components/BoardView')
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
    onQueueCheck={() => {}}
    onQueuePeripheral={() => {}}
    onQueueSupply={() => {}}
    onClose={() => {}}
  />)
  expect(html).toContain('data-testid="selection-add-probe"')
  expect(html).toContain('Watch this trace live')
  expect(html).toContain('data-testid="assert-rail_window"')
  expect(html).toContain('data-testid="selection-add-stimulus"')
  expect(html).toContain('Drive this trace in a co-sim scenario')
  expect(html).toContain('data-testid="selection-add-supply"')
  expect(html).toContain('Use this trace as a 3.3 V scenario supply')

  const liveHtml = renderToStaticMarkup(<SelectionCard
    net="+3V3"
    component={null}
    onQueuePeripheral={() => {}}
    onQueueSupply={() => {}}
    peripheralMode="live-and-scenario"
    onClose={() => {}}
  />)
  expect(liveHtml).toContain('Drive this trace now and save the interaction')
  expect(liveHtml).toContain('Attach a pushbutton now and save it')
  expect(liveHtml).toContain('Power this trace now at 3.3 V and save the supply')
})

test('component constraint offers fail closed against engine capabilities', async () => {
  const { assertionOffers } = await import('../src/components/SelectionCard')
  const component = { ref: 'U2', value: '74HC595', lib_id: '74xx:74HC595' }
  expect(assertionOffers(null, component, []).map(offer => offer.kind)).toEqual([])
  expect(assertionOffers(null, component, ['max_temp']).map(offer => offer.kind)).toEqual(['max_temp'])

  const resistor = { ref: 'R1', value: '10k', lib_id: 'Device:R', padNet: '+5V' }
  expect(assertionOffers(null, resistor, ['max_current', 'max_temp']).map(offer => offer.kind)).toEqual([
    'max_current', 'max_temp', 'voltage',
  ])
})

test('visual interaction builder round-trips a real stimulus and timeline', async () => {
  const { buildToml, tomlToBuilder } = await import('../src/components/ChecksView')
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
  const { buildToml, tomlToBuilder } = await import('../src/components/ChecksView')
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
  const html = renderToStaticMarkup(<SelectionCard
    net={null}
    component={{ ref: 'U7', value: 'BMA423', lib_id: 'Sensor:BMA423' }}
    modelCoverage={{
      reference: 'U7', value: 'BMA423', lib_id: 'Sensor:BMA423', footprint: 'LGA-12',
      stage: 'executable_partial', actionable_behavior_gap: true, model_id: 'bma423',
      model_kind: 'digital', layer: 'builtin', origin: 'digital',
      source: { tier: 'vendor', layer: 'builtin', origin: 'digital', validation: 'datasheet-checked' },
      implements: ['pin_roles'], missing: ['i2c_spi_register_map'], pins: [],
    }}
    onQueueSensor={() => {}}
    onClose={() => {}}
  />)
  expect(html).toContain('data-testid="selection-add-sensor"')
  expect(html).toContain('Open its register-map behavior builder')

  const modelOwned = renderToStaticMarkup(<SelectionCard
    net={null}
    component={{ ref: 'U7', value: 'BMA423', lib_id: 'Sensor:BMA423' }}
    modelCoverage={{
      reference: 'U7', value: 'BMA423', lib_id: 'Sensor:BMA423', footprint: 'LGA-12',
      stage: 'executable_partial', actionable_behavior_gap: true, model_id: 'bma423',
      model_kind: 'digital', layer: 'builtin', origin: 'digital',
      source: { tier: 'vendor', layer: 'builtin', origin: 'digital', validation: 'datasheet-checked' },
      implements: ['register_map_subset', 'chip_id_read'], missing: ['full_register_map'], pins: [],
    }}
    onQueueSensor={() => {}}
    onClose={() => {}}
  />)
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
  const guessed = renderToStaticMarkup(<InputSourcesPanel boardInfo={base} frame={null} send={() => {}} />)
  expect(guessed).toContain('No input sources detected')
  expect(guessed).toContain('arbitrary net names are never treated as sources')
  expect(guessed).not.toContain('type="range"')

  const declared = renderToStaticMarkup(<InputSourcesPanel
    boardInfo={{
      ...base,
      input_sources: [{ id: 'A0', kind: 'voltage', min: 0, max: 3.3, initial: 1.65, unit: 'V' }],
    }}
    frame={null}
    send={() => {}}
  />)
  expect(declared).toContain('type="range"')
  expect(declared).toContain('1.65 V')
  expect(declared).toContain('3.3 V')
})

test('a report-only restored session does not offer model saves it cannot re-analyze', async () => {
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
