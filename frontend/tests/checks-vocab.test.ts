// The Checks page reads a run with the engine's own words: the static-check
// kind and leads (hauksbee-ir vocabulary) and the not-routed prefix the engine
// keys its pre-layout state off (hauksbee-extract). The TS copies in
// lib/status-vocab.ts must match the Rust constants byte for byte, so the page
// never forks the engine's wording.

import { describe, expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import {
  NOT_ROUTED_MESSAGE_PREFIX, STATIC_CHECK_FAILS_LEAD, STATIC_CHECK_KIND, STATIC_CHECK_NOT_JUDGED_LEAD,
} from '../src/lib/status-vocab'

const rust = (path: string) => readFileSync(new URL(`../../${path}`, import.meta.url), 'utf8')

/** The value of `pub const NAME: &str = "...";`, with Rust's `\`-newline
 *  continuation applied (the newline and the next line's leading spaces go). */
function rustStr(source: string, name: string): string {
  const m = new RegExp(`pub const ${name}: &str =\\s*"((?:[^"\\\\]|\\\\.)*)";`, 's').exec(source)
  if (!m) throw new Error(`${name} not found`)
  return m[1].replace(/\\\n\s*/g, '').replace(/\\"/g, '"').replace(/\\\\/g, '\\')
}

describe('the Checks page words match the engine', () => {
  const vocab = rust('crates/hauksbee-ir/src/vocabulary.rs')
  const lint = rust('crates/hauksbee-extract/src/layout_lint.rs')

  test('static-check kind and leads', () => {
    expect(STATIC_CHECK_KIND).toBe(rustStr(vocab, 'STATIC_CHECK_KIND'))
    expect(STATIC_CHECK_FAILS_LEAD).toBe(rustStr(vocab, 'STATIC_CHECK_FAILS_LEAD'))
    expect(STATIC_CHECK_NOT_JUDGED_LEAD).toBe(rustStr(vocab, 'STATIC_CHECK_NOT_JUDGED_LEAD'))
  })

  test('the not-routed prefix', () => {
    expect(NOT_ROUTED_MESSAGE_PREFIX).toBe(rustStr(lint, 'NOT_ROUTED_MESSAGE_PREFIX'))
  })
})
