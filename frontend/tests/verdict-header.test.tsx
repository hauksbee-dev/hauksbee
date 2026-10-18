// The verdict header must agree with itself and with the page under it. Three
// defects from a real-browser pass over an unrouted board, pinned here:
//   1. the key-parts ring clipped a four-digit count to "000/500";
//   2. the card said "0 serious" and "none serious" beside a finding tagged
//      SERIOUS, and the section below said "1 serious";
//   3. the Checks strip counted "1 failed" for a check no row showed. The
//      redesigned page reads the not-routed finding as layout readiness: a
//      calm note with the nets it names, never a count with nothing behind it.
// The unrouted fixtures are real engine output (stormduino, from `hauksbee
// serve`): one serious finding (the board is not routed) among twelve.

import { describe, expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import { renderToStaticMarkup } from 'react-dom/server'
import { ResultPanel } from '../src/components/report/ResultPanel'
import { VerdictBand } from '../src/components/checks/VerdictBand'
import { RING_FRACTION_MAX } from '../src/lib/gauge'
import { assertRowCount, findingEntries, readStaticFinding, reportView, resultsWithoutRows, runMarkdown, runView, sectionStatus } from '../src/lib/report-view'
import { FindingsList } from '../src/components/report/Findings'
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
  const view = reportView(unrouted)
  const entries = findingEntries(view)
  const list = renderToStaticMarkup(
    <FindingsList entries={entries} status={sectionStatus(view)} openParts={null} openRefs={new Set()} preLayout={view.preLayout} />,
  )

  test('the card says the verdict once, with the one serious finding named', () => {
    expect(unrouted.serious).toBe(1)
    expect(html.match(/data-testid="result-verdict-word"/g)).toHaveLength(1)
    expect(html).toMatch(/data-testid="result-verdict-word"[^>]*>FAIL</)
    expect(html).toMatch(/data-testid="result-verdict-detail"[^>]*>not routed yet</)
    // "Not routed" is said by the verdict detail, not again by the sentence.
    expect(html.match(/>[^<]*not routed/gi)).toHaveLength(1)
  })

  test('the headline quotes the counters and names the routing', () => {
    expect(unrouted.headline).toContain(`${unrouted.total} issues found, ${unrouted.serious} serious`)
    expect(unrouted.headline).toContain('missing routing')
    expect(html).not.toContain('none serious')
    expect(html).not.toContain('no findings')
  })

  test('the findings beside the board are every counted finding, the serious one first', () => {
    const found = entries.filter(e => /^s\d+g/.test(e.key))
    expect(found.reduce((n, e) => n + e.items.length, 0)).toBe(unrouted.total)
    expect(entries[0].level).toBe('serious')
    expect(entries.filter(e => e.level === 'serious').reduce((n, e) => n + e.items.length, 0)).toBe(unrouted.serious)
    expect(list).toMatch(/data-testid="findings-count"[^>]*>1 serious/)
    // No filler for a section that found nothing: one quiet line instead.
    expect(list).not.toContain('Looks healthy')
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
    expect(findingEntries(view).some(e => e.title.includes('two pads overlap here'))).toBe(false)
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

  const band = (spec_: string) => renderToStaticMarkup(
    <VerdictBand
      result={gateRun} runKey="k" stale={false} running={false} canRun onRun={() => {}} validationCount={0}
      reportSerious={0} onJump={() => {}} specFileName="spec.toml" specToml={spec_} rawMode={false}
      preLayoutNotice={null} exportMenu={null}
    />,
  )

  test('the not-routed finding is layout readiness, not a failure of these checks', () => {
    const v = runView(gateRun, spec)
    expect(v.rows).toEqual([results[0]])
    expect(v.layout).toBe(results[1])
    expect(v.boardFindings).toEqual([])
    // The page's verdict is the spec's own check: not judged, not failed.
    expect(v.verdict).toBe('invalid')
    // The CLI's verdict over the same run is unchanged: the run still fails.
    expect(gateRun.passed).toBe(false)
  })

  test('the static finding reads back as plain words, every net it names, and its fix', () => {
    const f = readStaticFinding(results[1])!
    expect(f.plain.startsWith('the board is not routed yet')).toBe(true)
    expect(f.plain).not.toContain('static check')
    expect(f.severity).toBe('serious')
    expect(f.subjects).toEqual(results[1].all_subjects ?? [])
    expect(f.fix).toContain('Route all the nets')
    expect(readStaticFinding(results[0])).toBeNull()
  })

  test('the band says the layout is not routed, calmly, with what CI will do', () => {
    const html = band(spec)
    expect(html).toContain('data-testid="layout-note"')
    expect(html).toContain('The board is not routed yet')
    expect(html).toContain('A CI run of this spec also fails until the board is routed.')
    expect(html).toContain('How to finish the layout')
    // No failure is counted that the page does not show.
    expect(html).not.toContain('data-testid="run-count-fail"')
    expect(html).not.toContain('data-testid="board-gate"')
    // No internal ids or engine leads on the page.
    expect(html).not.toContain('static_check')
    expect(html).not.toContain('hauksbee check')
  })

  test('a board finding that is not layout still fails the page verdict and is listed', () => {
    const short = { ...results[1], detail: results[1].detail.replace(/the board is not routed yet[^]*$/, 'two nets are shorted: +5V and GND'), all_subjects: ['+5V', 'GND'] }
    const run = { ...gateRun, results: [results[0], short] }
    const v = runView(run, spec)
    expect(v.layout).toBeNull()
    expect(v.boardFindings).toEqual([short])
    expect(v.verdict).toBe('failed')
  })

  test('the Markdown export carries the verdict, each check and the layout line', () => {
    const md = runMarkdown({ result: gateRun, specToml: spec, specFileName: 'spec.toml', boardLabel: 'stormduino', title: r => r.kind })
    expect(md).toContain('**Verdict: INVALID**')
    expect(md).toContain('| INVALID | no_faults |')
    expect(md).toContain('**Layout:** the board is not routed yet')
    expect(md).toContain('Reproduce: `hauksbee-ci run ci/spec.toml`')
  })
})
