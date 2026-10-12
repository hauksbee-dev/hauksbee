import { useCallback, useEffect, useMemo, useState } from 'react'
import { api, errorText } from '../lib/api'
import { tildeHome } from '../lib/display-path'
import type {
  ExtractAgentConfig, ExtractApiBackendConfig, ExtractBackendAvailability, ExtractBackendsConfig,
  ExtractPreset, ExtractResolved, ModelChoice, ExtractResolvedApi, ExtractSettings,
} from '../types/report'
import { BusyLine, Callout, LogWell, TerminalCommand } from './ui'
import { ChevronDownIcon, ChevronRightIcon } from './Icons'
import { budgetLabel } from './extract/activity'
import { escalateFromText, escalateToText, labelMap, ladderLabel } from './extract/escalate'

// The Settings view: one section for now, "Datasheet extraction", configuring
// which LLM backend `hauksbee models extract` (and the board panel's "draft a
// model" flow) runs against. GET /api/settings/extract carries everything the
// page needs to render itself (the saved config, every field resolved to its
// effective value, presets, per-backend availability, and the option lists) so
// this stays a thin editor over one server-owned shape rather than a second
// copy of the engine's defaulting logic.
//
// Nothing is written until Save. A preset "Use" click IS a write (the server
// applies it and echoes the new state back), matching `hauksbee models backend
// use <preset>` on the CLI side.

const BACKEND_IDS = ['claude-code', 'agy', 'codex', 'api'] as const
type BackendId = typeof BACKEND_IDS[number]

const BACKEND_FALLBACK_LABEL: Record<BackendId, string> = {
  'claude-code': 'Claude Code',
  agy: 'Antigravity (agy)',
  codex: 'Codex',
  api: 'OpenAI-compatible API',
}

interface DraftAgent {
  command: string
  model: string
  effort: string
  permission_mode: string
  profile: string
  extra_args: string
  /** Comma-separated model ids; blank = default ladder, `none` = no escalation. */
  escalate: string
}

interface Draft {
  backend: string
  retries: string
  timeout_secs: string
  codex: DraftAgent
  'claude-code': DraftAgent
  agy: DraftAgent
  /** The same three strings the server resolves; blank means its default. */
  api: ExtractResolvedApi
}

const agentDraft = (config?: ExtractAgentConfig): DraftAgent => ({
  command: config?.command ?? '',
  model: config?.model ?? '',
  effort: config?.effort ?? '',
  permission_mode: config?.permission_mode ?? '',
  profile: config?.profile ?? '',
  extra_args: (config?.extra_args ?? []).join(' '),
  escalate: escalateToText(config?.escalate),
})

const draftFromConfig = (config: ExtractBackendsConfig): Draft => ({
  backend: config.backend ?? '',
  retries: config.retries !== undefined ? String(config.retries) : '',
  timeout_secs: config.timeout_secs !== undefined ? String(config.timeout_secs) : '',
  codex: agentDraft(config.codex),
  'claude-code': agentDraft(config['claude-code']),
  agy: agentDraft(config.agy),
  api: {
    base_url: config.api?.base_url ?? '',
    model: config.api?.model ?? '',
    api_key_env: config.api?.api_key_env ?? '',
  },
})

const configFromDraft = (draft: Draft): ExtractBackendsConfig => {
  const config: ExtractBackendsConfig = {}
  if (draft.backend.trim()) config.backend = draft.backend.trim()
  if (draft.retries.trim() !== '') config.retries = Number(draft.retries)
  if (draft.timeout_secs.trim() !== '') config.timeout_secs = Number(draft.timeout_secs)

  for (const backend of ['codex', 'claude-code', 'agy'] as const) {
    const agent: ExtractAgentConfig = {}
    for (const key of ['command', 'model', 'effort', 'permission_mode', 'profile'] as const) {
      const value = draft[backend][key].trim()
      if (value) agent[key] = value
    }
    const args = draft[backend].extra_args.trim().split(/\s+/).filter(Boolean)
    if (args.length) agent.extra_args = args
    const escalate = escalateFromText(draft[backend].escalate)
    if (escalate) agent.escalate = escalate
    if (Object.keys(agent).length) config[backend] = agent
  }

  const apiCfg: ExtractApiBackendConfig = {}
  if (draft.api.base_url.trim()) apiCfg.base_url = draft.api.base_url.trim()
  if (draft.api.model.trim()) apiCfg.model = draft.api.model.trim()
  if (draft.api.api_key_env.trim()) apiCfg.api_key_env = draft.api.api_key_env.trim()
  if (Object.keys(apiCfg).length) config.api = apiCfg

  return config
}

/** Every non-blank field the draft would save, as `key=value` pairs in the
 *  exact dotted names `keys[]` documents (`claude-code.model`, `api.base_url`,
 *  ...), for the terminal-equivalent line. */
function draftArgs(config: ExtractBackendsConfig, saved?: ExtractBackendsConfig): string[] {
  const args: string[] = []
  if (config.backend) args.push(`backend=${config.backend}`)
  if (config.retries !== undefined) args.push(`retries=${config.retries}`)
  if (config.timeout_secs !== undefined) args.push(`timeout_secs=${config.timeout_secs}`)
  const block = (prefix: string, obj?: Record<string, unknown>) => {
    if (!obj) return
    for (const [k, v] of Object.entries(obj)) {
      if (v === undefined || v === null || v === '') continue
      if (k === 'escalate' && Array.isArray(v)) args.push(`${prefix}.${k}=${v.length ? v.join(',') : 'none'}`)
      else if (Array.isArray(v)) { if (v.length) args.push(`${prefix}.${k}=${v.join(' ')}`) }
      else args.push(`${prefix}.${k}=${v}`)
    }
  }
  block('codex', config.codex as Record<string, unknown> | undefined)
  block('claude-code', config['claude-code'] as Record<string, unknown> | undefined)
  block('agy', config.agy as Record<string, unknown> | undefined)
  block('api', config.api as Record<string, unknown> | undefined)
  const keys = new Set(args.map(arg => arg.split('=')[0]))
  for (const arg of saved ? draftArgs(saved) : []) {
    const key = arg.split('=')[0]
    if (!keys.has(key)) args.push(`${key}=`)
  }
  return args
}

function resolvedBlock(resolved: ExtractResolved, backend: string) {
  switch (backend) {
    case 'codex': return resolved.codex
    case 'claude-code': return resolved.claude_code
    case 'agy': return resolved.agy
    case 'api': return { model: resolved.api.model, effort: '', command: undefined }
    default: return { model: '', effort: '', command: undefined }
  }
}

/** The effective escalation ladder (after the first model) of an agent backend. */
function resolvedEscalate(resolved: ExtractResolved, backend: string): string[] | undefined {
  switch (backend) {
    case 'codex': return resolved.codex.escalate
    case 'claude-code': return resolved.claude_code.escalate
    case 'agy': return resolved.agy.escalate
    default: return undefined
  }
}

function presetMatchesResolved(preset: ExtractPreset, resolved: ExtractResolved): boolean {
  if (preset.backend !== resolved.backend) return false
  if (preset.backend === 'api') return preset.model === resolved.api.model && preset.api_base === resolved.api.base_url
  const { model, effort } = resolvedBlock(resolved, preset.backend)
  if (preset.model !== model || preset.effort !== effort) return false
  // Presets that differ only in their ladder (Sonnet alone vs Sonnet,
  // Opus) are told apart when both sides report one.
  const have = resolvedEscalate(resolved, preset.backend)
  return !preset.escalate || !have || preset.escalate.join(',') === have.join(',')
}

function sourceLine(data: ExtractSettings): string {
  const src = data.resolved.backend_source
  if (src.kind === 'env') return `overridden by $${src.name} for this server process`
  if (src.kind === 'flag') return 'set by a command-line flag for this server process'
  if (src.kind === 'auto') return 'auto-detected, nothing saved yet'
  const where = tildeHome(data.path)
  return data.exists ? `from ${where}` : `from ${where} (nothing saved yet)`
}

function availabilityFor(data: ExtractSettings, backend: string): ExtractBackendAvailability | undefined {
  return data.backends.find(b => b.backend === backend)
}

// ── Small building blocks ───────────────────────────────────────────────────

function Field({ label, hint, children }: { label: string; hint?: React.ReactNode; children: React.ReactNode }) {
  return (
    <label className="text-[11px] block min-w-0 max-w-full" style={{ color: 'var(--silk-faint)' }}>
      <span className="block mb-1">{label}</span>
      {children}
      {hint && <span className="block mt-1 leading-relaxed">{hint}</span>}
    </label>
  )
}

/** A monospace text field: paths, model ids and variable names read as code. */
function MonoInput({ testId, width, onChange, ...rest }: {
  testId: string
  width: string
  value: string
  placeholder?: string
  list?: string
  onChange: (v: string) => void
}) {
  return (
    <input
      data-testid={testId}
      className="hb-input text-[12px] block w-full"
      style={{ height: 30, maxWidth: width, fontFamily: 'var(--font-mono)' }}
      {...rest}
      onChange={e => onChange(e.target.value)}
    />
  )
}

/** The model picker: each pick with its note, one click to choose, plus a hint
 *  when the model in use is not on the list (it is still used as written).
 *  `hintable` is off for an OpenAI-compatible endpoint serving a non-GPT model. */
function ModelPicks({ picks, current, hintable, onPick }: {
  picks: ModelChoice[]
  current: string
  hintable: boolean
  onPick: (name: string) => void
}) {
  const listed = picks.some(p => p.name === current)
  return (
    <div className="mt-2 flex flex-col gap-1" data-testid="settings-model-picks">
      {picks.map(p => (
        <button
          key={p.name}
          type="button"
          data-testid={`settings-model-pick-${p.name}`}
          aria-pressed={p.name === current}
          onClick={() => onPick(p.name)}
          className="hb-press text-left text-[11px] cursor-pointer rounded-md px-2 py-1"
          style={{
            border: '1px solid var(--hairline)',
            background: p.name === current ? 'var(--copper-tint-strong)' : 'var(--surface-2)',
            color: 'var(--silk-dim)',
          }}
        >
          <span style={{ fontFamily: 'var(--font-mono)', color: p.name === current ? 'var(--copper-hi)' : 'var(--silk)' }}>{p.name}</span>
          {p.note && <span> &mdash; {p.note}</span>}
          {p.badge && (
            <span data-testid={`settings-model-badge-${p.name}`} style={{ color: 'var(--warn, var(--copper-hi))' }}>
              {' '}&middot; {p.badge}
            </span>
          )}
          {budgetLabel(p.context_budget) && (
            <span data-testid={`settings-model-harness-${p.name}`} style={{ color: 'var(--copper-hi)' }}>
              {' '}&middot; {budgetLabel(p.context_budget)}
            </span>
          )}
        </button>
      ))}
      {!listed && hintable && picks.length > 0 && (
        <span className="text-[11px]" data-testid="settings-model-hint" style={{ color: 'var(--silk-faint)' }}>
          {current} is not in the current list and is used as written; {(picks.find(p => p.default) ?? picks[0]).name} is recommended.
        </span>
      )}
    </div>
  )
}

function Segmented<T extends string>({ options, value, onChange, testIdPrefix, labels }: {
  options: readonly T[]
  value: T
  onChange: (v: T) => void
  testIdPrefix: string
  labels?: Partial<Record<T, string>>
}) {
  return (
    <div className="inline-flex flex-wrap rounded-lg overflow-hidden" style={{ border: '1px solid var(--hairline)' }}>
      {options.map((opt, i) => (
        <button
          key={opt || '(default)'}
          type="button"
          data-testid={`${testIdPrefix}-${opt || 'default'}`}
          aria-pressed={value === opt}
          onClick={() => onChange(opt)}
          className="hb-press px-2.5 py-1 text-[11px] cursor-pointer"
          style={{
            borderLeft: i > 0 ? '1px solid var(--hairline)' : 'none',
            background: value === opt ? 'var(--copper-tint-strong)' : 'var(--surface-2)',
            color: value === opt ? 'var(--copper-hi)' : 'var(--silk-dim)',
            fontWeight: value === opt ? 600 : 400,
          }}
        >
          {labels?.[opt] ?? (opt || 'Default')}
        </button>
      ))}
    </div>
  )
}

function PresetCard({ preset, labels, availability, customCommand, active, busy, onUse }: {
  preset: ExtractPreset
  /** Display names for the preset's backend's models, from the server. */
  labels: Record<string, string>
  customCommand?: string
  availability?: ExtractBackendAvailability
  active: boolean
  busy: boolean
  onUse: () => void
}) {
  const monoLine = preset.backend === 'api'
    ? [preset.api_base, preset.model].filter(Boolean).join(' · ')
    : `${preset.backend} · ${preset.escalate?.length ? ladderLabel(preset.model, preset.escalate, labels) : preset.model}${preset.effort ? ` · ${preset.effort} effort` : ''}`
  return (
    <div
      data-testid={`settings-preset-${preset.id}`}
      className="rounded-xl px-3.5 py-3 flex flex-col gap-1.5"
      style={{
        border: `1px solid ${active ? 'var(--copper-deep)' : 'var(--hairline)'}`,
        background: 'var(--surface)',
      }}
    >
      <div className="flex items-start justify-between gap-2">
        <span className="text-[13px] font-semibold" style={{ color: 'var(--silk)' }}>
          {preset.label}
          {preset.badge && (
            <span data-testid={`settings-preset-${preset.id}-badge`} className="ml-2 text-[10px] font-bold tracking-widest uppercase" style={{ color: 'var(--warn, var(--copper-hi))' }}>
              {preset.badge}
            </span>
          )}
        </span>
        {preset.recommended && availability?.available && (
          <span className="text-[9px] font-bold tracking-widest uppercase shrink-0 whitespace-nowrap" style={{ color: 'var(--copper-hi)' }}>
            Recommended
          </span>
        )}
      </div>
      <div className="text-[12px] leading-relaxed" style={{ color: 'var(--silk-dim)' }}>
        {customCommand ? `Keeps custom command ${customCommand}; its configuration controls routing and charges.` : preset.summary}
      </div>
      <div className="text-[11px] truncate" style={{ color: 'var(--silk-faint)', fontFamily: 'var(--font-mono)' }}>
        {monoLine}
      </div>
      <div className="flex items-center justify-between gap-2 mt-1">
        <span
          className="text-[10px] font-bold tracking-widest uppercase px-1.5 py-0.5 rounded"
          title={availability?.available ? undefined : availability?.install}
          style={{
            color: availability?.available ? 'var(--ok)' : 'var(--silk-faint)',
            background: availability?.available ? 'var(--ok-bg)' : 'var(--surface-2)',
            border: `1px solid ${availability?.available ? 'var(--ok-border)' : 'var(--hairline)'}`,
          }}
        >
          {preset.backend === 'api'
            ? (availability?.available ? 'key set' : 'no key set')
            : (availability?.available ? 'installed' : 'not installed')}
        </span>
        <button
          type="button"
          data-testid={`settings-preset-${preset.id}-use`}
          disabled={active || busy}
          onClick={onUse}
          className="hb-btn hb-press px-3 py-1 text-[12px]"
          style={{ height: 26 }}
        >
          {active ? 'Active' : busy ? 'Applying ...' : 'Use'}
        </button>
      </div>
    </div>
  )
}

// ── The view ─────────────────────────────────────────────────────────────

type FetchState =
  | { phase: 'loading' }
  | { phase: 'ready'; data: ExtractSettings }
  | { phase: 'unavailable'; reason: string }

type SaveState =
  | { phase: 'idle' }
  | { phase: 'saving' }
  | { phase: 'saved'; path: string }
  | { phase: 'error'; message: string }

type TestState =
  | { phase: 'idle' }
  | { phase: 'running'; log: string[] }
  | { phase: 'done'; log: string[]; reply: string }
  | { phase: 'error'; log: string[]; message: string }

export function SettingsView() {
  const [fetchState, setFetchState] = useState<FetchState>({ phase: 'loading' })
  const [draft, setDraft] = useState<Draft | null>(null)
  const [selectedBackend, setSelectedBackend] = useState<BackendId>('claude-code')
  const [advancedOpen, setAdvancedOpen] = useState(false)
  const [presetBusy, setPresetBusy] = useState<string | null>(null)
  const [presetError, setPresetError] = useState<string | null>(null)
  const [saveState, setSaveState] = useState<SaveState>({ phase: 'idle' })
  const [testState, setTestState] = useState<TestState>({ phase: 'idle' })

  const load = useCallback(async () => {
    setFetchState({ phase: 'loading' })
    try {
      const data = await api.extractSettings()
      setFetchState({ phase: 'ready', data })
      setDraft(draftFromConfig(data.config))
      setSelectedBackend((data.config.backend as BackendId) || (data.resolved.backend as BackendId) || 'claude-code')
    } catch (e) {
      setFetchState({ phase: 'unavailable', reason: errorText(e) })
    }
  }, [])

  useEffect(() => { void load() }, [load])

  const data = fetchState.phase === 'ready' ? fetchState.data : null

  const baseline = useMemo(
    () => (data ? JSON.stringify(configFromDraft(draftFromConfig(data.config))) : null),
    [data],
  )
  const draftJson = draft ? JSON.stringify(configFromDraft(draft)) : null
  const dirty = baseline !== null && draftJson !== null && draftJson !== baseline

  const args = draft ? draftArgs(configFromDraft(draft), data?.config) : []
  const quoted = args.map(arg => /^[\w./=:-]+$/.test(arg) ? arg : `'${arg.replaceAll("'", "'\\''")}'`)
  const terminalCommand = args.length ? `hauksbee models backend set ${quoted.join(' ')}` : 'hauksbee models backend show'

  const applyPreset = useCallback(async (id: string) => {
    setPresetBusy(id)
    setPresetError(null)
    try {
      const fresh = await api.extractSettingsPreset(id)
      setFetchState({ phase: 'ready', data: fresh })
      setDraft(draftFromConfig(fresh.config))
      setSelectedBackend((fresh.config.backend as BackendId) || (fresh.resolved.backend as BackendId) || 'claude-code')
      setSaveState({ phase: 'idle' })
    } catch (e) {
      setPresetError(errorText(e))
    } finally {
      setPresetBusy(null)
    }
  }, [])

  const save = useCallback(async () => {
    if (!draft) return
    setSaveState({ phase: 'saving' })
    try {
      const fresh = await api.extractSettingsSave(configFromDraft(draft))
      setFetchState({ phase: 'ready', data: fresh })
      setDraft(draftFromConfig(fresh.config))
      setSaveState({ phase: 'saved', path: fresh.path })
    } catch (e) {
      setSaveState({ phase: 'error', message: errorText(e) })
    }
  }, [draft])

  const discard = useCallback(() => {
    if (!data) return
    setDraft(draftFromConfig(data.config))
    setSaveState({ phase: 'idle' })
  }, [data])

  const runTest = useCallback(async () => {
    const log: string[] = []
    setTestState({ phase: 'running', log: [] })
    const append = (line: string) => { log.push(line); setTestState({ phase: 'running', log: [...log] }) }
    try {
      let settled = false
      await api.extractSettingsTest(({ event, data: frame }) => {
        if (event === 'log') append(frame)
        else if (event === 'done') { settled = true; setTestState({ phase: 'done', log: [...log], reply: frame }) }
        else if (event === 'error') { settled = true; setTestState({ phase: 'error', log: [...log], message: frame }) }
      })
      if (!settled) {
        setTestState({ phase: 'error', log: [...log], message: 'the connection closed before the test reported a result' })
      }
    } catch (e) {
      setTestState({ phase: 'error', log: [...log], message: `the test request failed: ${errorText(e)}` })
    }
  }, [])

  if (fetchState.phase === 'loading') {
    return (
      <div className="mt-10" data-testid="settings-view">
        <BusyLine className="justify-center mt-6" color="var(--silk-dim)">
          Reading extraction settings ...
        </BusyLine>
      </div>
    )
  }

  if (fetchState.phase === 'unavailable') {
    return (
      <div className="mt-10" data-testid="settings-view">
        <Callout tone="warn" testId="settings-unavailable" className="mt-4" title="Settings unavailable">
          This server does not expose the settings endpoints: {fetchState.reason}. Configuring the
          datasheet-extraction backend needs a full <code className="hb-inline">hauksbee serve</code>{' '}
          (or <code className="hb-inline">hauksbee run --serve</code>) session. From a terminal,{' '}
          <code className="hb-inline">hauksbee models backend setup</code> configures it there instead.
          <div className="mt-2">
            <button
              type="button"
              data-testid="settings-retry"
              onClick={() => void load()}
              className="hb-btn hb-press px-3 text-[12px]"
              style={{ height: 28 }}
            >
              Retry
            </button>
          </div>
        </Callout>
      </div>
    )
  }

  if (!data || !draft) return null

  const availability = availabilityFor(data, selectedBackend)
  const resolvedForSelected = resolvedBlock(data.resolved, selectedBackend)
  const agentBackend = selectedBackend === 'api' ? 'agy' : selectedBackend
  const selectedAgent = draft[agentBackend]
  const setAgent = (key: keyof DraftAgent) => (value: string) =>
    setDraft(d => d && { ...d, [agentBackend]: { ...d[agentBackend], [key]: value } })
  const setApiModel = (v: string) => setDraft(d => d && { ...d, api: { ...d.api, model: v } })
  const fields = {
    ...selectedAgent, extraArgs: selectedAgent.extra_args, permission: selectedAgent.permission_mode,
    setModel: setAgent('model'), setEffort: setAgent('effort'), setCommand: setAgent('command'),
    setExtraArgs: setAgent('extra_args'), setPermission: setAgent('permission_mode'), setProfile: setAgent('profile'),
    setEscalate: setAgent('escalate'),
  }

  return (
    <div className="mt-10" data-testid="settings-view">
      <div
        className="text-[11px] font-semibold tracking-[0.2em] uppercase text-center mb-5"
        style={{ color: 'var(--silk-faint)', fontFamily: 'var(--font-mono)' }}
      >
        Datasheet extraction
      </div>

      {/* ── Status strip ── */}
      <div
        className="rounded-xl px-4 py-3 text-[12px] flex flex-wrap items-start justify-between gap-x-6 gap-y-2"
        style={{ border: '1px solid var(--hairline)', background: 'var(--surface)', color: 'var(--silk-dim)' }}
        data-testid="settings-status"
      >
        <div className="min-w-0">
          <div>
            Reads datasheets with:{' '}
            <span style={{ color: 'var(--silk)', fontFamily: 'var(--font-mono)' }}>{data.summary}</span>
          </div>
          <div className="mt-1 text-[11px]" style={{ color: 'var(--silk-faint)' }} title={data.path}>{sourceLine(data)}</div>
        </div>
        <button
          type="button"
          data-testid="settings-test"
          disabled={testState.phase === 'running'}
          onClick={() => void runTest()}
          className="hb-btn hb-press px-3.5 py-1.5 text-[12px] shrink-0"
          style={{ height: 30 }}
        >
          {testState.phase === 'running' ? 'Testing ...' : 'Test backend'}
        </button>
      </div>

      {data.resolved.env_overrides.length > 0 && (
        <Callout tone="warn" testId="settings-env-overrides" className="mt-3">
          {data.resolved.env_overrides.map(v => <code key={v} className="hb-inline mr-1">${v}</code>)}
          {data.resolved.env_overrides.length === 1 ? ' is' : ' are'} set for this server.
          HAUKSBEE_EXTRACT_CONFIG selects the file edited here; other settings override its values.
        </Callout>
      )}

      {data.load_error && (
        <Callout tone="err" testId="settings-load-error" className="mt-3" title="The saved file could not be read">
          <div>{tildeHome(data.load_error)}</div>
          <div className="mt-1" style={{ color: 'var(--silk-faint)', fontFamily: 'var(--font-mono)' }} title={data.path}>{tildeHome(data.path)}</div>
        </Callout>
      )}

      <div className="mt-2.5 text-[11px]" style={{ color: 'var(--silk-faint)' }}>
        Tests the saved configuration with a one-line prompt. Routing and charges: {availabilityFor(data, data.resolved.backend)?.sends_data_to ?? 'unknown'}.
      </div>

      {testState.phase !== 'idle' && (
        <div className="mt-2">
          <LogWell lines={testState.log} testId="settings-test-log" />
          {testState.phase === 'running' && <BusyLine className="mt-1.5">Running a one-token test call ...</BusyLine>}
          {testState.phase === 'done' && (
            <Callout tone="ok" testId="settings-test-done" live className="mt-1.5 py-2 text-[12px]">
              Backend replied: {testState.reply}
            </Callout>
          )}
          {testState.phase === 'error' && (
            <Callout tone="err" testId="settings-test-error" live className="mt-1.5 py-2 text-[12px] whitespace-pre-wrap">
              {testState.message}
            </Callout>
          )}
        </div>
      )}

      {/* ── Quick setup ── */}
      <div className="mt-8">
        <div className="text-[11px] font-bold tracking-widest uppercase mb-2.5" style={{ color: 'var(--silk-faint)' }}>
          Quick setup
        </div>
        {presetError && (
          <Callout tone="err" testId="settings-preset-error" className="mb-2.5">{presetError}</Callout>
        )}
        <div className="grid grid-cols-1 sm:grid-cols-2 gap-3">
          {data.presets.map(preset => (
            <PresetCard
              key={preset.id}
              preset={preset}
              labels={labelMap(data.options.models[preset.backend])}
              availability={data.backends.find(b => b.backend === preset.backend)}
              customCommand={resolvedBlock(data.resolved, preset.backend).command === data.keys.find(k => k.key === `${preset.backend}.command`)?.default
                ? undefined : resolvedBlock(data.resolved, preset.backend).command}
              active={presetMatchesResolved(preset, data.resolved)}
              busy={presetBusy === preset.id}
              onUse={() => void applyPreset(preset.id)}
            />
          ))}
        </div>
      </div>

      {/* ── Backend ── */}
      <div className="mt-8">
        <div className="text-[11px] font-bold tracking-widest uppercase mb-2.5" style={{ color: 'var(--silk-faint)' }}>
          Backend
        </div>
        <div className="inline-flex flex-wrap rounded-lg overflow-hidden" style={{ border: '1px solid var(--hairline)' }}>
          {BACKEND_IDS.map((id, i) => {
            const avail = availabilityFor(data, id)
            const active = selectedBackend === id
            return (
              <button
                key={id}
                type="button"
                data-testid={`settings-backend-${id}`}
                title={avail?.detail}
                aria-pressed={active}
                onClick={() => { setSelectedBackend(id); setDraft(d => d && { ...d, backend: id }) }}
                className="hb-press px-3 py-1.5 text-[12px] cursor-pointer inline-flex items-center gap-1.5"
                style={{
                  borderLeft: i > 0 ? '1px solid var(--hairline)' : 'none',
                  background: active ? 'var(--copper-tint-strong)' : 'var(--surface-2)',
                  color: active ? 'var(--copper-hi)' : 'var(--silk-dim)',
                  fontWeight: active ? 600 : 400,
                }}
              >
                <span
                  aria-hidden
                  style={{
                    width: 7, height: 7, borderRadius: 4, flexShrink: 0, display: 'inline-block',
                    background: avail?.available ? 'var(--ok)' : 'var(--err)',
                  }}
                />
                {avail?.label ?? BACKEND_FALLBACK_LABEL[id]}
              </button>
            )
          })}
        </div>
        <div className="mt-1.5 text-[11px] leading-relaxed" style={{ color: 'var(--silk-faint)' }}>
          Saved routing (save edits to update): {availability?.sends_data_to ?? 'unknown'}
        </div>
        {availability && !availability.available && (
          <p className="mt-1.5 text-[12px]" data-testid="settings-backend-install">{availability.install}</p>
        )}
      </div>

      {/* ── Per-backend fields ── */}
      <div className="mt-5 rounded-xl px-4 py-3.5" style={{ border: '1px solid var(--hairline)', background: 'var(--surface)' }}>
        {selectedBackend === 'api' ? (
          <div className="flex flex-wrap gap-3.5">
            <Field label="Base URL" hint={`blank = ${data.resolved.api.base_url}`}>
              <MonoInput testId="settings-base-url" width="20rem" value={draft.api.base_url}
                placeholder={data.resolved.api.base_url} onChange={v => setDraft(d => d && { ...d, api: { ...d.api, base_url: v } })} />
            </Field>
            <Field label="Model" hint={`blank = ${data.resolved.api.model}`}>
              <MonoInput testId="settings-model" width="17rem" list="settings-model-options" value={draft.api.model}
                placeholder={data.resolved.api.model} onChange={setApiModel} />
              <ModelPicks picks={data.options.models.api ?? []} current={draft.api.model || data.resolved.api.model}
                hintable={/^gpt-/.test(draft.api.model || data.resolved.api.model)} onPick={setApiModel} />
            </Field>
            <Field
              label="API key variable"
              hint={
                <>
                  The NAME of an environment variable, never the key. It is read when an extraction runs.
                  {' '}{availability?.detail}
                </>
              }
            >
              <MonoInput testId="settings-api-key-env" width="14rem" value={draft.api.api_key_env}
                placeholder={data.resolved.api.api_key_env} onChange={v => setDraft(d => d && { ...d, api: { ...d.api, api_key_env: v } })} />
            </Field>
            <datalist id="settings-model-options">
              {(data.options.models.api ?? []).map(m => <option key={m.name} value={m.name} label={m.note} />)}
            </datalist>
          </div>
        ) : (
          <div className="flex flex-col gap-3.5">
            <div className="flex flex-wrap gap-3.5 items-end">
              <Field label="Model" hint={`blank = ${resolvedForSelected.model}`}>
                <MonoInput testId="settings-model" width="17rem" list="settings-model-options" value={fields.model}
                  placeholder={resolvedForSelected.model} onChange={fields.setModel} />
                <datalist id="settings-model-options">
                  {(data.options.models[selectedBackend] ?? []).map(m => <option key={m.name} value={m.name} label={m.note} />)}
                </datalist>
                <ModelPicks picks={data.options.models[selectedBackend] ?? []}
                  current={fields.model || resolvedForSelected.model} hintable onPick={fields.setModel} />
              </Field>
              <Field label="Effort" hint={`blank = ${resolvedForSelected.effort}`}>
                <Segmented
                  testIdPrefix="settings-effort"
                  options={['', ...(data.options.efforts[selectedBackend] ?? [])]}
                  value={fields.effort}
                  onChange={v => fields.setEffort(v)}
                />
              </Field>
            </div>
            <Field
              label="Escalate to"
              hint={
                <>
                  Models tried in order when a tier fails; comma-separated. Blank = the default
                  ladder, <code className="hb-inline">none</code> = never escalate.{' '}
                  <span data-testid="settings-escalate-effective">
                    In effect: {resolvedEscalate(data.resolved, selectedBackend)?.length
                      ? ladderLabel(resolvedForSelected.model, resolvedEscalate(data.resolved, selectedBackend), labelMap(data.options.models[selectedBackend]))
                      : 'no escalation'}.
                  </span>
                </>
              }
            >
              <MonoInput testId="settings-escalate" width="30rem" value={fields.escalate}
                placeholder={escalateToText(resolvedEscalate(data.resolved, selectedBackend))} onChange={fields.setEscalate} />
            </Field>
          </div>
        )}

        {/* ── Advanced ── */}
        <button
          type="button"
          data-testid="settings-advanced-toggle"
          onClick={() => setAdvancedOpen(v => !v)}
          className="hb-press mt-3.5 inline-flex items-center gap-1 text-[11px] font-semibold cursor-pointer"
          style={{ background: 'none', border: 'none', padding: 0, color: 'var(--silk-faint)' }}
          aria-expanded={advancedOpen}
        >
          {advancedOpen ? <ChevronDownIcon size={12} /> : <ChevronRightIcon size={12} />}
          Advanced
        </button>

        {advancedOpen && (
          <div className="mt-3 flex flex-wrap gap-3.5 items-end">
            {selectedBackend !== 'api' && (
              <Field label="Executable" hint="Optional CLI-compatible wrapper. Its configuration controls routing and charges.">
                <MonoInput testId="settings-command" width="20rem" value={fields.command}
                  placeholder="Default executable" onChange={fields.setCommand} />
              </Field>
            )}
            {selectedBackend === 'codex' && (
              <Field label="Profile" hint="A named codex profile, if you use one.">
                <MonoInput testId="settings-profile" width="14rem" value={fields.profile}
                  placeholder={data.resolved.codex.profile ?? ''} onChange={fields.setProfile} />
              </Field>
            )}
            {selectedBackend !== 'api' && (
              <Field label="Permission mode" hint={`blank = ${fields.permission || resolvedPermission(data.resolved, selectedBackend)}`}>
                <select
                  data-testid="settings-permission-mode"
                  className="hb-input text-[12px] block w-full"
                  style={{ height: 30, maxWidth: '14rem' }}
                  value={fields.permission}
                  onChange={e => fields.setPermission(e.target.value)}
                >
                  <option value="">default</option>
                  {(data.options.permission_modes[selectedBackend] ?? []).map(m => <option key={m} value={m}>{m}</option>)}
                </select>
              </Field>
            )}
            {selectedBackend !== 'api' && (
              <Field label="Extra arguments" hint="Space-separated, passed through to the backend's own CLI.">
                <MonoInput testId="settings-extra-args" width="20rem" value={fields.extraArgs}
                  onChange={fields.setExtraArgs} />
              </Field>
            )}
            <Field label="Retries" hint={`blank = ${data.resolved.retries}`}>
              <input
                data-testid="settings-retries"
                type="number"
                min={0}
                max={10}
                className="hb-input text-[12px] block w-full tnum"
                style={{ height: 30, maxWidth: '6rem' }}
                value={draft.retries}
                placeholder={String(data.resolved.retries)}
                onChange={e => setDraft(d => d && { ...d, retries: e.target.value })}
              />
            </Field>
            <Field label="Timeout (seconds)" hint={`blank = ${data.resolved.timeout_secs}`}>
              <input
                data-testid="settings-timeout"
                type="number"
                min={30}
                max={7200}
                className="hb-input text-[12px] block w-full tnum"
                style={{ height: 30, maxWidth: '7rem' }}
                value={draft.timeout_secs}
                placeholder={String(data.resolved.timeout_secs)}
                onChange={e => setDraft(d => d && { ...d, timeout_secs: e.target.value })}
              />
            </Field>
          </div>
        )}
      </div>

      {/* ── Actions ── */}
      <div className="mt-4 flex flex-wrap items-center gap-2.5">
        <button
          type="button"
          data-testid="settings-save"
          disabled={!dirty || saveState.phase === 'saving'}
          onClick={() => void save()}
          className="hb-btn-primary hb-press px-3.5 py-1.5 text-[12px]"
          style={{ height: 30 }}
        >
          {saveState.phase === 'saving' ? 'Saving ...' : 'Save'}
        </button>
        <button
          type="button"
          data-testid="settings-discard"
          disabled={!dirty}
          onClick={discard}
          className="hb-btn hb-press px-3 py-1.5 text-[12px]"
          style={{ height: 30 }}
        >
          Discard changes
        </button>
        <span className="flex-1" />
        <span className="text-[11px] truncate max-w-full" style={{ color: 'var(--silk-faint)', fontFamily: 'var(--font-mono)' }} title={data.path}>
          {tildeHome(data.path)}
        </span>
      </div>

      {saveState.phase === 'saved' && (
        <Callout tone="ok" testId="settings-saved" live className="mt-2.5 py-2 text-[12px]">
          Saved to <span style={{ fontFamily: 'var(--font-mono)' }} title={saveState.path}>{tildeHome(saveState.path)}</span>
        </Callout>
      )}
      {saveState.phase === 'error' && (
        <Callout tone="err" testId="settings-save-error" live className="mt-2.5 py-2 text-[12px] whitespace-pre-wrap">
          {saveState.message}
        </Callout>
      )}

      <div className="mt-3 text-[11px]" style={{ color: 'var(--silk-faint)' }} data-testid="settings-terminal">
        <TerminalCommand command={terminalCommand} testId="settings-terminal-copy" />
      </div>
    </div>
  )
}

function resolvedPermission(resolved: ExtractResolved, backend: Exclude<BackendId, 'api'>): string {
  if (backend === 'claude-code') return resolved.claude_code.permission_mode ?? ''
  return resolved[backend].permission_mode ?? ''
}
