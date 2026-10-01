import { describe, expect, test } from 'bun:test'
import { renderToStaticMarkup } from 'react-dom/server'
import { OpenSelectionsBanner } from '../src/components/SelectionCard'
import { buildToml, tomlToBuilder, upsertOverride } from '../src/lib/check-spec'
import type { WebOpenSelection } from '../src/types/report'

describe('variant selection overrides', () => {
  test('builder round-trips [[override]] and a re-pick replaces, not duplicates', () => {
    let rows = upsertOverride([], { ref: 'U3', select: 'xyz1234b' })
    rows = upsertOverride(rows, { ref: 'U3', select: 'xyz1234a', reason: 'picked in the app' })
    const toml = buildToml('n', '5', [], [], [], [], rows)
    expect(toml.match(/\[\[override\]\]/g)).toHaveLength(1)
    expect(tomlToBuilder(toml)?.overrides).toEqual([{ ref: 'U3', select: 'xyz1234a', reason: 'picked in the app' }])
    expect(tomlToBuilder('[[override]]\nref = "U3"\nbogus = 1\n')).toBeNull()
  })

  test('banner names the state and offers each member with its page', () => {
    const selection = (same_pinout: boolean): WebOpenSelection => ({
      reference: 'U3', value: 'XYZ1234', family: 'XYZ1234', determined: null, via: 'board', same_pinout,
      members: [
        { id: 'xyz1234a', selector: 'XYZ1234ADBV', basis: 'p. 3', verdict: 'U', reason: '' },
        { id: 'xyz1234b', selector: 'XYZ1234BDBV', basis: 'p. 4', verdict: 'U', reason: '' },
      ],
    })
    const open = renderToStaticMarkup(<OpenSelectionsBanner selections={[selection(false)]} onQueue={() => {}} />)
    expect(open).toContain('selection required')
    expect(open).toContain('xyz1234b: XYZ1234BDBV · p. 4')
    const same = renderToStaticMarkup(<OpenSelectionsBanner selections={[selection(true)]} />)
    expect(same).toContain('simulated as xyz1234a; CI checks every variant')
  })
})
