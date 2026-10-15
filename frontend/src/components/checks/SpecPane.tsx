import { lazy, Suspense, useState } from 'react'
import { downloadText } from '../../lib/report-export'
import { RunResults } from './RunResults'
import type { RunResponse } from '../../types/report'
import type { RemedyActions } from './Remedies'
import type { GutterResult } from '../editor/TomlEditor'

// The editor loads on first use, never in the app's first paint.
const TomlEditor = lazy(() => import('../editor/TomlEditor'))

// The right-hand column: the spec TOML, live and editable beside the builder
// (both edit the same text), the run/download actions, the run's verdict, and
// the GitHub CI setup. The Download button writes the path lines shown above
// the editor followed by the editor's text, byte for byte.

export function SpecPane({
  specText, specStem, specFileName, bodyText, onBodyChange, pathLines, select, nets, refs, gutter, onGutterClick,
  showAllResults, stale, running, canRun, preparing, pulseRun, remedyActions, remedyEditor, onRun,
  validationCount, result, runKey, workflowYml, workflowPinned, reportSerious,
}: {
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
}) {
  const [ciOpen, setCiOpen] = useState(false)
  const download = (name: string, contents: string) =>
    downloadText(name, contents, 'text/plain;charset=utf-8')

  // Pinned beside the builder in two columns; below the stacking width it sits
  // after the builder and scrolls with it (`.checks-spec`).
  return (
    <div className="min-w-0">
    <div className="sticky top-0 checks-spec">
      <div className="hb-card overflow-hidden">
        <div className="flex items-center justify-between gap-2 px-3 py-2" style={{ borderBottom: '1px solid var(--hairline)' }}>
          <div className="text-[11px] font-bold tracking-widest uppercase inline-flex items-center gap-2 min-w-0" style={{ color: 'var(--silk-faint)' }}>
            <span
              style={{
                width: 7, height: 7, borderRadius: 4, display: 'inline-block', flexShrink: 0,
                background: stale ? 'var(--warn)' : 'var(--ok)',
              }}
              title={stale
                ? 'The spec changed since the last run'
                : 'These are the assertions that run, and this is the file the download writes'}
            />
            <span className="truncate">{specFileName}</span>
          </div>
          <span className="text-[11px] shrink-0" style={{ color: 'var(--silk-faint)' }}>
            type here or use the builder
          </span>
        </div>
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
            style={{ background: 'var(--code-bg)', color: 'var(--silk-dim)', fontFamily: 'var(--font-mono)', maxHeight: '46vh' }}
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
      {/* The exact bytes the Download button writes, whole. */}
      <details className="mt-1.5 text-[12px] leading-relaxed" style={{ color: 'var(--silk-faint)' }}>
        <summary className="cursor-pointer">
          The file as downloaded. Running from here uses the design inputs already uploaded
          in this session, so the path lines are for the checked-in copy.
        </summary>
        <pre
          data-testid="spec-preview"
          className="hb-code mt-1.5 p-3 overflow-auto text-[11px] leading-relaxed"
          style={{ maxHeight: '30vh' }}
        >
          {specText}
        </pre>
      </details>

      <div className="mt-3 flex flex-wrap items-center gap-2">
        <button
          type="button"
          data-testid="run-checks"
          disabled={!canRun || running || preparing}
          onClick={onRun}
          className={`hb-btn-primary hb-press px-4 py-2 text-[13px] inline-flex items-center gap-2${pulseRun && !running ? ' run-nudge' : ''}`}
        >
          {running && <span className="slot-spin" style={{ borderTopColor: 'var(--on-copper)' }} />}
          {running ? 'Running…' : preparing ? 'Preparing…' : pulseRun ? 'Run the checks again' : 'Run these checks now'}
        </button>
        <button
          type="button"
          className="hb-btn hb-press px-3 py-2 text-[13px]"
          onClick={() => download(specFileName, specText)}
        >
          Download {specStem}.toml
        </button>
        <button
          type="button"
          className="hb-btn hb-press px-3 py-2 text-[13px]"
          onClick={() => setCiOpen(open => !open)}
        >
          {ciOpen ? 'Hide GitHub CI setup' : 'Set up GitHub CI'}
        </button>
      </div>
      {!canRun && (
        <div className="mt-1.5 text-[12px]" style={{ color: 'var(--silk-faint)' }}>
          (running from here needs the board uploaded in this session)
        </div>
      )}
      {validationCount > 0 && (
        <div
          data-testid="builder-validation"
          className="mt-3 rounded-lg px-3 py-2.5 text-[13px]"
          style={{ background: 'var(--err-bg)', border: '1px solid var(--err-border)', color: 'var(--err-strong)' }}
        >
          Nothing was run: {validationCount === 1 ? 'a check is' : `${validationCount} checks are`} missing
          values. The highlighted fields above say which.
        </div>
      )}

      {result && <RunResults
        result={result} runKey={runKey} stale={stale} rawMode={showAllResults} reportSerious={reportSerious}
        remedyActions={remedyActions} remedyEditor={remedyEditor}
      />}

      {/* GitHub CI setup: the two files and where they go. */}
      {ciOpen && (
        <div data-testid="ci-setup-panel" className="hb-card view-enter mt-3 px-3 py-3">
          <div className="text-[13px] mb-2" style={{ color: 'var(--silk-dim)' }}>
            Two files make this run on every push. Recommended layout: board in{' '}
            <code className="hb-inline">hardware/</code>, firmware in <code className="hb-inline">firmware/</code>,
            this spec in <code className="hb-inline">ci/</code>.
          </div>
          <div className="text-[12px] mb-1" style={{ color: 'var(--silk-dim)' }}>
            1. <code className="hb-inline break-all">ci/{specStem}.toml</code>; the Download button above produces it
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
    </div>
  </div>
    )
  }
