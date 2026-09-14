import { useCallback, useEffect, useMemo, useState } from 'react'
import { api, errorText } from '../lib/api'
import type {
  ExtractApiBackendConfig, ExtractBackendAvailability, ExtractBackendsConfig, ExtractClaudeCodeBackendConfig,
  ExtractCodexBackendConfig, ExtractPreset, ExtractResolved, ExtractSettings,
} from '../types/report'
import { BusyLine, Callout, LogWell, TerminalCommand } from './ui'
import { ChevronDownIcon, ChevronRightIcon } from './Icons'

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

interface DraftLikeBackend {
  model: string
  effort: string
  permission_mode: string
  extra_args: string
}

interface DraftCodex {
  model: string
  effort: string
  profile: string
  extra_args: string
}

interface DraftApi {
  base_url: string
  model: string
  api_key_env: string
}

interface Draft {
  backend: string
  retries: string
  timeout_secs: string
  codex: DraftCodex
  'claude-code': DraftLikeBackend
  agy: DraftLikeBackend
  api: DraftApi
}

const draftFromConfig = (config: ExtractBackendsConfig): Draft => ({
  backend: config.backend ?? '',
  retries: config.retries !== undefined ? String(config.retries) : '',
  timeout_secs: config.timeout_secs !== undefined ? String(config.timeout_secs) : '',
  codex: {
    model: config.codex?.model ?? '',
    effort: config.codex?.effort ?? '',
    profile: config.codex?.profile ?? '',
    extra_args: (config.codex?.extra_args ?? []).join(' '),
  },
  'claude-code': {
    model: config['claude-code']?.model ?? '',
    effort: config['claude-code']?.effort ?? '',
    permission_mode: config['claude-code']?.permission_mode ?? '',
    extra_args: (config['claude-code']?.extra_args ?? []).join(' '),
  },
  agy: {
    model: config.agy?.model ?? '',
    effort: config.agy?.effort ?? '',
    permission_mode: config.agy?.permission_mode ?? '',
    extra_args: (config.agy?.extra_args ?? []).join(' '),
  },
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

  const codex: ExtractCodexBackendConfig = {}
  if (draft.codex.model.trim()) codex.model = draft.codex.model.trim()
  if (draft.codex.effort.trim()) codex.effort = draft.codex.effort.trim()
  if (draft.codex.profile.trim()) codex.profile = draft.codex.profile.trim()
  const codexArgs = draft.codex.extra_args.trim().split(/\s+/).filter(Boolean)
  if (codexArgs.length) codex.extra_args = codexArgs
  if (Object.keys(codex).length) config.codex = codex

  const claude: ExtractClaudeCodeBackendConfig = {}
  if (draft['claude-code'].model.trim()) claude.model = draft['claude-code'].model.trim()
  if (draft['claude-code'].effort.trim()) claude.effort = draft['claude-code'].effort.trim()
  if (draft['claude-code'].permission_mode.trim()) claude.permission_mode = draft['claude-code'].permission_mode.trim()
  const claudeArgs = draft['claude-code'].extra_args.trim().split(/\s+/).filter(Boolean)
  if (claudeArgs.length) claude.extra_args = claudeArgs
  if (Object.keys(claude).length) config['claude-code'] = claude

  const agy: ExtractClaudeCodeBackendConfig = {}
  if (draft.agy.model.trim()) agy.model = draft.agy.model.trim()
  if (draft.agy.effort.trim()) agy.effort = draft.agy.effort.trim()
  if (draft.agy.permission_mode.trim()) agy.permission_mode = draft.agy.permission_mode.trim()
  const agyArgs = draft.agy.extra_args.trim().split(/\s+/).filter(Boolean)
  if (agyArgs.length) agy.extra_args = agyArgs
  if (Object.keys(agy).length) config.agy = agy

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
function draftArgs(config: ExtractBackendsConfig): string[] {
  const args: string[] = []
  if (config.backend) args.push(`backend=${config.backend}`)
  if (config.retries !== undefined) args.push(`retries=${config.retries}`)
  if (config.timeout_secs !== undefined) args.push(`timeout_secs=${config.timeout_secs}`)
  const block = (prefix: string, obj?: Record<string, unknown>) => {
    if (!obj) return
    for (const [k, v] of Object.entries(obj)) {
      if (v === undefined || v === null || v === '') continue
      if (Array.isArray(v)) { if (v.length) args.push(`${prefix}.${k}=${v.join(' ')}`) }
      else args.push(`${prefix}.${k}=${v}`)
    }
  }
  block('codex', config.codex as Record<string, unknown> | undefined)
  block('claude-code', config['claude-code'] as Record<string, unknown> | undefined)
  block('agy', config.agy as Record<string, unknown> | undefined)
  block('api', config.api as Record<string, unknown> | undefined)
  return args
}

function resolvedBlock(resolved: ExtractResolved, backend: string) {
  switch (backend) {
    case 'codex': return { model: resolved.codex.model, effort: resolved.codex.effort }
    case 'claude-code': return { model: resolved.claude_code.model, effort: resolved.claude_code.effort }
    case 'agy': return { model: resolved.agy.model, effort: resolved.agy.effort }
    case 'api': return { model: resolved.api.model, effort: '' }
    default: return { model: '', effort: '' }
  }
}

function presetMatchesResolved(preset: ExtractPreset, resolved: ExtractResolved): boolean {
  if (preset.backend !== resolved.backend) return false
  if (preset.backend === 'api') return preset.model === resolved.api.model && preset.api_base === resolved.api.base_url
  const { model, effort } = resolvedBlock(resolved, preset.backend)
  return preset.model === model && preset.effort === effort
}

/** Which preset (if any) the CURRENT draft is exactly equivalent to, for the
 *  terminal line's `backend use <preset>` shortcut. */
function draftMatchesPreset(draft: Draft, selectedBackend: string, presets: ExtractPreset[]): ExtractPreset | null {
  for (const preset of presets) {
    if (preset.backend !== selectedBackend) continue
    if (preset.backend === 'api') {
      if (draft.api.model === preset.model && draft.api.base_url === preset.api_base
        && draft.api.api_key_env === preset.api_key_env) return preset
      continue
    }
    const block = preset.backend === 'codex' ? draft.codex
      : preset.backend === 'claude-code' ? draft['claude-code']
      : preset.backend === 'agy' ? draft.agy : null
    if (block && block.model === preset.model && block.effort === preset.effort) return preset
  }
  return null
}

function sourceLine(data: ExtractSettings): string {
  const src = data.resolved.backend_source
  if (src.kind === 'env') return `overridden by $${src.name} for this server process`
  if (src.kind === 'flag') return 'set by a command-line flag for this server process'
  if (src.kind === 'auto') return 'auto-detected, nothing saved yet'
  return data.exists ? `from ${data.path}` : `from ${data.path} (nothing saved yet)`
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

function PresetCard({ preset, availability, active, busy, onUse }: {
  preset: ExtractPreset
  availability?: ExtractBackendAvailability
  active: boolean
  busy: boolean
  onUse: () => void
}) {
  const monoLine = preset.backend === 'api'
    ? [preset.api_base, preset.model].filter(Boolean).join(' · ')
    : `${preset.backend} · ${preset.model}${preset.effort ? ` · ${preset.effort} effort` : ''}`
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
        <span className="text-[13px] font-semibold" style={{ color: 'var(--silk)' }}>{preset.label}</span>
        {preset.recommended && availability?.available && (
          <span className="text-[9px] font-bold tracking-widest uppercase shrink-0 whitespace-nowrap" style={{ color: 'var(--copper-hi)' }}>
            Recommended
          </span>
        )}
      </div>
      <div className="text-[12px] leading-relaxed" style={{ color: 'var(--silk-dim)' }}>{preset.summary}</div>
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

  const matchedPreset = data && draft ? draftMatchesPreset(draft, selectedBackend, data.presets) : null
  const terminalCommand = data && draft
    ? (matchedPreset
      ? `hauksbee models backend use ${matchedPreset.id}`
      : (() => {
          const args = draftArgs(configFromDraft(draft))
          return args.length ? `hauksbee models backend set ${args.join(' ')}` : 'hauksbee models backend show'
        })())
    : ''

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
  // Computed once per render, rather than indexed dynamically inside JSX
  // closures: `draft[selectedBackend]` narrows fine in a direct expression,
  // but not reliably once captured by a nested onChange callback.
  const fields = selectedBackend === 'codex'
    ? {
        model: draft.codex.model, effort: draft.codex.effort, extraArgs: draft.codex.extra_args,
        permission: '', profile: draft.codex.profile,
        setModel: (v: string) => setDraft(d => d && { ...d, codex: { ...d.codex, model: v } }),
        setEffort: (v: string) => setDraft(d => d && { ...d, codex: { ...d.codex, effort: v } }),
        setExtraArgs: (v: string) => setDraft(d => d && { ...d, codex: { ...d.codex, extra_args: v } }),
        setPermission: (_v: string) => {},
        setProfile: (v: string) => setDraft(d => d && { ...d, codex: { ...d.codex, profile: v } }),
      }
    : selectedBackend === 'claude-code'
    ? {
        model: draft['claude-code'].model, effort: draft['claude-code'].effort, extraArgs: draft['claude-code'].extra_args,
        permission: draft['claude-code'].permission_mode, profile: '',
        setModel: (v: string) => setDraft(d => d && { ...d, 'claude-code': { ...d['claude-code'], model: v } }),
        setEffort: (v: string) => setDraft(d => d && { ...d, 'claude-code': { ...d['claude-code'], effort: v } }),
        setExtraArgs: (v: string) => setDraft(d => d && { ...d, 'claude-code': { ...d['claude-code'], extra_args: v } }),
        setPermission: (v: string) => setDraft(d => d && { ...d, 'claude-code': { ...d['claude-code'], permission_mode: v } }),
        setProfile: (_v: string) => {},
      }
    : {
        // 'agy' (and a never-rendered fallback for 'api', whose fields render
        // via a separate branch below and never read this object).
        model: draft.agy.model, effort: draft.agy.effort, extraArgs: draft.agy.extra_args,
        permission: draft.agy.permission_mode, profile: '',
        setModel: (v: string) => setDraft(d => d && { ...d, agy: { ...d.agy, model: v } }),
        setEffort: (v: string) => setDraft(d => d && { ...d, agy: { ...d.agy, effort: v } }),
        setExtraArgs: (v: string) => setDraft(d => d && { ...d, agy: { ...d.agy, extra_args: v } }),
        setPermission: (v: string) => setDraft(d => d && { ...d, agy: { ...d.agy, permission_mode: v } }),
        setProfile: (_v: string) => {},
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
          <div className="mt-1 text-[11px]" style={{ color: 'var(--silk-faint)' }}>{sourceLine(data)}</div>
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
          {data.resolved.env_overrides.length === 1 ? ' is' : ' are'} set in this server process's environment and
          override the saved file for as long as it keeps running.
        </Callout>
      )}

      {data.load_error && (
        <Callout tone="err" testId="settings-load-error" className="mt-3" title="The saved file could not be read">
          <div>{data.load_error}</div>
          <div className="mt-1" style={{ color: 'var(--silk-faint)', fontFamily: 'var(--font-mono)' }}>{data.path}</div>
        </Callout>
      )}

      <div className="mt-2.5 text-[11px]" style={{ color: 'var(--silk-faint)' }}>
        Sends a one-line prompt to {availabilityFor(data, data.resolved.backend)?.label ?? data.resolved.backend}; nothing else leaves this machine.
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
              availability={data.backends.find(b => b.backend === preset.backend)}
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
          Sends the datasheet's text to: {availability?.sends_data_to ?? 'unknown'}
        </div>
        {availability && !availability.available && (
          <div className="mt-1.5">
            <TerminalCommand command={availability.install} testId="settings-backend-install" />
          </div>
        )}
      </div>

      {/* ── Per-backend fields ── */}
      <div className="mt-5 rounded-xl px-4 py-3.5" style={{ border: '1px solid var(--hairline)', background: 'var(--surface)' }}>
        {selectedBackend === 'api' ? (
          <div className="flex flex-wrap gap-3.5">
            <Field label="Base URL" hint={`blank = ${data.resolved.api.base_url}`}>
              <input
                data-testid="settings-base-url"
                className="hb-input text-[12px] block w-full"
                style={{ height: 30, maxWidth: '20rem', fontFamily: 'var(--font-mono)' }}
                value={draft.api.base_url}
                placeholder={data.resolved.api.base_url}
                onChange={e => setDraft(d => d && { ...d, api: { ...d.api, base_url: e.target.value } })}
              />
            </Field>
            <Field label="Model" hint={`blank = ${data.resolved.api.model}`}>
              <input
                data-testid="settings-model"
                list="settings-model-options"
                className="hb-input text-[12px] block w-full"
                style={{ height: 30, maxWidth: '17rem', fontFamily: 'var(--font-mono)' }}
                value={draft.api.model}
                placeholder={data.resolved.api.model}
                onChange={e => setDraft(d => d && { ...d, api: { ...d.api, model: e.target.value } })}
              />
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
              <input
                data-testid="settings-api-key-env"
                className="hb-input text-[12px] block w-full"
                style={{ height: 30, maxWidth: '14rem', fontFamily: 'var(--font-mono)' }}
                value={draft.api.api_key_env}
                placeholder={data.resolved.api.api_key_env}
                onChange={e => setDraft(d => d && { ...d, api: { ...d.api, api_key_env: e.target.value } })}
              />
            </Field>
            <datalist id="settings-model-options">
              {(data.options.models.api ?? []).map(m => <option key={m} value={m} />)}
            </datalist>
          </div>
        ) : (
          <div className="flex flex-col gap-3.5">
            <div className="flex flex-wrap gap-3.5 items-end">
              <Field label="Model" hint={`blank = ${resolvedForSelected.model}`}>
                <input
                  data-testid="settings-model"
                  list="settings-model-options"
                  className="hb-input text-[12px] block w-full"
                  style={{ height: 30, maxWidth: '17rem', fontFamily: 'var(--font-mono)' }}
                  value={fields.model}
                  placeholder={resolvedForSelected.model}
                  onChange={e => fields.setModel(e.target.value)}
                />
                <datalist id="settings-model-options">
                  {(data.options.models[selectedBackend] ?? []).map(m => <option key={m} value={m} />)}
                </datalist>
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
            {selectedBackend === 'codex' && (
              <Field label="Profile" hint="A named codex profile, if you use one.">
                <input
                  data-testid="settings-profile"
                  className="hb-input text-[12px] block w-full"
                  style={{ height: 30, maxWidth: '14rem', fontFamily: 'var(--font-mono)' }}
                  value={fields.profile}
                  placeholder={data.resolved.codex.profile ?? ''}
                  onChange={e => fields.setProfile(e.target.value)}
                />
              </Field>
            )}
            {(selectedBackend === 'claude-code' || selectedBackend === 'agy') && (
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
                <input
                  data-testid="settings-extra-args"
                  className="hb-input text-[12px] block w-full"
                  style={{ height: 30, maxWidth: '20rem', fontFamily: 'var(--font-mono)' }}
                  value={fields.extraArgs}
                  onChange={e => fields.setExtraArgs(e.target.value)}
                />
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
          {data.path}
        </span>
      </div>

      {saveState.phase === 'saved' && (
        <Callout tone="ok" testId="settings-saved" live className="mt-2.5 py-2 text-[12px]">
          Saved to <span style={{ fontFamily: 'var(--font-mono)' }}>{saveState.path}</span>
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

function resolvedPermission(resolved: ExtractResolved, backend: 'claude-code' | 'agy'): string {
  return backend === 'claude-code' ? resolved.claude_code.permission_mode : resolved.agy.permission_mode
}
