// The report exports (the HTML page and the Markdown summary) say things in the
// reader's words: no engine id, input-format id or verdict id is printed
// raw, and a name the board itself uses is never rewritten.

import { describe, expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import { buildReportHtml } from '../src/lib/report-export'
import { reportMarkdown } from '../src/lib/result-export'
import { boardNames, isLabelledId, withLabels } from '../src/lib/plain-ids'
import type { WebReport } from '../src/types/report'

const fixture = (name: string) =>
  JSON.parse(readFileSync(new URL(`./visual-lint/fixtures/${name}.json`, import.meta.url), 'utf8')) as WebReport

const html = (report: WebReport) => buildReportHtml({
  report, boardLabel: null, firmwareName: null, analyzedAt: Date.UTC(2026, 9, 18, 12, 0, 0),
  engineVersion: '0.1.0', appVersion: '0.1.0', spec: null, checks: null, sessionName: null, restored: false,
})
const markdown = (report: WebReport) => reportMarkdown({
  report, boardLabel: null, firmwareName: null, engineVersion: '0.1.0', analyzedAt: Date.UTC(2026, 9, 18, 12, 0, 0),
})
/** The visible text of the page: tags and the style block dropped. */
const visible = (page: string) => page
  .replace(/<style[\s\S]*?<\/style>/g, '')
  .replace(/<title>[\s\S]*?<\/title>/g, '')
  .replace(/<[^>]+>/g, '\n')
  .replace(/&#39;/g, "'")

const HIGH = 'generic_pmos_sot_corner_high_vth'
const LOW = 'generic_pmos_sot_corner_low_vth'

describe('withLabels', () => {
  test('says an engine model id in words and leaves everything else as written', () => {
    const text = `Q1 passes across assumed corners (${HIGH}, ${LOW}); add value_re or mpn_re; net spi_clk; U1.kicad_pcb`
    const said = withLabels(text)
    expect(said).toContain('Generic small-signal P-channel MOSFET at its high-threshold corner')
    expect(said).toContain('Generic small-signal P-channel MOSFET at its low-threshold corner')
    expect(said).not.toContain(HIGH)
    // A TOML key, a lower-case net name and a file name are not engine ids.
    expect(said).toContain('value_re or mpn_re')
    expect(said).toContain('net spi_clk')
    expect(said).toContain('U1.kicad_pcb')
  })

  test('a name the board uses is kept even when it is shaped like an id', () => {
    const keep = boardNames({ nets: [HIGH], components: [{ reference: 'Q1' }] })
    expect(withLabels(`net ${HIGH}`, keep)).toBe(`net ${HIGH}`)
    expect(isLabelledId(HIGH)).toBe(true)
    expect(isLabelledId('spi_clk')).toBe(false)
  })

  test('a stand-in model and a coverage stage both read as words', () => {
    expect(withLabels("the generic fallback model 'signal_diode_1n4148_fallback'")).toContain('Signal diode 1n4148 fallback')
    expect(withLabels('at executable_scope_unspecified')).toBe('at Coverage not listed')
  })
})

describe('the HTML export', () => {
  const page = visible(html(fixture('analyze-conditional-pass')))

  test('prints no model id, no input-format id and no assumption id', () => {
    for (const raw of [HIGH, LOW, 'ki_cad_pcb', 'reduced-fidelity:', 'reduced_fidelity']) {
      expect(page).not.toContain(raw)
    }
    expect(page).toContain('Generic small-signal P-channel MOSFET at its high-threshold corner')
    expect(page).toContain('KiCad board')
  })

  test('an assumption is tagged with the kind of gap and the assertion rests on what was assumed', () => {
    expect(page).toContain('Reduced fidelity')
    // The "Rests on" cell holds the statements, not their ids.
    expect(page).toContain('Q1 (Q_PMOS_GSD) is modelled only in part.')
  })

  test('the board name and file name are left exactly as written', () => {
    expect(page).toContain('generic_pmos_switch_gate_0v')
    expect(page).toContain('generic_pmos_switch_gate_0v.kicad_pcb')
  })
})

describe('the Markdown export', () => {
  test('names the verdict in the CLI word only, and prints no model id', () => {
    for (const name of ['analyze-conditional-pass', 'analyze-conditional-fail', 'analyze-partly-judged']) {
      const text = markdown(fixture(name))
      expect(text).toMatch(/\*\*VERDICT: [^*]+\*\* · /)
      expect(text).not.toMatch(/\(`(pass|fail|invalid)`\)/)
      expect(text.replace(/\\/g, '')).not.toContain('generic_pmos_sot_corner')
    }
  })
})
