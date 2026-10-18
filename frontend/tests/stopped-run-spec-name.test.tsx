// A browser check run stopped at its ceiling says which terminal command to run.
// The engine writes the spec placeholder (see hauksbee_ir::vocabulary); the page
// must show the file the person downloaded in its place, in both the run strip
// and the details drawer.

import { describe, expect, test } from 'bun:test'
import { renderToStaticMarkup } from 'react-dom/server'
import { RunResults } from '../src/components/checks/RunResults'
import { RunStrip } from '../src/components/checks/RunStrip'
import { namedSpecIn, SPEC_FILE_PLACEHOLDER } from '../src/lib/ci-workflow'
import type { RunResponse } from '../src/types/report'

// The engine's stopped-run message, as `check_run_stopped` words it.
const STOPPED = 'The check run was stopped after 10 minutes while "Simulating" (42% of the estimated work done) '
  + 'because it passed the time the browser gives a board this size. Download this spec from the page and run '
  + 'it in a terminal, which has no time limit: `hauksbee-ci run <spec.toml>`.'

const stopped: RunResponse = { ok: false, error: STOPPED }

const strip = (result: RunResponse | null) => renderToStaticMarkup(
  <RunStrip
    result={result} stale={false} running={false} canRun validationCount={0} reportSerious={0}
    detailsOpen={false} onToggleDetails={() => {}} onJump={() => {}} specFileName="chain2500.toml"
  />,
)

describe('a stopped check run names the downloaded spec', () => {
  test('the placeholder constant is the one the engine writes', () => {
    expect(SPEC_FILE_PLACEHOLDER).toBe('<spec.toml>')
    expect(STOPPED).toContain(SPEC_FILE_PLACEHOLDER)
  })

  test('namedSpecIn replaces every placeholder and passes an absent message through', () => {
    expect(namedSpecIn(STOPPED, 'chain2500.toml')).toContain('`hauksbee-ci run chain2500.toml`.')
    expect(namedSpecIn(STOPPED, 'chain2500.toml')).not.toContain(SPEC_FILE_PLACEHOLDER)
    expect(namedSpecIn('no placeholder here', 'chain2500.toml')).toBe('no placeholder here')
    expect(namedSpecIn(undefined, 'chain2500.toml')).toBeUndefined()
  })

  test('the run strip shows the real file name, not the placeholder', () => {
    const html = strip(stopped)
    expect(html).toContain('hauksbee-ci run chain2500.toml')
    expect(html).not.toContain('spec.toml&gt;')
    expect(html).not.toContain(SPEC_FILE_PLACEHOLDER)
  })

  test('the details drawer shows the real file name too', () => {
    const html = renderToStaticMarkup(
      <RunResults
        result={stopped} runKey="k" stale={false} rawMode={false} reportSerious={0}
        specFileName="chain2500.toml"
      />,
    )
    expect(html).toContain('hauksbee-ci run chain2500.toml')
    expect(html).not.toContain('spec.toml&gt;')
  })
})
