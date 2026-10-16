// The "Fix these first" hierarchy and the pre-layout state: the engine orders
// the causes, the browser renders that list, holds the copper findings back on
// an unrouted board, and collapses the groups that are not the next action.

import { describe, expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import { renderToStaticMarkup } from 'react-dom/server'
import { AdditionalEvidencePanel } from '../src/components/AdditionalEvidencePanel'
import { FixFirstCard, PreLayoutStrip } from '../src/components/report/Findings'
import { OpenSelectionsBanner } from '../src/components/SelectionCard'
import { openPartRef, reportView } from '../src/lib/report-view'
import type { EvidenceAssumption, FixFirst, WebOpenSelection, WebReport } from '../src/types/report'

const base = JSON.parse(
  readFileSync(new URL('./visual-lint/fixtures/analyze-watchy.json', import.meta.url), 'utf8'),
) as WebReport

const NOTICE = 'This board is not routed yet.'
const copperSection = {
  title: 'Copper spacing (DRC)',
  verdict: 'fail',
  findings: [
    { level: 'serious', what: 'pads overlap', why: 'w', fix: 'move one' },
    { level: 'serious', what: 'pads overlap', why: 'w', fix: 'move one' },
  ],
} as unknown as WebReport['sections'][number]
const fixFirst: FixFirst = {
  notice: NOTICE,
  items: [
    { level: 'serious', what: 'U1 pin 3 floats', why: '', fix: 'tie it to GND', x: 4, y: 5, similar: 2 },
    { level: 'serious', what: 'the board is not routed yet', why: '', fix: 'route it', similar: 0 },
  ],
  more: 1,
}

const open = (id: string): EvidenceAssumption => ({
  id: `open-part:${id}`, kind: 'open_part', source: 'bind', scope: { type: 'subjects', value: [{ kind: 'part', id }] },
  statement: '', because: '', consequence: '', replacement: '',
})

describe('fix first view', () => {
  test('no fix_first means no card and no pre-layout strip', () => {
    const v = reportView(base)
    expect(v.fixFirst).toBeNull()
    expect(v.preLayout).toBeNull()
  })

  test('a notice holds the copper section back without removing it from sections', () => {
    const v = reportView({
      ...base,
      fix_first: fixFirst,
      sections: [copperSection, ...base.sections.slice(1)],
    })
    expect(v.fixFirst?.items).toHaveLength(2)
    expect(v.preLayout?.notice).toBe(NOTICE)
    expect(v.preLayout?.copper?.title).toBe('Copper spacing (DRC)')
    expect(v.preLayout?.copper).toBe(v.sections[0])
  })

  test('open parts come from the open_part assumptions only', () => {
    const other = { ...open('X'), kind: 'other' }
    expect(openPartRef(open('C2'))).toBe('C2')
    expect(openPartRef(other)).toBeNull()
    expect(reportView({ ...base, assumptions: [open('C2'), other, open('F1')] }).openParts).toEqual(['C2', 'F1'])
  })

  test('the card lists the causes in order with their remedy and the overflow count', () => {
    const html = renderToStaticMarkup(<FixFirstCard fixFirst={fixFirst} onLocate={() => {}} />)
    expect(html).toContain('data-testid="fix-first"')
    expect(html.match(/data-testid="fix-first-item"/g)).toHaveLength(2)
    expect(html.indexOf('U1 pin 3 floats')).toBeLessThan(html.indexOf('route it'))
    expect(html).toContain('+2 more like this')
    expect(html).toContain('tie it to GND')
    expect(html).toContain('1 more serious cause')
  })

  test('the strip states what waits and counts the held-back copper findings', () => {
    const copper = reportView({ ...base, fix_first: fixFirst, sections: [copperSection] }).sections[0]
    const held = copper.groups.reduce((n, g) => n + g.items.length, 0)
    expect(held).toBeGreaterThan(0)
    const html = renderToStaticMarkup(<PreLayoutStrip notice={NOTICE} copper={copper} />)
    expect(html).toContain('data-testid="prelayout-strip"')
    expect(html).toContain(NOTICE)
    expect(html).toContain(`Copper findings held back (${held})`)
    expect(renderToStaticMarkup(<PreLayoutStrip notice={NOTICE} copper={null} />)).not.toContain('held back')
  })
})

describe('collapsed groups', () => {
  const sel = (reference: string, family = 'XYZ1234'): WebOpenSelection => ({
    reference, value: family, family, determined: null, via: 'board', same_pinout: false,
    members: [{ id: 'a', selector: 'A', basis: 'p. 3', verdict: 'U', reason: '' }],
  })

  test('identical open parts share one picker, a lone part keeps its own', () => {
    const grouped = renderToStaticMarkup(<OpenSelectionsBanner selections={[sel('U3'), sel('U4')]} onQueue={() => {}} />)
    expect(grouped).toContain('2 XYZ1234 parts')
    expect(grouped).toContain('data-testid="variant-picker-group"')
    expect(grouped).toContain('Pick one by one')
    const lone = renderToStaticMarkup(<OpenSelectionsBanner selections={[sel('U3'), sel('U9', 'OTHER')]} onQueue={() => {}} />)
    expect(lone).not.toContain('variant-picker-group')
  })

  test('the report placement of additional evidence starts closed', () => {
    const slots = {
      firmware: null, schematic: null, bom: null, placementFile: null, variant: null, asbuilt: null, models: [],
      onFirmware: () => {}, onSchematic: () => {}, onBom: () => {}, onPlacement: () => {},
      onVariant: () => {}, onAsbuilt: () => {}, onModels: () => {},
    }
    const html = renderToStaticMarkup(<AdditionalEvidencePanel placement="report" {...slots} />)
    expect(html).toMatch(/<details[^>]*data-testid="additional-evidence-report"/)
    expect(html).not.toMatch(/<details[^>]*\bopen=""/)
    expect(html).toContain('Add more evidence')
  })
})
