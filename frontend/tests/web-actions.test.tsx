// The engine's typed actions: one handler maps each id to what the app does,
// an id the host cannot run is left out (never a dead button), and the same step
// is one button however many sections ask for it.

import { describe, expect, test } from 'bun:test'
import { renderToStaticMarkup } from 'react-dom/server'
import { NextStep, nextStepActions } from '../src/components/report/NextStep'
import { FindingsList } from '../src/components/report/Findings'
import { fixFor } from '../src/lib/check-catalog'
import type { CatalogKind } from '../src/lib/check-catalog'
import { findingEntries, reportView, sectionName, sectionStatus } from '../src/lib/report-view'
import { runnableActions, withoutShown, type ActionHost } from '../src/lib/web-actions'
import type { ActionId, WebAction, WebReport } from '../src/types/report'

const act = (id: ActionId, label: string, args?: WebAction['args']): WebAction => ({ id, label, ...(args ? { args } : {}) })

/** Every id the engine can send. */
const EVERY_ID: ActionId[] = [
  'model_parts', 'extract_unmodelled', 'add_bom', 'choose_models_folder', 'add_scenario', 'add_supply', 'add_ac_sweep',
  'add_firmware', 'open_check', 'show_on_board', 'start_checks', 'add_project_rules', 'set_part_number', 'mark_not_fitted',
  'install_backend',
]

function recorder() {
  const calls: string[] = []
  const host: ActionHost = {
    giveModel: (ref, tab) => calls.push(`model:${ref}:${tab ?? 'default'}`),
    openEvidence: () => calls.push('evidence'),
    openChecks: () => calls.push('checks'),
    queue: r => calls.push(`queue:${JSON.stringify(r)}`),
    show: t => calls.push(`show:${JSON.stringify(t)}`),
    openEnvironment: () => calls.push('env'),
  }
  return { calls, host }
}

describe('one handler for every action id', () => {
  test('each id the app can run does what the app already does', () => {
    const { calls, host } = recorder()
    const run = (a: WebAction) => runnableActions([a], host)[0]?.run()
    run(act('model_parts', 'Give U1 a model', { refs: ['U1', 'U2'] }))
    run(act('extract_unmodelled', 'Draft', { refs: ['U1'] }))
    run(act('add_bom', 'Add a bill of materials'))
    run(act('add_firmware', 'Add firmware'))
    run(act('choose_models_folder', 'Choose a models folder'))
    run(act('add_supply', 'Add a supply on +5V', { net: '+5V', volts: 5 }))
    run(act('open_check', 'Check it', { check: 'voltage', net: 'VCC', refs: ['U3'] }))
    run(act('show_on_board', 'Show on board', { refs: ['U1'] }))
    run(act('show_on_board', 'Show on board', { x: 1.5, y: 2 }))
    run(act('start_checks', 'Set up checks'))
    run(act('install_backend', 'Install ngspice', { backend: 'ngspice' }))
    expect(calls).toEqual([
      'model:U1:default',
      'model:U1:draft',
      'evidence', 'evidence', 'evidence',
      'queue:{"type":"supply","net":"+5V","volts":5}', 'checks',
      'queue:{"type":"check","kind":"voltage","net":"VCC","ref":"U3"}', 'checks',
      'show:{"refs":["U1"],"label":"U1"}',
      'show:{"refs":[],"at":{"x":1.5,"y":2},"label":"Show on board"}',
      'checks',
      'env',
    ])
  })

  test('ids with no app equivalent, and ids the host lacks the means for, show nothing', () => {
    const { host } = recorder()
    for (const id of ['add_project_rules', 'set_part_number', 'mark_not_fitted', 'add_scenario', 'add_ac_sweep'] as ActionId[]) {
      expect(runnableActions([act(id, 'x', { refs: ['C1'], file: '.kicad_pro' })], host)).toEqual([])
    }
    // A supply with no net, a check with no kind, a model with no part: not enough to start the flow.
    expect(runnableActions([act('add_supply', 'x'), act('open_check', 'x'), act('model_parts', 'x')], host)).toEqual([])
    // A restored session has no files: no model flow, no evidence slots.
    expect(runnableActions(EVERY_ID.map(id => act(id, id, { refs: ['U1'], net: 'N', check: 'voltage' })), {})).toEqual([])
    expect(runnableActions([act('model_parts', 'x', { refs: ['U1'] })], { openChecks: () => {} })).toEqual([])
  })

  test('the same step asked for twice is one button, and a second surface skips what the first shows', () => {
    const { host } = recorder()
    const bom = act('add_bom', 'Add a bill of materials')
    const shown = runnableActions([bom, { ...bom }], host)
    expect(shown).toHaveLength(1)
    expect(withoutShown([bom, act('add_firmware', 'Add firmware')], shown).map(a => a.id)).toEqual(['add_firmware'])
  })

  test('one part keeps the model button id the verdict card has always had', () => {
    const { host } = recorder()
    const [b] = runnableActions([act('model_parts', 'Give Q1 a model', { refs: ['Q1'] })], host)
    expect(b.testId).toBe('result-give-model-Q1')
  })
})

describe('the Next step offers the engine\'s steps', () => {
  const base = {
    verdict: 'invalid', serious: 0, partly_judged: true,
    judgement_scope: { open_parts: ['U1'], nets: 4, wiring_dependent: [], electrical_dependent: [] },
    next_actions: [
      act('model_parts', 'Give U1 a model', { refs: ['U1'] }),
      act('extract_unmodelled', 'Draft a model for U1 from its datasheet', { refs: ['U1'] }),
      act('set_part_number', 'Set the part number of C7', { refs: ['C7'] }),
      act('add_project_rules', 'Add the .kicad_pro file', { file: '.kicad_pro' }),
    ],
  } as unknown as WebReport
  const { host } = recorder()
  const html = renderToStaticMarkup(
    <NextStep report={base} view={reportView(base)} firmwareName={null} checks={null} specFile={null}
      onQueue={() => {}} onOpenChecks={() => {}} host={host} />,
  )

  test('the model step is the primary button and the draft step sits beside it', () => {
    expect(html).toContain('data-next="model"')
    expect(html).toContain('Give U1 a model')
    expect(html).toContain('Draft a model for U1 from its datasheet')
    expect(html.match(/hb-btn-primary/g)).toHaveLength(1)
  })

  test('steps the app cannot take are not buttons', () => {
    expect(html).not.toContain('part number')
    expect(html).not.toContain('.kicad_pro')
    expect(nextStepActions(base, 'model', host).map(a => a.id)).toEqual(['model_parts', 'extract_unmodelled'])
  })

  test('a routed-board or CI card keeps its one step, whatever the engine also sent', () => {
    expect(nextStepActions(base, 'route', host)).toEqual([])
    expect(nextStepActions(base, 'ci', host)).toEqual([])
  })

  test('the checks card does not repeat the buttons it already has', () => {
    const r = { ...base, next_actions: [act('start_checks', 'Set up checks'), act('add_firmware', 'Add firmware'), act('add_bom', 'Add a bill of materials')] } as WebReport
    expect(nextStepActions(r, 'checks', host).map(a => a.id)).toEqual(['add_bom'])
  })
})

describe('a section\'s buttons', () => {
  test('sit on the first card of that section, once', () => {
    const r = {
      verdict: 'fail', serious: 1, num_components: 1, num_nets: 1,
      sections: [{
        title: 'Signal integrity (checked)', verdict: 'x',
        findings: [
          { level: 'serious', what: 'First thing', why: 'a', fix: 'b' },
          { level: 'warning', what: 'Second thing', why: 'c', fix: 'd' },
        ],
        actions: [act('model_parts', 'Give U1 a model', { refs: ['U1'] })],
      }],
    } as unknown as WebReport
    const v = reportView(r)
    const { host } = recorder()
    const sectionActions = { [sectionName(v.sections[0].title)]: runnableActions(v.sections[0].actions, host) }
    const html = renderToStaticMarkup(
      <FindingsList entries={findingEntries(v)} status={sectionStatus(v)} openParts={null} openRefs={new Set()}
        preLayout={null} sectionActions={sectionActions} />,
    )
    expect(html.match(/Give U1 a model/g)).toHaveLength(1)
  })
})

describe('a check\'s missing prerequisite is fixed by the engine\'s action', () => {
  const kind = (k: string, example: string): CatalogKind => ({
    kind: k, title: k, summary: '', example, fails_because: '', targets: [], offer: {}, fields: [], prerequisites: [],
  })
  const kinds = [kind('increases', '[search]\nnet = "A"\n'), kind('rail_window', '[[scenario]]\nid = "boot"\n')]

  test('the block comes from the action id, with the engine\'s words on the button', () => {
    const fix = fixFor({ need: 'scenario', label: '[[scenario]]', fix: '', action: { id: 'add_scenario', label: 'Add a load-step test' } }, kinds)
    expect(fix).toEqual({ label: 'Add a load-step test', toml: '[[scenario]]\nid = "boot"\n' })
  })

  test('the need word is not what picks it: an action for another block wins', () => {
    const fix = fixFor({ need: 'scenario', label: 's', fix: '', action: { id: 'add_firmware', label: 'Add firmware' } }, kinds)
    expect(fix).toEqual({ label: 'Add firmware', pickFirmware: true })
  })

  test('with no action the app adds only the two blocks it has always had, and a trace has none', () => {
    expect(fixFor({ need: 'search', label: '[search]', fix: '', action: null }, kinds)?.toml).toContain('[search]')
    expect(fixFor({ need: 'scenario', label: 's', fix: '', action: null }, kinds)).toBeNull()
    expect(fixFor({ need: 'trace', label: 't', fix: '', action: null }, kinds)).toBeNull()
  })
})
