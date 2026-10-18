// Settings and Environment wording and choices that must stay coherent:
// one recommended setup, an escalation sentence that agrees with the option
// selected, and an install size that names this machine's OS only.

import { describe, expect, test } from 'bun:test'
import { escalationSummary, featuredPresets } from '../src/components/SettingsView'
import { costForOs } from '../src/components/DepsPanel'
import type { ExtractBackendAvailability, ExtractPreset, ExtractSettings } from '../src/types/report'

const preset = (id: string, backend: string, recommended: boolean, cheaper = false): ExtractPreset => ({
  id, label: id, summary: '', backend, model: 'm', effort: 'high', api_base: '', api_key_env: '', recommended, cheaper,
})
const avail = (backend: string, available: boolean): ExtractBackendAvailability => ({
  id: backend, label: backend, backend, available, detail: '', install: '', sends_data_to: '',
})
// The engine decides which preset a fresh install is steered to and which preset
// is the cheaper way to use a tool; the page only reads those two facts.
const settings = (installed: string[], defaultPreset?: string): ExtractSettings => ({
  presets: [
    preset('claude-code', 'claude-code', true),
    preset('claude-code-haiku', 'claude-code', false, true),
    preset('agy', 'agy', true),
    preset('codex', 'codex', true),
    preset('codex-luna', 'codex', false, true),
    preset('openai', 'api', false),
  ],
  default_preset: defaultPreset,
  backends: ['claude-code', 'agy', 'codex', 'api'].map(b => avail(b, installed.includes(b))),
}) as unknown as ExtractSettings

describe('one recommended setup', () => {
  test('it is the preset the engine steers a fresh install to, with that tool\'s cheaper option', () => {
    const { recommended, cheaper } = featuredPresets(settings(['claude-code', 'agy', 'codex'], 'codex'))
    expect(recommended?.id).toBe('codex')
    expect(cheaper?.id).toBe('codex-luna')
  })

  test('it follows the engine when it picks another tool, with that tool\'s cheaper option', () => {
    const { recommended, cheaper } = featuredPresets(settings(['claude-code'], 'claude-code'))
    expect(recommended?.id).toBe('claude-code')
    expect(cheaper?.id).toBe('claude-code-haiku')
  })

  test('an older server with no default_preset gets the first preset it marks recommended, in its own order', () => {
    expect(featuredPresets(settings([])).recommended?.id).toBe('claude-code')
  })

  test('a tool with no cheaper preset offers none', () => {
    expect(featuredPresets(settings(['agy'], 'agy')).cheaper).toBeUndefined()
  })
})

describe('escalation sentence agrees with the selected option', () => {
  const labels = { 'claude-sonnet-5-5': 'Sonnet 5.5', 'claude-opus-5-5': 'Opus 5.5' }
  const base = { firstModel: 'claude-sonnet-5-5', labels }

  test('a default of no escalation says so, and never claims a ladder', () => {
    const text = escalationSummary({ ...base, value: '', defaultLadder: [] })
    expect(text).toMatch(/no escalation/i)
    expect(text).not.toContain('→')
  })

  test('a default ladder names every step', () => {
    expect(escalationSummary({ ...base, value: '', defaultLadder: ['claude-opus-5-5'] })).toContain('Sonnet 5.5 → Opus 5.5')
  })

  test('a default hidden by a saved ladder is not guessed', () => {
    expect(escalationSummary({ ...base, value: '', defaultLadder: null })).toMatch(/built-in ladder/)
  })

  test('"none" means a failed draft is reported, whatever the default is', () => {
    const text = escalationSummary({ ...base, value: 'none', defaultLadder: ['claude-opus-5-5'] })
    expect(text).toMatch(/^No escalation/)
    expect(text).not.toContain('→')
  })

  test('a chosen ladder is shown in order', () => {
    expect(escalationSummary({ ...base, value: 'claude-opus-5-5', defaultLadder: [] })).toContain('Sonnet 5.5 → Opus 5.5')
  })
})

describe('install size for this machine', () => {
  const cost = 'about a 120 MB download on Windows (about 80 MB on Unix), a few hundred MB unpacked'

  test('a Mac or Linux machine sees only the Unix size', () => {
    expect(costForOs(cost, 'macos')).toBe('about 80 MB download, a few hundred MB unpacked')
    expect(costForOs(cost, 'linux')).not.toContain('Windows')
  })

  test('Windows sees only the Windows size', () => {
    expect(costForOs(cost, 'windows')).toBe('about 120 MB download, a few hundred MB unpacked')
  })

  test('another wording, or an unknown OS, is left as the engine wrote it', () => {
    expect(costForOs('a package-manager install', 'macos')).toBe('a package-manager install')
    expect(costForOs(cost, undefined)).toBe(cost)
  })
})
