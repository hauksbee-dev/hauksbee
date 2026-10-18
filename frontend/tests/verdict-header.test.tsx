// The verdict header must agree with itself and with the page under it. Three
// defects from a real-browser pass over an unrouted board, pinned here:
//   1. the key-parts ring clipped a four-digit count to "000/500";
//   2. the card said "0 serious" and "none serious" beside a finding tagged
//      SERIOUS, and the section below said "1 serious";
//   3. the Checks strip counted "1 failed" for a check no row showed.
// The unrouted fixtures are real engine output (stormduino, from `hauksbee
// serve`): one serious finding (the board is not routed) among twelve.

import { describe, expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import { renderToStaticMarkup } from 'react-dom/server'
import { ResultPanel } from '../src/components/report/ResultPanel'
import { BoardGate } from '../src/components/checks/BoardGate'
import { RunStrip } from '../src/components/checks/RunStrip'
import { RING_FRACTION_MAX } from '../src/lib/gauge'
import { assertRowCount, reportView, resultsWithoutRows } from '../src/lib/report-view'
import { boardVerdict } from '../src/lib/status-vocab'
import type { RunResponse, WebReport } from '../src/types/report'

const fixture = <T,>(name: string): T =>
  JSON.parse(readFileSync(new URL(`./visual-lint/fixtures/${name}`, import.meta.url), 'utf8')) as T

const unrouted = fixture<WebReport>('analyze-unrouted.json')
const gateRun = fixture<RunResponse>('check-run-board-gate.json')

const card = (report: WebReport) =>
  renderToStaticMarkup(
    <ResultPanel
      report={report}
      view={reportView(report)}
      explanation={null}
      exportSlot={null}
      command="hauksbee run board.kicad_pcb --check"
    />,
  )

const withKeyParts = (bound: number, total: number): WebReport => ({
  ...unrouted,
  bind: { ...unrouted.bind!, critical_parts_bound: `${bound}/${total}` },
})

/** The text the ring draws inside its hole. */
const ringText = (html: string) => html.match(/data-testid="result-gauge-modelled-text"[^>]*>([^<]*)</)?.[1] ?? ''

describe('the key-parts ring', () => {
  test.each([1, 3, 4, 5, 6])('a %i-digit count never draws more than fits, and says the exact numbers', digits => {
    const total = 10 ** digits - 1
    for (const bound of [total, total - 1, Math.floor(total / 3)]) {
      const html = card(withKeyParts(bound, total))
      expect(ringText(html).length).toBeLessThanOrEqual(RING_FRACTION_MAX)
      // The exact numbers are on the page for any reader, in the label and the tooltip.
      expect(html).toContain(`aria-label="${bound} of ${total} key parts modelled"`)
      expect(html).toContain(`data-value="${bound}" data-total="${total}"`)
      if (digits >= 4) {
        const grouped = (n: number) => n.toLocaleString('en-US')
        expect(html).toContain(`${grouped(bound)} of ${grouped(total)}`)
      }
    }
  })

  test('5000 of 5000 is not clipped to a fragment of itself', () => {
    const html = card(withKeyParts(5000, 5000))
    expect(ringText(html)).toBe('100%')
    expect(html).toContain('5,000 of 5,000')
    expect(html).not.toMatch(/>\d*000\/500</)
  })

  test('a small count keeps its fraction inside the ring', () => {
    expect(ringText(card(withKeyParts(4, 5)))).toBe('4/5')
  })
})

describe('every number on an unrouted board agrees', () => {
  const html = card(unrouted)
  const counters = [...html.matchAll(/text-\[20px\][^>]*>(\d+)</g)].map(m => Number(m[1]))

  test('the counters are the report numbers, and the serious one is not zero', () => {
    expect(unrouted.serious).toBe(1)
    expect(counters).toEqual([unrouted.serious, unrouted.total])
  })

  test('the headline quotes the counters and names the routing', () => {
    expect(unrouted.headline).toContain(`${unrouted.total} issues found, ${unrouted.serious} serious`)
    expect(unrouted.headline).toContain('missing routing')
    expect(html).not.toContain('none serious')
    expect(html).not.toContain('no findings')
  })

  test('the card lists the serious finding and says what the rows leave out', () => {
    expect(html.match(/data-testid="result-finding"/g)).toHaveLength(1)
    expect(html).toContain('>serious<')
    expect(html).toMatch(/and 11 more findings in the sections below/)
  })

  test('the sections add up to the counters, outside the copper held back', () => {
    const own = unrouted.sections.filter(s => !s.title.startsWith('Copper spacing'))
    expect(own.reduce((n, s) => n + s.findings.length, 0)).toBe(unrouted.total)
    expect(own.reduce((n, s) => n + s.findings.filter(f => f.level === 'serious').length, 0)).toBe(unrouted.serious)
    const wiring = unrouted.sections.find(s => s.title === 'Connectivity & wiring')!
    expect(wiring.verdict).toBe(`${wiring.findings.length} issues found, 1 serious.`)
  })

  test('the header chip says what the one serious finding is', () => {
    expect(boardVerdict(unrouted)).toMatchObject({ term: 'fail', detail: 'not routed yet' })
  })

  test('copper findings held back are not listed as serious beside a counter that leaves them out', () => {
    const held = {
      title: 'Copper spacing (DRC)',
      verdict: 'fail',
      findings: [{ level: 'serious', what: 'two pads overlap here', why: 'w', fix: 'move one' }],
    } as unknown as WebReport['sections'][number]
    const report: WebReport = { ...unrouted, sections: [held, ...unrouted.sections.slice(1)] }
    const view = reportView(report)
    expect(view.preLayout?.copper).toBe(view.sections[0])
    expect(card(report)).not.toContain('two pads overlap here')
  })
})

describe('the Checks page shows every failure it counts', () => {
  const results = gateRun.results ?? []
  // The spec the page builds for this board asks for one check.
  const spec = '[[assert]]\nkind = "no_faults"\n'

  test('the run has more results than the spec has rows, and the failed one is past them', () => {
    expect(assertRowCount(spec)).toBe(1)
    expect(results).toHaveLength(2)
    expect(results.filter(r => !r.invalid && !r.passed)).toHaveLength(1)
    expect(resultsWithoutRows(results, spec)).toEqual([results[1]])
  })

  test('a spec whose rows own every result leaves nothing to add', () => {
    expect(resultsWithoutRows(results, `${spec}\n[[assert]]\nkind = "x"\n`)).toEqual([])
  })

  test('the failed result is listed with its reason and its fix, not only behind Details', () => {
    const html = renderToStaticMarkup(<BoardGate results={resultsWithoutRows(results, spec)} stale={false} />)
    expect(html).toContain('data-testid="board-gate"')
    expect(html).toContain('FAIL')
    expect(html).toContain('the board is not routed yet')
    expect(html).toContain('Fix: ')
    expect(html).toContain('Route all the nets')
    // Nothing to say, nothing drawn.
    expect(renderToStaticMarkup(<BoardGate results={[]} stale={false} />)).toBe('')
  })

  test('the strip counts the same failure the card lists', () => {
    const strip = renderToStaticMarkup(
      <RunStrip
        result={gateRun} stale={false} running={false} canRun validationCount={0} reportSerious={0}
        detailsOpen={false} onToggleDetails={() => {}} onJump={() => {}}
      />,
    )
    expect(strip).toMatch(/data-testid="run-count-fail"[^>]*>1 failed/)
  })
})
