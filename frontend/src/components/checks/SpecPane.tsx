import { useState } from 'react'
import { downloadText } from '../../lib/report-export'
import { tomlToBuilder } from '../../lib/check-spec'
import type { BuilderState } from '../../lib/check-spec'
import { RunResults } from './RunResults'
import type { RunResponse } from '../../types/report'

// The right-hand column: the spec TOML as it currently stands, the run/download
// actions, the run's verdict, and the GitHub CI setup. What this pane shows IS
// the file the Download button writes, byte for byte, including the `board` and
// `firmware` path lines.

export function SpecPane({
  specText, specStem, specFileName, builtToml, rawMode, rawText, setRawText,
  onLeaveRawMode, onEnterRawMode, stale, running, canRun, onRun,
  validationCount, result, runKey, workflowYml,
}: {
  specText: string
  specStem: string
  specFileName: string
  builtToml: string
  rawMode: boolean
  rawText: string
  setRawText: (text: string) => void
  /** The parsed builder state when the raw TOML fits the builder's vocabulary,
   *  or null when going back would discard it. */
  onLeaveRawMode: (parsed: BuilderState | null) => void
  onEnterRawMode: () => void
  stale: boolean
  running: boolean
  canRun: boolean
  onRun: () => void
  /** Rows the preflight refused, for the "nothing was run" line. */
  validationCount: number
  result: RunResponse | null
  runKey: string | undefined
  /** The generated workflow, or null on a build with no release commit. */
  workflowYml: string | null
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
        <div className="flex items-center justify-between px-3 py-2" style={{ borderBottom: '1px solid var(--hairline)' }}>
          <div className="text-[11px] font-bold tracking-widest uppercase inline-flex items-center gap-2" style={{ color: 'var(--silk-faint)' }}>
            <span
              style={{
                width: 7, height: 7, borderRadius: 4, display: 'inline-block',
                background: stale ? 'var(--warn)' : 'var(--ok)',
              }}
              title={stale
                ? 'The spec changed since the last run'
                : 'These are the assertions that run, and this is the file the download writes'}
            />
            spec.toml
          </div>
          <button
            type="button"
            data-testid="raw-toggle"
            className="hb-press text-[11px] cursor-pointer"
            style={{ color: 'var(--copper)', background: 'none', border: 'none' }}
            onClick={() => {
              if (!rawMode) {
                setRawText(builtToml)
                onEnterRawMode()
                return
              }
              // Raw is the source of truth on a successful parse: an
              // intentionally emptied list clears the builder rows too. When it
              // does not parse, going back DISCARDS it, so ask first.
              const parsed = tomlToBuilder(rawText)
              if (parsed) onLeaveRawMode(parsed)
              else if (window.confirm(
                'This TOML uses features the visual builder does not cover '
                + '(tolerances, scenarios, overrides...). Going back to the '
                + 'builder will DISCARD the raw text. Continue?')) {
                onLeaveRawMode(null)
              }
            }}
          >
            {rawMode ? '← back to the builder' : 'edit raw →'}
          </button>
        </div>
        {rawMode ? (
          <textarea
            data-testid="raw-toml"
            value={rawText}
            onChange={e => setRawText(e.target.value)}
            spellCheck={false}
            className="w-full block p-3"
            style={{
              background: 'var(--code-bg)', border: 'none', outline: 'none', resize: 'vertical',
              color: 'var(--silk)', minHeight: 260, maxHeight: '46vh',
              fontFamily: 'var(--font-mono)', fontSize: 12, lineHeight: 1.5,
            }}
          />
        ) : (
          // What this pane shows IS the file the Download button
          // writes, byte for byte, including the `board` (and
          // `firmware`) path lines.
          <pre
            data-testid="spec-preview"
            className="p-3 m-0 overflow-auto text-[12px] leading-relaxed"
            style={{
              background: 'var(--code-bg)', color: 'var(--silk-dim)',
              fontFamily: 'var(--font-mono)', maxHeight: '46vh',
            }}
          >
            {specText}
          </pre>
        )}
      </div>
      {!rawMode && (
        <div className="mt-1.5 text-[12px] leading-relaxed" style={{ color: 'var(--silk-faint)' }}>
          This is the file the Download button writes. Running from here uses the
          design inputs already uploaded in this session, so the path lines are for
          the checked-in copy.
        </div>
      )}

      <div className="mt-3 flex flex-wrap items-center gap-2">
        <button
          type="button"
          data-testid="run-checks"
          disabled={!canRun || running}
          onClick={onRun}
          className="hb-btn-primary hb-press px-4 py-2 text-[13px] inline-flex items-center gap-2"
        >
          {running && <span className="slot-spin" style={{ borderTopColor: 'var(--on-copper)' }} />}
          {running ? 'Running…' : 'Run these checks now'}
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
      {!rawMode && validationCount > 0 && (
        <div
          data-testid="builder-validation"
          className="mt-3 rounded-lg px-3 py-2.5 text-[13px]"
          style={{ background: 'var(--err-bg)', border: '1px solid var(--err-border)', color: 'var(--err-strong)' }}
        >
          Nothing was run: {validationCount === 1 ? 'a check is' : `${validationCount} checks are`} missing
          values. The highlighted fields above say which.
        </div>
      )}

      {result && <RunResults result={result} runKey={runKey} stale={stale} rawMode={rawMode} />}

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
          {workflowYml ? (
            <>
              {/* pre-wrap: the long `paths:` lines soft-wrap at spaces
                  instead of clipping at the card's right edge (the
                  copied text keeps its real newlines either way). */}
              <pre className="hb-code p-3 overflow-x-auto whitespace-pre-wrap text-[11px] leading-relaxed">
                {workflowYml}
              </pre>
              <button
                type="button"
                className="hb-btn hb-press mt-2 px-3 py-1.5 text-[12px]"
                onClick={() => download('hauksbee-ci.yml', workflowYml)}
              >
                Download hauksbee-ci.yml
              </button>
            </>
          ) : (
            <div className="text-[12px] leading-relaxed" style={{ color: 'var(--warn)' }}>
              This development build has no immutable release commit, so it will not export a credential-bearing workflow. Install a released Hauksbee build first.
            </div>
          )}
        </div>
      )}
    </div>
  </div>
    )
  }
