// A part or net named inside a sentence is a chip, and the punctuation after it
// must hug it: "(from D13, driven by U1)." read "(from D13 , driven by U1 )."
// because every chip carried the same side padding and margin whatever followed
// it. The engine's sentence is fine (the CLI prints it unbroken); the gap was
// the chip's, so it is pinned here, at the rendering.

import { describe, expect, test } from 'bun:test'
import { renderToStaticMarkup } from 'react-dom/server'
import { Linked, ResultLinkContext } from '../src/components/report/ResultLinks'

const SENTENCE = 'Indicators: not checked for D1 (from D13, driven by U1).'
const vocab = { refs: new Set(['D1', 'U1']), nets: new Set(['D13']) }

const html = (text: string) =>
  renderToStaticMarkup(
    <ResultLinkContext.Provider value={{ vocab }}>
      <Linked text={text} />
    </ResultLinkContext.Provider>,
  )

/** The chip buttons, by name, with their inline style. */
const chips = (markup: string) =>
  Object.fromEntries(
    [...markup.matchAll(/<button[^>]*data-name="([^"]+)"[^>]*style="([^"]*)"/g)].map(m => [m[1], m[2]]),
  )

describe('chips inside a sentence', () => {
  const markup = html(SENTENCE)

  test('the sentence reads whole once the markup is stripped', () => {
    expect(markup.replace(/<[^>]+>/g, '')).toBe(SENTENCE)
  })

  test('no whitespace sits between a chip and the punctuation that follows it', () => {
    // The chip and its closing mark share one unbreakable span, so a line never
    // starts with a stray comma, and no whitespace sits between them.
    expect(markup).toMatch(/<\/button>,<\/span><span> driven by <\/span>/)
    expect(markup).toMatch(/<span data-testid="chip-with-punct" style="white-space:nowrap"><button/)
    expect(markup).toMatch(/<\/button>\)\.<\/span><span><\/span>$/)
  })

  test('a chip against closing punctuation has no space on that side', () => {
    const style = chips(markup)
    // D13 is followed by a comma, U1 by ")." and D1 by a space then "(from".
    expect(style.D13).toContain('padding-right:3px')
    expect(style.D13).toContain('margin-right:0')
    expect(style.U1).toContain('padding-right:3px')
    expect(style.D1).toContain('padding-right:6px')
  })

  test('a chip after an opening bracket is flush on the left', () => {
    const style = chips(html('Both (U1, D1) are open.'))
    expect(style.U1).toContain('padding-left:3px')
    expect(style.D1).toContain('padding-right:3px')
    expect(chips(markup).D13).toContain('padding-left:6px')
  })
})
