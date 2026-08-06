import { expect, test } from 'bun:test'
import { buildReportHtml } from '../src/lib/report-export'
import type { WebReport } from '../src/types/report'

test('the human export preserves evidence status and the actionable assumption chain', () => {
  const report = {
    ok: true,
    board_name: 'rail-check',
    file_name: 'rail-check.kicad_pcb',
    num_components: 1,
    num_nets: 1,
    headline: 'Evidence is undermined.',
    serious: 0,
    total: 0,
    sections: [],
    components: [],
    assumptions: [{
      id: 'open-part:U9',
      kind: 'open_part',
      source: 'binder',
      scope: { type: 'subjects', value: [{ kind: 'part', id: 'U9' }] },
      statement: 'U9 is treated as an open circuit.',
      because: 'No model matched <U9>.',
      consequence: 'The asserted rail has no simulated current path.',
      replacement: 'Add the U9 model and re-run.',
    }],
    evidence: [{
      assertion: '3V3 stays above 3.1 V',
      assumptions: ['open-part:U9'],
      status: 'undermined',
    }],
  } as WebReport

  const html = buildReportHtml({
    report,
    boardLabel: report.file_name,
    firmwareName: null,
    analyzedAt: Date.UTC(2026, 7, 6),
    engineVersion: '0.1.0',
    appVersion: '0.1.0',
    spec: null,
    checks: null,
    sessionName: null,
    restored: false,
  })
  expect(html).toContain('Evidence &amp; limitations')
  expect(html).toContain('3V3 stays above 3.1 V')
  expect(html).toContain('undermined')
  expect(html).toContain('U9 is treated as an open circuit.')
  expect(html).toContain('No model matched &lt;U9&gt;.')
  expect(html).toContain('The asserted rail has no simulated current path.')
  expect(html).toContain('Add the U9 model and re-run.')
})
