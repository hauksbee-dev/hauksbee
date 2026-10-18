import { useCallback, useEffect, useMemo, useState } from 'react'
import { api, errorText } from '../lib/api'
import { tildeHome } from '../lib/display-path'
import type {
  ExtractAgentConfig, ExtractApiBackendConfig, ExtractBackendAvailability, ExtractBackendsConfig,
  ExtractPreset, ExtractResolved, ModelChoice, ExtractResolvedApi, ExtractSettings, DepInfo,
} from '../types/report'
import { BusyLine, Callout, LogWell, TerminalCommand } from './ui'
import { useDeps } from '../hooks/useDeps'
import { InstallButton, InstallProgress } from './DepInstall'
import { LinkedText } from './LinkedText'
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

// Backend ids and display names come from the server (`data.backends`); only
// the keys of the saved config block per agent CLI are named here.
type AgentBackendId = 'codex' | 'claude-code' | 'agy'

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

/** A model name is compared with the pick list only when the server's prefix
 *  rule for the backend says so (an OpenAI-compatible endpoint may serve any model). */
function hintsApply(data: ExtractSettings, backend: string, model: string): boolean {
  const prefix = data.options.model_hint_prefix?.[backend]
  return !prefix || model.startsWith(prefix)
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

/** The server's model notes lead with "Recommended: ..." on each backend's
 *  default. The page recommends one setup (in the choice list), so a model's note
 *  says what it is, not that it is recommended. */
const modelNote = (note: string): string => {
  const plain = note.replace(/^Recommended:\s*/i, '')
  return plain.charAt(0).toUpperCase() + plain.slice(1)
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
  const defaultPick = picks.find(p => p.default) ?? picks[0]
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
          {p.default && <span style={{ color: 'var(--silk)' }}> (default)</span>}
          {p.note && <span> &mdash; {modelNote(p.note)}</span>}
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
          {current} is not in the current list and is used as written; {defaultPick.label ?? defaultPick.name} is the default.
        </span>
      )}
    </div>
  )
}

/** What the escalation setting does, in one sentence that always agrees with the
 *  option that is selected. `defaultLadder` is the ladder the backend uses when the
 *  setting is blank, or null when it cannot be known (a saved ladder hides it). */
export function escalationSummary(input: {
  value: string
  firstModel: string
  defaultLadder: readonly string[] | null
  labels?: Readonly<Record<string, string>>
}): string {
  const { value, firstModel, defaultLadder, labels } = input
  const ids = escalateFromText(value)
  const retry = (chain: readonly string[]) =>
    `${ladderLabel(firstModel, chain, labels)}. Each step runs only when a draft from the step before it fails or doubts its pin map.`
  if (ids !== undefined && ids.length === 0) {
    return 'No escalation: if a draft fails it is reported as failed, not retried on another model.'
  }
  if (ids !== undefined) return `Ladder: ${retry(ids)}`
  if (defaultLadder === null) return 'Default: the built-in ladder for this tool.'
  if (defaultLadder.length === 0) {
    return 'Default: no escalation. Pick models below to retry on a stronger one when a draft fails.'
  }
  return `Default ladder: ${retry(defaultLadder)}`
}

/** The escalation ladder as picks: the same one-click shape as the model picker.
 *  Order is the order picked (shown as 1, 2, ...). "Default ladder" leaves the
 *  setting blank; "Never escalate" saves it empty. The text spelling the CLI
 *  takes (`none`, comma-separated) is the stored value and the terminal line. */
function EscalatePicks({ picks, firstModel, value, effective, defaultLabel, onChange }: {
  picks: ModelChoice[]
  /** The name of the "leave it to the default" option, which says what the default is. */
  defaultLabel: string
  /** The model tried first; it is not an escalation target. */
  firstModel: string
  value: string
  /** The ladder in effect when the setting is blank. */
  effective: readonly string[]
  onChange: (text: string) => void
}) {
  const ids = escalateFromText(value)
  const mode = ids === undefined ? 'default' : ids.length === 0 ? 'none' : 'custom'
  const chosen = ids ?? []
  const names = picks.filter(p => p.name !== firstModel).map(p => p.name)
  // Anything saved that the list does not know stays visible and removable.
  const options = [...names, ...chosen.filter(id => !names.includes(id) && id !== firstModel)]
  const labels = labelMap(picks)
  const toggle = (name: string) => {
    const from = ids ?? [...effective]
    const next = from.includes(name) ? from.filter(x => x !== name) : [...from, name]
    onChange(next.length ? next.join(', ') : 'none')
  }
  const pill = (pressed: boolean): React.CSSProperties => ({
    border: '1px solid var(--hairline)',
    background: pressed ? 'var(--copper-tint-strong)' : 'var(--surface-2)',
    color: pressed ? 'var(--copper-hi)' : 'var(--silk-dim)',
    fontWeight: pressed ? 600 : 400,
  })
  return (
    <div className="mt-1 flex flex-wrap gap-1.5" data-testid="settings-escalate">
      <button type="button" data-testid="settings-escalate-default" aria-pressed={mode === 'default'}
        onClick={() => onChange('')} className="hb-press text-[11px] cursor-pointer rounded-md px-2 py-1" style={pill(mode === 'default')}>
        {defaultLabel}
      </button>
      <button type="button" data-testid="settings-escalate-none" aria-pressed={mode === 'none'}
        onClick={() => onChange('none')} className="hb-press text-[11px] cursor-pointer rounded-md px-2 py-1" style={pill(mode === 'none')}>
        No escalation
      </button>
      {options.map(name => {
        const at = chosen.indexOf(name)
        return (
          <button key={name} type="button" data-testid={`settings-escalate-pick-${name}`} aria-pressed={at >= 0}
            onClick={() => toggle(name)} className="hb-press text-[11px] cursor-pointer rounded-md px-2 py-1" style={pill(at >= 0)}>
            {at >= 0 && <span className="tnum mr-1">{at + 1}.</span>}
            {labels[name] ?? name}
          </button>
        )
      })}
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

/** The sentences of a preset's summary that help someone choose. The server's
 *  wording also carries benchmark results and baseline names; those belong in the
 *  documentation, not in a choice list. */
function plainSummary(summary: string): string {
  const kept = summary.split(/(?<=[.!?])\s+/).filter(sentence => !/benchmark|baseline/i.test(sentence))
  return (kept.length ? kept : [summary]).join(' ')
}

/** Why a backend cannot be used yet and the one thing that fixes it: an Install
 *  button where the server can install it, the exact command where it cannot,
 *  the sign-in command when it is installed but signed out. */
function BackendProblem({ availability, dep, signedOut, depsApi }: {
  availability: ExtractBackendAvailability
  dep?: DepInfo
  signedOut: boolean
  depsApi: ReturnType<typeof useDeps>
}) {
  return (
    <div className="flex flex-col gap-1.5" data-testid="settings-backend-problem">
      <div className="flex flex-wrap items-start gap-3">
        <p className="text-[12px] flex-1 min-w-0 leading-relaxed" data-testid="settings-backend-install" style={{ color: 'var(--warn-strong)' }}>
          <LinkedText text={signedOut ? 'Installed, but not signed in. Sign in once with the command below.' : availability.install} />
        </p>
        {dep && !dep.present && dep.installable && (
          <InstallButton dep={dep} install={depsApi.install} onInstall={id => void depsApi.runInstall(id)} variant="secondary" />
        )}
      </div>
      {dep && !dep.present && !dep.installable && dep.cost && (
        <div className="text-[11px]" style={{ color: 'var(--silk-dim)' }}>
          <span style={{ color: 'var(--silk-faint)' }}>Cost: </span>{dep.cost}
        </div>
      )}
      {dep?.manual && (!dep.present || signedOut) && (
        <div className="text-[11px]" style={{ color: 'var(--silk-dim)' }}>
          <TerminalCommand command={dep.manual} testId="settings-backend-copy" />
        </div>
      )}
      <InstallProgress id={availability.backend} install={depsApi.install} />
    </div>
  )
}

/** "installed" / "not installed" / "key set" / "no key set" / "not signed in". */
function backendState(b: ExtractBackendAvailability | undefined, signedOut: boolean): string {
  if (b?.backend === 'api') return b.available ? 'key set' : 'no key set'
  if (signedOut) return 'installed · not signed in'
  return b?.available ? 'installed' : 'not installed'
}

/** One way of reading datasheets, as one row: what it is, what it costs, whether
 *  it works on this machine, and the single button that switches to it. */
function PresetRow({ preset, tag, ladder, availability, dep, depsApi, showProblem, customCommand, active, busy, onUse }: {
  preset: ExtractPreset
  /** "Haiku 5.5 → Sonnet 5.5": the models tried in order, when the server names them all. */
  ladder?: string
  /** "Recommended" / "Cheaper option": at most one row carries each. */
  tag?: string
  customCommand?: string
  availability?: ExtractBackendAvailability
  dep?: DepInfo
  depsApi: ReturnType<typeof useDeps>
  showProblem: boolean
  active: boolean
  busy: boolean
  onUse: () => void
}) {
  const signedOut = !!dep && dep.present && dep.sign_in === 'not_signed_in'
  const problem = !!availability && (!availability.available || signedOut)
  const good = !!availability?.available && !signedOut
  return (
    <div
      data-testid={`settings-preset-${preset.id}`}
      className="rounded-xl px-3.5 py-3 flex flex-col gap-1.5"
      style={{
        border: `1px solid ${active ? 'var(--copper-deep)' : 'var(--hairline)'}`,
        background: 'var(--surface)',
      }}
    >
      <div className="flex items-start justify-between gap-3">
        <div className="min-w-0 flex flex-col gap-1">
          <span className="text-[13px] font-semibold" style={{ color: 'var(--silk)' }}>
            {preset.label}
            {tag && (
              <span data-testid={`settings-preset-${preset.id}-tag`} className="ml-2 text-[9px] font-bold tracking-widest uppercase whitespace-nowrap" style={{ color: 'var(--copper-hi)' }}>
                {tag}
              </span>
            )}
            {preset.badge && (
              <span data-testid={`settings-preset-${preset.id}-badge`} className="ml-2 text-[10px] font-bold tracking-widest uppercase" style={{ color: 'var(--warn, var(--copper-hi))' }}>
                {preset.badge}
              </span>
            )}
          </span>
          <span className="text-[12px] leading-relaxed" style={{ color: 'var(--silk-dim)' }}>
            {customCommand ? `Keeps custom command ${customCommand}; its configuration controls routing and charges.` : plainSummary(preset.summary)}
          </span>
          {ladder && (
            <span className="text-[11px]" data-testid={`settings-preset-${preset.id}-ladder`} style={{ color: 'var(--silk-faint)' }}>
              Tries in order: {ladder}
            </span>
          )}
          <span
            className="text-[10px] font-bold tracking-widest uppercase self-start px-1.5 py-0.5 rounded"
            data-testid={`settings-preset-${preset.id}-state`}
            style={{
              color: good ? 'var(--ok)' : signedOut ? 'var(--warn-strong)' : 'var(--silk-faint)',
              background: good ? 'var(--ok-bg)' : 'var(--surface-2)',
              border: `1px solid ${good ? 'var(--ok-border)' : 'var(--hairline)'}`,
            }}
          >
            {backendState(availability, signedOut)}
          </span>
        </div>
        <button
          type="button"
          data-testid={`settings-preset-${preset.id}-use`}
          disabled={active || busy}
          onClick={onUse}
          className="hb-btn hb-press px-3 py-1 text-[12px] shrink-0"
          style={{ height: 28 }}
        >
          {active ? 'Active' : busy ? 'Applying ...' : 'Use'}
        </button>
      </div>
      {problem && showProblem && availability && (
        <BackendProblem availability={availability} dep={dep} signedOut={signedOut} depsApi={depsApi} />
      )}
    </div>
  )
}

/** One row per backend the server lists: pick it, see whether it works, fix it
 *  right there. The problem and its action appear on the selected row only. */
function BackendRows({ backends, selected, onSelect, deps, depsApi }: {
  backends: ExtractBackendAvailability[]
  selected: string
  onSelect: (backend: string) => void
  deps: DepInfo[]
  depsApi: ReturnType<typeof useDeps>
}) {
  return (
    <div className="rounded-xl overflow-hidden" style={{ border: '1px solid var(--hairline)' }} data-testid="settings-backends">
      {backends.map((b, i) => {
        const active = selected === b.backend
        const dep = deps.find(d => d.id === b.backend)
        const signedOut = !!dep && dep.present && dep.sign_in === 'not_signed_in'
        const problem = !b.available || signedOut
        return (
          <div key={b.backend} style={{ borderTop: i > 0 ? '1px solid var(--hairline)' : 'none', background: active ? 'var(--copper-tint)' : 'var(--surface)' }}>
            <button
              type="button"
              data-testid={`settings-backend-${b.backend}`}
              title={b.detail}
              aria-pressed={active}
              onClick={() => onSelect(b.backend)}
              className="hb-press w-full text-left px-3.5 py-2.5 text-[12px] cursor-pointer flex flex-wrap items-center gap-x-2.5 gap-y-1"
              style={{ background: 'none', border: 'none', color: active ? 'var(--copper-hi)' : 'var(--silk-dim)', fontWeight: active ? 600 : 400 }}
            >
              <span aria-hidden style={{ width: 7, height: 7, borderRadius: 4, flexShrink: 0, display: 'inline-block', background: b.available ? (signedOut ? 'var(--warn)' : 'var(--ok)') : 'var(--err)' }} />
              <span className="min-w-0">{b.label}</span>
              <span className="flex-1" />
              <span className="text-[10px] font-bold tracking-widest uppercase" data-testid={`settings-backend-state-${b.backend}`} style={{ color: !b.available ? 'var(--silk-faint)' : signedOut ? 'var(--warn-strong)' : 'var(--ok)' }}>
                {backendState(b, signedOut)}
              </span>
            </button>
            {active && problem && (
              <div className="px-3.5 pb-3">
                <BackendProblem availability={b} dep={dep} signedOut={signedOut} depsApi={depsApi} />
              </div>
            )}
          </div>
        )
      })}
    </div>
  )
}

/** A disclosure heading that is a real button (keyboard and screen reader). */
function Disclosure({ open, onToggle, testId, children }: {
  open: boolean
  onToggle: () => void
  testId: string
  children: React.ReactNode
}) {
  return (
    <button
      type="button"
      data-testid={testId}
      onClick={onToggle}
      aria-expanded={open}
      className="hb-press inline-flex items-center gap-1 text-[12px] font-semibold cursor-pointer"
      style={{ background: 'none', border: 'none', padding: 0, color: 'var(--silk-dim)' }}
    >
      {open ? <ChevronDownIcon size={12} /> : <ChevronRightIcon size={12} />}
      {children}
    </button>
  )
}

/** The setup the page names, and the cheaper way to use the same tool. Both are
 *  the engine's call: `default_preset` is the preset a fresh install is steered
 *  to (the rule `hauksbee models backend setup` uses) and a preset's `cheaper`
 *  flag marks the cheaper option for its tool. An older server sends neither, so
 *  the first preset it marks `recommended` stands in and no cheaper one is shown. */
export function featuredPresets(data: ExtractSettings): { recommended?: ExtractPreset; cheaper?: ExtractPreset } {
  const recommended = data.presets.find(p => p.id === data.default_preset) ?? data.presets.find(p => p.recommended)
  const cheaper = recommended ? data.presets.find(p => p.cheaper && p.backend === recommended.backend) : undefined
  return { recommended, cheaper }
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
  const [selectedBackend, setSelectedBackend] = useState<string>('claude-code')
  const [advancedOpen, setAdvancedOpen] = useState(false)
  const [changeOpen, setChangeOpen] = useState(false)
  const [moreOpen, setMoreOpen] = useState(false)
  const [customOpen, setCustomOpen] = useState(false)
  const [presetBusy, setPresetBusy] = useState<string | null>(null)
  const [presetError, setPresetError] = useState<string | null>(null)
  const [saveState, setSaveState] = useState<SaveState>({ phase: 'idle' })
  const [testState, setTestState] = useState<TestState>({ phase: 'idle' })

  // A finished install changes which backends work: re-read the availability
  // without touching the draft being edited.
  const refreshAvailability = useCallback(() => {
    void api.extractSettings().then(fresh => setFetchState({ phase: 'ready', data: fresh })).catch(() => undefined)
  }, [])
  const depsApi = useDeps(refreshAvailability)
  const deps = depsApi.fetchState.phase === 'ready' ? depsApi.fetchState.deps : []

  const load = useCallback(async () => {
    setFetchState({ phase: 'loading' })
    try {
      const data = await api.extractSettings()
      setFetchState({ phase: 'ready', data })
      setDraft(draftFromConfig(data.config))
      setSelectedBackend(data.config.backend || data.resolved.backend || 'claude-code')
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
      setSelectedBackend(fresh.config.backend || fresh.resolved.backend || 'claude-code')
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
  const agentBackend: AgentBackendId = selectedBackend === 'api' ? 'agy' : (selectedBackend as AgentBackendId)
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

  const { recommended, cheaper } = featuredPresets(data)
  const featured = [recommended, cheaper].filter((p): p is ExtractPreset => !!p)
  const others = data.presets.filter(p => !featured.includes(p))
  const currentAvailability = availabilityFor(data, data.resolved.backend)
  const routing = currentAvailability?.sends_data_to
  // The tool in use may itself be missing or signed out: say so on the line that names it.
  const currentDep = deps.find(d => d.id === data.resolved.backend)
  const currentSignedOut = !!currentDep && currentDep.present && currentDep.sign_in === 'not_signed_in'
  const currentProblem = !!currentAvailability && (!currentAvailability.available || currentSignedOut)

  const renderPreset = (preset: ExtractPreset, tag?: string) => {
    const dep = deps.find(d => d.id === preset.backend)
    const customCommand = resolvedBlock(data.resolved, preset.backend).command
    const names = labelMap(data.options.models[preset.backend])
    const chain = [preset.model, ...(preset.escalate ?? [])]
    const ladder = preset.escalate?.length && chain.every(id => names[id]) ? ladderLabel(preset.model, preset.escalate, names) : undefined
    const builtIn = data.keys.find(k => k.key === `${preset.backend}.command`)?.default
    return (
      <PresetRow
        key={preset.id}
        preset={preset}
        tag={tag}
        ladder={ladder && !preset.label.includes(ladder) ? ladder : undefined}
        availability={availabilityFor(data, preset.backend)}
        dep={dep}
        depsApi={depsApi}
        showProblem={!customOpen && preset.backend !== data.resolved.backend}
        customCommand={customCommand === builtIn ? undefined : customCommand}
        active={presetMatchesResolved(preset, data.resolved)}
        busy={presetBusy === preset.id}
        onUse={() => void applyPreset(preset.id)}
      />
    )
  }

  // The escalation setting: its help always describes the option that is selected.
  const modelLabels = labelMap(data.options.models[selectedBackend])
  const firstModel = fields.model || resolvedForSelected.model
  const savedEscalate = data.config[agentBackend]?.escalate
  const defaultLadder = savedEscalate === undefined ? (resolvedEscalate(data.resolved, selectedBackend) ?? []) : null
  const defaultLabel = defaultLadder === null
    ? 'Default'
    : `Default (${defaultLadder.length ? ladderLabel(firstModel, defaultLadder, modelLabels) : 'no escalation'})`
  const showActions = dirty || customOpen

  return (
    <div className="mt-10" data-testid="settings-view">
      <div
        className="text-[11px] font-semibold tracking-[0.2em] uppercase text-center mb-2"
        style={{ color: 'var(--silk-faint)', fontFamily: 'var(--font-mono)' }}
      >
        Datasheet drafting
      </div>
      <div className="text-[12px] text-center mb-5" data-testid="settings-intro" style={{ color: 'var(--silk-dim)' }}>
        Only used when you draft a part model from a datasheet. Board checks never use this.
      </div>

      {/* ── The current choice, once ── */}
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
          <div className="mt-1 text-[11px]" style={{ color: 'var(--silk-faint)' }} title={data.path}>
            {sourceLine(data)}. Your datasheet is sent to: {routing ?? 'unknown'}.
          </div>
          {currentProblem && currentAvailability && (
            <div className="mt-2.5">
              <BackendProblem availability={currentAvailability} dep={currentDep} signedOut={currentSignedOut} depsApi={depsApi} />
            </div>
          )}
        </div>
        <div className="flex items-center gap-2 shrink-0">
          <button
            type="button"
            data-testid="settings-test"
            disabled={testState.phase === 'running'}
            onClick={() => void runTest()}
            title="Sends a one-line prompt using the saved configuration."
            className="hb-btn hb-press px-3.5 py-1.5 text-[12px]"
            style={{ height: 30 }}
          >
            {testState.phase === 'running' ? 'Testing ...' : 'Test backend'}
          </button>
          <button
            type="button"
            data-testid="settings-change"
            aria-expanded={changeOpen}
            onClick={() => setChangeOpen(v => !v)}
            className="hb-btn hb-press px-3.5 py-1.5 text-[12px]"
            style={{ height: 30 }}
          >
            {changeOpen ? 'Close' : 'Change'}
          </button>
        </div>
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

      {/* ── The one place to choose: a recommended setup, a cheaper one, the rest, or custom ── */}
      {changeOpen && (
        <div className="mt-6" data-testid="settings-choose">
          {presetError && (
            <Callout tone="err" testId="settings-preset-error" className="mb-2.5">{presetError}</Callout>
          )}
          <div className="flex flex-col gap-3">
            {recommended && renderPreset(recommended, 'Recommended')}
            {cheaper && renderPreset(cheaper, 'Cheaper option')}
          </div>

          {others.length > 0 && (
            <div className="mt-4">
              <Disclosure open={moreOpen} onToggle={() => setMoreOpen(v => !v)} testId="settings-more-toggle">
                More setups ({others.length})
              </Disclosure>
              {moreOpen && (
                <div className="mt-2.5 grid grid-cols-1 gap-3" data-testid="settings-more">
                  {others.map(p => renderPreset(p))}
                </div>
              )}
            </div>
          )}

          <div className="mt-4">
            <Disclosure open={customOpen} onToggle={() => setCustomOpen(v => !v)} testId="settings-custom-toggle">
              Custom: tool, model, effort and escalation
            </Disclosure>
          </div>
        </div>
      )}

      {/* ── Custom ── */}
      {changeOpen && customOpen && (
        <div className="mt-3" data-testid="settings-custom">
          <BackendRows
            backends={data.backends}
            selected={selectedBackend}
            onSelect={id => { setSelectedBackend(id); setDraft(d => d && { ...d, backend: id }) }}
            deps={deps}
            depsApi={depsApi}
          />
          {selectedBackend !== data.resolved.backend && (
            <div className="mt-1.5 text-[11px] leading-relaxed" style={{ color: 'var(--silk-faint)' }}>
              This tool sends your datasheet to: {availability?.sends_data_to ?? 'unknown'}.
            </div>
          )}

          {/* ── Per-backend fields ── */}
          <div className="mt-4 rounded-xl px-4 py-3.5" style={{ border: '1px solid var(--hairline)', background: 'var(--surface)' }}>
            {selectedBackend === 'api' ? (
              <div className="flex flex-wrap gap-3.5">
                <Field label="Base URL" hint={`Leave empty to use ${data.resolved.api.base_url}.`}>
                  <MonoInput testId="settings-base-url" width="20rem" value={draft.api.base_url}
                    placeholder={data.resolved.api.base_url} onChange={v => setDraft(d => d && { ...d, api: { ...d.api, base_url: v } })} />
                </Field>
                <Field label="Model" hint={`Leave empty to use ${data.resolved.api.model}.`}>
                  <MonoInput testId="settings-model" width="17rem" list="settings-model-options" value={draft.api.model}
                    placeholder={data.resolved.api.model} onChange={setApiModel} />
                  <ModelPicks picks={data.options.models.api ?? []} current={draft.api.model || data.resolved.api.model}
                    hintable={hintsApply(data, 'api', draft.api.model || data.resolved.api.model)} onPick={setApiModel} />
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
                  <Field label="Model" hint={`Leave empty to use ${resolvedForSelected.model}.`}>
                    <MonoInput testId="settings-model" width="17rem" list="settings-model-options" value={fields.model}
                      placeholder={resolvedForSelected.model} onChange={fields.setModel} />
                    <datalist id="settings-model-options">
                      {(data.options.models[selectedBackend] ?? []).map(m => <option key={m.name} value={m.name} label={m.note} />)}
                    </datalist>
                    <ModelPicks picks={data.options.models[selectedBackend] ?? []}
                      current={firstModel} hintable onPick={fields.setModel} />
                  </Field>
                  <Field label="Effort">
                    <Segmented
                      testIdPrefix="settings-effort"
                      options={['', ...(data.options.efforts[selectedBackend] ?? [])]}
                      value={fields.effort}
                      labels={{ '': `Default (${resolvedForSelected.effort})` }}
                      onChange={v => fields.setEffort(v)}
                    />
                  </Field>
                </div>
                <Field
                  label="Escalate to"
                  hint={
                    <span data-testid="settings-escalate-effective">
                      {escalationSummary({ value: fields.escalate, firstModel, defaultLadder, labels: modelLabels })}
                    </span>
                  }
                >
                  <EscalatePicks
                    picks={data.options.models[selectedBackend] ?? []}
                    firstModel={firstModel}
                    value={fields.escalate}
                    effective={resolvedEscalate(data.resolved, selectedBackend) ?? []}
                    defaultLabel={defaultLabel}
                    onChange={fields.setEscalate}
                  />
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
                  <Field label="Permission mode" hint={`Default: ${resolvedPermission(data.resolved, selectedBackend)}.`}>
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
                <Field label="Retries" hint={`Leave empty for ${data.resolved.retries}.`}>
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
                <Field label="Timeout (seconds)" hint={`Leave empty for ${data.resolved.timeout_secs}.`}>
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
        </div>
      )}

      {/* ── Save: the page's one primary button, enabled only when there is something to save ── */}
      {showActions && (
        <div className="mt-4 flex flex-wrap items-center gap-2.5" data-testid="settings-actions">
          <button
            type="button"
            data-testid="settings-save"
            disabled={!dirty || saveState.phase === 'saving'}
            onClick={() => void save()}
            className={`${dirty ? 'hb-btn-primary' : 'hb-btn'} hb-press px-3.5 py-1.5 text-[12px]`}
            style={{ height: 30, ...(dirty ? {} : { opacity: 0.6 }) }}
          >
            {saveState.phase === 'saving' ? 'Saving ...' : dirty ? 'Save changes' : 'No changes to save'}
          </button>
          <button
            type="button"
            data-testid="settings-discard"
            disabled={!dirty}
            onClick={discard}
            className="hb-btn hb-press px-3 py-1.5 text-[12px]"
            style={{ height: 30, opacity: dirty ? 1 : 0.6 }}
          >
            Discard changes
          </button>
          <span className="flex-1" />
          <span className="text-[11px] truncate max-w-full" style={{ color: 'var(--silk-faint)', fontFamily: 'var(--font-mono)' }} title={data.path}>
            {tildeHome(data.path)}
          </span>
        </div>
      )}

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

      {changeOpen && customOpen && (
        <div className="mt-3 text-[11px]" style={{ color: 'var(--silk-faint)' }} data-testid="settings-terminal">
          <TerminalCommand command={terminalCommand} testId="settings-terminal-copy" />
        </div>
      )}
    </div>
  )
}

function resolvedPermission(resolved: ExtractResolved, backend: string): string {
  if (backend === 'claude-code') return resolved.claude_code.permission_mode ?? ''
  if (backend === 'codex' || backend === 'agy') return resolved[backend].permission_mode ?? ''
  return ''
}
