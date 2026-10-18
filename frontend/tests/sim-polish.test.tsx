// Live-sim polish: the header chip says "ready" until the first Play, the live
// net card drops its mini trace once the net is on the scope, the embedded
// check card does not repeat its host's title, and the scope tokens are legible.

import { describe, expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import { renderToStaticMarkup } from 'react-dom/server'
import { ShellChips } from '../src/components/ShellChips'
import { LiveNetCard } from '../src/components/sim/LiveNetCard'
import { SelectionCard } from '../src/components/SelectionCard'
import type { WebReport } from '../src/types/report'
import type { NetReading } from '../src/lib/net-state'

const noop = () => {}

const simChip = (sim: Record<string, unknown>) =>
  renderToStaticMarkup(
    <ShellChips
      view="sim"
      report={{ ok: true, serious: 0, total: 0 } as unknown as WebReport}
      checks={null}
      sim={{ faults: 0, connected: true, sessionBoard: 'b.kicad_pcb', ...sim } as never}
      simMounted
      simOffline={false}
      sessionBoard="b.kicad_pcb"
      sessionMatchesCurrent
      currentBoard="b.kicad_pcb"
      serverLiveActive
      goTo={noop}
      onOpenLiveSession={noop}
      onDriveLive={noop}
    />,
  )

describe('header sim chip', () => {
  test('says ready before the first Play, paused after, running while it runs', () => {
    expect(simChip({ running: false, runState: 'ready' })).toContain('sim ready')
    expect(simChip({ running: false, runState: 'ready' })).not.toContain('sim paused')
    expect(simChip({ running: false, runState: 'paused' })).toContain('sim paused')
    expect(simChip({ running: true, runState: 'running' })).toContain('sim running')
  })
})

const reading: NetReading = { kind: 'measured', volts: 5, moving: false, min: 5, max: 5 } as NetReading

const netCard = (probed: boolean) =>
  renderToStaticMarkup(
    <LiveNetCard
      net="+5V" reading={reading} frame={null} frames={[]} probed={probed}
      onProbe={noop} onUnprobe={noop} onClose={noop}
    />,
  )

describe('live net card', () => {
  test('shows its own trace until the net is on the scope, then only the scope has it', () => {
    expect(netCard(false)).toContain('data-testid="live-net-trace"')
    const probed = netCard(true)
    expect(probed).not.toContain('data-testid="live-net-trace"')
    expect(probed).toContain('live-net-unprobe')
  })
})

describe('selection card, embedded', () => {
  const render = (embedded: boolean) =>
    renderToStaticMarkup(
      <SelectionCard net="+5V" component={null} reading={reading} embedded={embedded} onAddProbe={noop} onClose={noop} />,
    )
  test('has no title row, net name or probe button of its own', () => {
    const plain = render(false)
    expect(plain).toContain('NET · LIVE')
    expect(plain).toContain('selection-add-probe')
    const inner = render(true)
    expect(inner).not.toContain('NET · LIVE')
    expect(inner).not.toContain('selection-add-probe')
    expect(inner).not.toContain('Close selection')
  })
})

describe('part card with no model', () => {
  test('offers the model as its one primary action even without a coverage row', () => {
    const html = renderToStaticMarkup(
      <SelectionCard
        net={null} component={{ ref: 'Q1', value: 'BS250', lib_id: '' }} boundKind={null}
        onAuthorModel={noop} onClose={noop}
      />,
    )
    expect(html).toContain('selection-author-model')
    expect(html).toContain('Give Q1 a model')
  })
})

// WCAG relative luminance contrast of a (possibly translucent) colour over a face.
const lin = (c: number) => { const v = c / 255; return v <= 0.03928 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4 }
const lum = ([r, g, b]: number[]) => 0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b)
const contrast = (a: number[], b: number[]) => {
  const [hi, lo] = [lum(a), lum(b)].sort((x, y) => y - x)
  return (hi + 0.05) / (lo + 0.05)
}
const hex = (h: string) => [1, 3, 5].map(i => parseInt(h.slice(i, i + 2), 16))

describe('scope tokens', () => {
  const css = readFileSync(new URL('../src/index.css', import.meta.url), 'utf8')
  const over = (token: string, theme: 'dark' | 'light') => {
    const block = theme === 'dark' ? css.slice(0, css.indexOf("[data-theme='light']")) : css.slice(css.indexOf("[data-theme='light']"))
    const face = hex(/--scope-face: (#[0-9a-f]{6})/.exec(block)![1])
    const m = new RegExp(`--${token}: rgba?\\((\\d+), (\\d+), (\\d+)(?:, ([\\d.]+))?\\)`).exec(block)!
    const a = m[4] === undefined ? 1 : parseFloat(m[4])
    const fg = [1, 2, 3].map(i => Math.round(parseInt(m[i]) * a + face[i - 1] * (1 - a)))
    return contrast(fg, face)
  }
  for (const theme of ['dark', 'light'] as const) {
    test(`${theme}: tick and hint text are AA (4.5:1), the axis line 3:1`, () => {
      expect(over('scope-tick', theme)).toBeGreaterThanOrEqual(4.5)
      expect(over('scope-hint', theme)).toBeGreaterThanOrEqual(4.5)
      expect(over('scope-axis', theme)).toBeGreaterThanOrEqual(3)
    })
  }
})
