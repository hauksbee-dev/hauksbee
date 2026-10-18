import { lazy, Suspense, useState } from 'react'
import { downloadText } from '../../lib/report-export'
import type { LoadProgress } from '../../lib/api'
import { VerdictBand } from './VerdictBand'
import { ChecksExport } from './ChecksExport'
import type { RunStatus } from '../../lib/status-vocab'
import type { RunResponse } from '../../types/report'
import type { RemedyActions } from './Remedies'
import type { GutterResult } from '../editor/TomlEditor'

// The editor loads on first use, never in the app's first paint.
const TomlEditor = lazy(() => import('../editor/TomlEditor'))

// The Checks page frame: ONE verdict band (the verdict, what to do, Run, and
// the Export menu), then ONE working surface, the builder or the spec TOML,
// chosen by the mode switch. Both edit the same text. A run's issues are read
// inside the active surface (gutter marks and lint squiggles in the TOML, a tag
// on each builder row); the band only states the verdict, counts and jumps.
// The spec download writes the path lines shown above the editor followed by
// the editor's text, byte for byte.

export type SpecMode = 'builder' | 'toml'

export function SpecPane({
  mode, onModeChange, builder, firmwareChoice,
  specText, specStem, specFileName, bodyText, onBodyChange, pathLines, select, nets, refs, gutter, onGutterClick,
  showAllResults, stale, running, canRun, preparing, progress, onCancel, pulseRun, remedyActions, remedyEditor, onRun, onJump,
  validationCount, result, runKey, workflowYml, workflowPinned, reportSerious, preLayoutNotice, onMarkdown,
}: {
  mode: SpecMode
  onModeChange: (mode: SpecMode) => void
  /** The builder surface, shown in builder mode. */
  builder: React.ReactNode
  /** The firmware include choice, above the surface in either mode. */
  firmwareChoice?: React.ReactNode
  /** Jump to the first check with this status, inline in the active surface. */
  onJump: (status: RunStatus) => void
  /** The portable file: `pathLines` then the body. */
  specText: string
  specStem: string
  specFileName: string
  /** The spec body: the one source of truth the builder and the editor share. */
  bodyText: string
  onBodyChange: (text: string) => void
  /** The `board = ...` lines the download adds for the checked-in copy. */
  pathLines: string
  /** A range to select in the editor (a catalogue example's first value to fill). */
  select?: { start: number; end: number; seq: number } | null
  nets: readonly string[]
  refs: readonly string[]
  /** The last run's verdict per [[assert]], for the editor's gutter. */
  gutter: readonly (GutterResult | null)[]
  onGutterClick: (assertIndex: number) => void
  /** List every result under the verdict (some assertions have no builder row). */
  showAllResults: boolean
  stale: boolean
  running: boolean
  canRun: boolean
  /** The starter supplies are still on their way: a run now would not be the first one. */
  preparing?: boolean
  /** How far the engine says the run (or the preparation) has got. */
  progress?: LoadProgress | null
  /** Stop the run, or stop waiting for the preparation. */
  onCancel?: () => void
  /** A remedy changed the spec: point at Run, the only thing that re-judges. */
  pulseRun?: boolean
  remedyActions?: RemedyActions
  /** The model editor a remedy opened, shown under the verdict. */
  remedyEditor?: React.ReactNode
  onRun: () => void
  /** Rows the preflight refused, for the "nothing was run" line. */
  validationCount: number
  result: RunResponse | null
  runKey: string | undefined
  /** The generated workflow, or null on a build with no release commit. */
  workflowYml: string
  /** False in a development build: the YAML is a preview, not downloadable. */
  workflowPinned: boolean
  /** Serious findings on the board's own report, carried into the verdict. */
  reportSerious: number
  /** The board report's not-routed notice, for the band before a run. */
  preLayoutNotice: string | null
  /** Download the landed run as Markdown. */
  onMarkdown?: () => void
}) {
  const [ciOpen, setCiOpen] = useState(false)
  const download = (name: string, contents: string) =>
    downloadText(name, contents, 'text/plain;charset=utf-8')

  const modeButton = (value: SpecMode, label: string) => (
    <button
      type="button"
      role="tab"
      aria-selected={mode === value}
      data-testid={`mode-${value}`}
      onClick={() => onModeChange(value)}
      className="hb-press px-3 py-1 text-[12px] cursor-pointer rounded-md"
      style={{
        border: 'none',
        background: mode === value ? 'var(--copper-tint-strong)' : 'transparent',
        color: mode === value ? 'var(--silk)' : 'var(--silk-dim)',
        fontWeight: mode === value ? 600 : 400,
      }}
    >
      {label}
    </button>
  )

  return (
    <div className="min-w-0" data-testid="spec-pane" data-mode={mode}>
      <VerdictBand
        result={result} runKey={runKey} stale={stale} running={running} preparing={preparing}
        progress={progress} onCancel={onCancel} canRun={canRun} onRun={onRun} pulseRun={pulseRun}
        validationCount={validationCount} reportSerious={reportSerious} onJump={onJump}
        specFileName={specFileName} specToml={runKey ?? ''} rawMode={showAllResults}
        preLayoutNotice={preLayoutNotice} remedyActions={remedyActions} remedyEditor={remedyEditor}
        exportMenu={
          <ChecksExport
            specFileName={specFileName}
            onDownloadSpec={() => download(specFileName, specText)}
            cliCommand={`hauksbee-ci run ci/${specFileName}`}
            onSetUpCi={() => setCiOpen(open => !open)}
            ciOpen={ciOpen}
            exportId={result?.ok && !stale ? result.export_id : null}
            onMarkdown={result?.ok ? onMarkdown : undefined}
          />
        }
      />

      {/* GitHub CI setup: the two files and where they go. */}
      {ciOpen && (
        <div data-testid="ci-setup-panel" className="hb-card view-enter mt-3 px-3 py-3">
          <div className="text-[13px] mb-2" style={{ color: 'var(--silk-dim)' }}>
            Two files make this run on every push. Recommended layout: board in{' '}
            <code className="hb-inline">hardware/</code>, firmware in <code className="hb-inline">firmware/</code>,
            this spec in <code className="hb-inline">ci/</code>.
          </div>
          <div className="text-[12px] mb-1" style={{ color: 'var(--silk-dim)' }}>
            1. <code className="hb-inline break-all">ci/{specStem}.toml</code>; Export, then Download {specFileName}, produces it
            (paths already relative to that layout).
          </div>
          {/* break-all: the workflow path outgrows the sticky card at
              320px wide; a wrapped path beats one clipped mid-word. */}
          <div className="text-[12px] mb-1.5" style={{ color: 'var(--silk-dim)' }}>
            2. <code className="hb-inline break-all">.github/workflows/hauksbee-ci.yml</code>:
          </div>
          {!workflowPinned && (
            <div
              data-testid="ci-dev-banner"
              className="text-[12px] mb-2 px-2.5 py-2 rounded-md leading-relaxed"
              style={{ color: 'var(--warn)', border: '1px solid var(--warn)' }}
            >
              Dev build: pin a release before committing. This is a preview. Replace{' '}
              <code className="hb-inline">PIN-A-RELEASE-COMMIT</code> with a released Hauksbee
              build&apos;s commit, or install a released build and download the real file.
            </div>
          )}
          {/* pre-wrap: the long `paths:` lines soft-wrap at spaces
              instead of clipping at the card's right edge (the
              copied text keeps its real newlines either way). */}
          <pre className="hb-code p-3 overflow-x-auto whitespace-pre-wrap text-[11px] leading-relaxed">
            {workflowYml}
          </pre>
          {workflowPinned && (
            <button
              type="button"
              className="hb-btn hb-press mt-2 px-3 py-1.5 text-[12px]"
              onClick={() => download('hauksbee-ci.yml', workflowYml)}
            >
              Download hauksbee-ci.yml
            </button>
          )}
          <div className="text-[12px] mt-2 leading-relaxed" style={{ color: 'var(--silk-dim)' }}>
            The workflow needs no secret or token: the Action and its release assets are public
            and pinned to one commit.
          </div>
        </div>
      )}

      {/* How to edit: the builder or the TOML, one surface at a time. */}
      <div className="mt-5 mb-2 flex flex-wrap items-center gap-x-3 gap-y-2">
        <div role="tablist" aria-label="How to edit the checks" className="inline-flex items-center gap-0.5 p-0.5 rounded-lg" style={{ border: '1px solid var(--hairline)', background: 'var(--surface-2)' }}>
          {modeButton('builder', 'Builder')}
          {modeButton('toml', 'TOML')}
        </div>
        <span
          className="text-[11px] inline-flex items-center gap-1.5 min-w-0"
          style={{ color: 'var(--silk-faint)' }}
          title={stale ? 'The spec changed since the last run' : 'The spec matches the last run'}
        >
          <span style={{ width: 7, height: 7, borderRadius: 4, display: 'inline-block', flexShrink: 0, background: stale ? 'var(--warn)' : 'var(--ok)' }} />
          <span className="truncate">{specFileName}{stale ? ', changed since the last run' : ''}</span>
        </span>
      </div>

      {firmwareChoice && <div className="mb-3">{firmwareChoice}</div>}

      <div>
        {mode === 'builder' ? builder : (
          <div className="hb-card overflow-hidden">
            {pathLines && (
              <pre
                data-testid="spec-paths"
                title="Added by the Download button for the checked-in copy. A run here uses this session's uploads."
                className="m-0 px-3 pt-2 pb-1.5 text-[11px] leading-relaxed overflow-x-auto"
                style={{ background: 'var(--code-bg)', color: 'var(--silk-faint)', fontFamily: 'var(--font-mono)', borderBottom: '1px dashed var(--hairline)' }}
              >
                {pathLines.replace(/\n$/, '')}
              </pre>
            )}
            <Suspense fallback={
              <pre
                data-testid="toml-editor-loading"
                className="p-3 m-0 overflow-auto text-[12px] leading-relaxed"
                style={{ background: 'var(--code-bg)', color: 'var(--silk-dim)', fontFamily: 'var(--font-mono)', maxHeight: '60vh' }}
              >
                {bodyText}
              </pre>
            }>
              <TomlEditor
                value={bodyText}
                onChange={onBodyChange}
                nets={nets}
                refs={refs}
                results={gutter}
                onResultClick={onGutterClick}
                select={select}
                testId="toml-editor"
                ariaLabel={`${specFileName}, the checks spec`}
              />
            </Suspense>
          </div>
        )}
      </div>

      {/* The exact bytes the spec download writes, whole. The TOML mode is the
          file itself, so this only rides under the builder. */}
      {mode === 'builder' && <details className="mt-2 text-[12px] leading-relaxed" style={{ color: 'var(--silk-faint)' }}>
        <summary className="cursor-pointer">
          See the spec file this builds
        </summary>
        <div className="mt-1">
          A run here uses the files uploaded in this session; the path lines are for the checked-in copy.
        </div>
        <pre
          data-testid="spec-preview"
          className="hb-code mt-1.5 p-3 overflow-auto text-[11px] leading-relaxed"
          style={{ maxHeight: '30vh' }}
        >
          {specText}
        </pre>
      </details>}
    </div>
  )
}
