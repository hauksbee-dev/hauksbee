import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import type { WebSection, WebFinding, WebHeadsUp, WebComponent, WebCosimSection, WebImportDiagnostics } from '../types/report'
import { fallbackWindowLine, timingCoverageLine, uncoveredTimingRefusals } from '../lib/cosim-coverage'
import { summarizeErrorBudget } from '../lib/error-budget'
import type { BoardSession } from '../hooks/useBoardSession'
import { CheckIcon, WarningIcon } from './Icons'
import { BoardViewer, TOOLBAR_CLEARANCE } from './BoardViewer'
import { SelectionCard } from './SelectionCard'
import { FirmwareJack } from './FirmwareJack'
import { SchematicJack } from './SchematicJack'
import { DatasheetExtract } from './DatasheetExtract'
import { WritePart } from './WritePart'
import { displayNet } from '../lib/net-name'
import { cssToken, onThemeChange } from '../lib/theme-tokens'
import { acceptedFormatsSentence, withoutEngineFormatList } from '../lib/board-formats'
import { ArriveOnce, StaggerItem } from '../motion'
import { ExportMenu } from './ExportMenu'
import type { SpecSnapshot } from '../hooks/useSessions'
import { groupFindings } from '../lib/findings'
import type { FindingGroup } from '../lib/findings'
import { summarizeEvidence } from '../lib/evidence'
import { reportVerdictHeadline, reportVerdictPalette } from '../lib/report-verdict'
import { refusalLines } from '../lib/refusal-contract'

// The Board view with a report in hand: the viewer as the hero surface (with
// its toolbar and layers panel), the plain-language verdict, and the findings.
// Landing/UploadView owns getting a board in; everything here owns saying what
// came back and letting the board be explored.

const LEVEL_ACCENT: Record<string, string> = {
  serious: 'var(--err)',
  warning: 'var(--warn)',
  note: 'var(--note-accent)',
}

const LEVEL_TEXT: Record<string, string> = {
  serious: 'var(--err-strong)',
  warning: 'var(--warn-strong)',
  note: 'var(--note)',
}

// Copy-to-clipboard button for the CLI fallback path.
function CopyButton({ text, label = 'Copy' }: { text: string; label?: string }) {
  const [copied, setCopied] = useState(false)
  const copy = useCallback(async () => {
    try {
      await navigator.clipboard.writeText(text)
    } catch {
      // Fallback for insecure contexts / older browsers.
      const ta = document.createElement('textarea')
      ta.value = text
      ta.style.position = 'fixed'
      ta.style.opacity = '0'
      document.body.appendChild(ta)
      ta.select()
      try { document.execCommand('copy') } catch { /* nothing more to try */ }
      document.body.removeChild(ta)
    }
    setCopied(true)
    setTimeout(() => setCopied(false), 1500)
  }, [text])
  return (
    <button
      type="button"
      data-testid="copy-cli"
      onClick={() => void copy()}
      className="hb-press ml-2 rounded px-2 py-0.5 text-[11px] font-semibold cursor-pointer"
      style={{
        background: copied ? 'var(--ok-bg)' : 'var(--copper-tint)',
        border: `1px solid ${copied ? 'var(--ok-border)' : 'var(--copper-deep)'}`,
        color: copied ? 'var(--ok)' : 'var(--copper-hi)',
        whiteSpace: 'nowrap',
      }}
    >
      {copied ? (
        <span className="inline-flex items-center gap-1"><CheckIcon size={11} /> Copied</span>
      ) : label}
    </button>
  )
}

function ImportDiagnosticsPanel({
  diagnostics, overlay, selectedNet, onToggleOverlay, onLocate, onInspectNet,
}: {
  diagnostics: WebImportDiagnostics
  overlay: boolean
  selectedNet: string | null
  onToggleOverlay: () => void
  onLocate: (x: number, y: number, label: string) => void
  onInspectNet: (net: string) => void
}) {
  const issues = diagnostics.issues ?? []
  const caveated = diagnostics.partial + diagnostics.missing_or_refused
  return (
    <details
      data-testid="import-diagnostics"
      open={caveated > 0 || issues.length > 0}
      className="mt-3 rounded-lg px-4 py-3"
      style={{
        border: `1px solid ${caveated > 0 ? 'var(--warn-border)' : 'var(--hairline)'}`,
        background: 'var(--surface)',
      }}
    >
      <summary className="cursor-pointer text-sm font-semibold" style={{ color: 'var(--silk)' }}>
        Import coverage
        <span className="ml-2 text-[11px] font-normal tnum" style={{ color: 'var(--silk-dim)' }}>
          {diagnostics.recovered} recovered · {diagnostics.partial} partial · {diagnostics.unplaced} unplaced
          {diagnostics.missing_or_refused > 0 ? ` · ${diagnostics.missing_or_refused} missing/refused limit${diagnostics.missing_or_refused === 1 ? '' : 's'}` : ''}
        </span>
      </summary>
      <div className="mt-2 flex flex-wrap items-center justify-between gap-2 text-[12px]" style={{ color: 'var(--silk-dim)' }}>
        <span>
          Reader: <b style={{ color: 'var(--silk)', fontWeight: 600 }}>{diagnostics.format}</b>.
          Confidence describes fields actually recovered; it is not a claim about the physical board.
        </span>
        <button
          type="button"
          data-testid="toggle-import-overlay"
          className="hb-press rounded-md px-2.5 py-1 text-[11px] font-semibold"
          style={{
            border: '1px solid var(--hairline)',
            background: overlay ? 'var(--copper-tint)' : 'var(--canvas)',
            color: overlay ? 'var(--copper-hi)' : 'var(--silk-dim)',
          }}
          onClick={onToggleOverlay}
        >
          {overlay ? 'Hide board overlay' : 'Show recovered / partial on board'}
        </button>
      </div>

      {issues.map((issue, index) => {
        const locatedOnNet = issue.net
          ? diagnostics.objects.filter(object =>
              object.x !== undefined && object.y !== undefined && object.nets?.includes(issue.net!),
            )
          : []
        const inspecting = !!issue.net && selectedNet === issue.net
        return (
          <div
            key={`${index}:${issue.kind}:${issue.title}`}
            data-testid="import-issue"
            className="mt-2 rounded-md px-3 py-2.5 text-[12px] leading-relaxed"
            style={{ border: '1px solid var(--warn-border)', background: 'var(--warn-bg)' }}
          >
            <div className="font-semibold" style={{ color: 'var(--warn-strong)' }}>{issue.title}</div>
            <div className="mt-1" style={{ color: 'var(--silk)' }}>{issue.explanation}</div>
            <div className="mt-1" style={{ color: 'var(--silk-dim)' }}><b>What fixes it:</b> {issue.suggested_fix}</div>
            {issue.net && (
              <>
                <button
                  type="button"
                  className="hb-press mt-2 rounded px-2 py-1 text-[11px] font-semibold"
                  style={{ border: '1px solid var(--hairline)', color: 'var(--copper-hi)', background: 'var(--surface)' }}
                  onClick={() => onInspectNet(issue.net!)}
                >
                  Inspect {displayNet(issue.net)}
                </button>
                {inspecting && (
                  <div data-testid="import-net-inspection" className="mt-2" style={{ color: 'var(--silk-dim)' }}>
                    {locatedOnNet.length > 0
                      ? `Highlighted ${locatedOnNet.length} located imported object${locatedOnNet.length === 1 ? '' : 's'} on this net.`
                      : 'No placeable object was recovered for this net. The reader supplied no coordinate to highlight.'}
                  </div>
                )}
              </>
            )}
          </div>
        )
      })}

      <div className="mt-3 max-h-64 overflow-y-auto rounded-md" style={{ border: '1px solid var(--hairline)' }}>
        {diagnostics.objects.map(object => {
          const located = object.x !== undefined && object.y !== undefined
          return (
            <div
              key={object.id}
              data-testid="import-object"
              className="grid gap-2 px-3 py-2 text-[12px] sm:grid-cols-[minmax(5rem,auto)_auto_minmax(0,1fr)_auto]"
              style={{ borderBottom: '1px solid var(--hairline)', color: 'var(--silk-dim)' }}
            >
              <span style={{ color: 'var(--silk)', fontFamily: 'var(--font-mono)' }}>{object.id}</span>
              <span style={{ color: object.status === 'recovered' ? 'var(--ok)' : 'var(--warn-strong)' }}>{object.status}</span>
              <span title={object.explanation}>{object.confidence} confidence · {object.explanation}</span>
              {located ? (
                <button
                  type="button"
                  className="hb-press text-[11px] font-semibold"
                  style={{ color: 'var(--copper-hi)', background: 'none', border: 'none' }}
                  onClick={() => onLocate(object.x!, object.y!, `Imported ${object.id}: ${object.status}`)}
                >
                  Show
                </button>
              ) : <span title="The source supplied no coordinate, so plotting one would be fabricated.">not placeable</span>}
            </div>
          )
        })}
      </div>
      {diagnostics.unplaced > 0 && (
        <p className="mt-2 text-[11px]" style={{ color: 'var(--silk-dim)' }}>
          Unplaced objects stay in this list. They are not drawn at guessed coordinates.
        </p>
      )}
    </details>
  )
}

export function BoardView({
  session, onQueueCheck, onOpenChecks, onDriveLive, simMounted, engineVersion, spec, checks, sessionName,
}: {
  session: BoardSession
  onQueueCheck: (check: { kind: string; net?: string; ref?: string }) => void
  onOpenChecks: () => void
  onDriveLive: () => void
  simMounted: boolean
  /** The hauksbee that produced the report, for the exported file's provenance. */
  engineVersion: string | null
  /** The spec the Checks pane composed, offered alongside the report. */
  spec: SpecSnapshot | null
  checks: { passed: number; failed: number; invalid: number } | null
  sessionName: string | null
}) {
  const r = session.report!
  const {
    boardUrl, selectedNet, selectedComponent, setSelectedNet, setSelectedComponent,
    busy, uploadError, uploadNotice, dismissNotice, firmwareFile, schematicFile, handleFirmware,
    clearFirmware, handleSchematic, clearSchematic, boardFile, boardLabel, liveMode, onEmptyBoard, restoredFrom,
  } = session

  // Every hook in this component lives ABOVE the unreadable-file branch below.
  // It used to sit under it, so a session that went from a refused file to a good
  // one (or back) rendered a different NUMBER of hooks than the render before and
  // React tore the tree down with "rendered more hooks than during the previous
  // render". Hooks first, then the early return.
  // "Show on board": pan/zoom the map to a finding's board location and drop
  // a labeled marker there. Only wired when the real renderer is drawing
  // (the dot map has no camera to move).
  const [focusPoint, setFocusPoint] = useState<{ x: number; y: number; label: string; seq: number } | null>(null)
  // Which mode the viewer's 2D/3D control is in, so the caption under the
  // canvas describes the interactions that actually exist in that mode.
  const [viewerMode, setViewerMode] = useState<'2d' | '3d'>('2d')
  // Expand-to-viewport for the map. Per-view and deliberately not persisted:
  // it is a "let me look at this properly" gesture, not a setting.
  const [mapFullscreen, setMapFullscreen] = useState(false)
  const [importOverlay, setImportOverlay] = useState(false)
  const focusSeq = useRef(0)
  const mapRef = useRef<HTMLDivElement>(null)
  const locate = useCallback((x: number, y: number, label: string) => {
    focusSeq.current += 1
    setFocusPoint({ x, y, label, seq: focusSeq.current })
    mapRef.current?.scrollIntoView({ behavior: 'smooth', block: 'center' })
  }, [])
  useEffect(() => setImportOverlay(false), [session.runEpoch])
  const importMarkers = useMemo(() => {
    if (!importOverlay) return []
    return (r.import_diagnostics?.objects ?? []).flatMap(object =>
      object.x !== undefined && object.y !== undefined
        ? [{ x: object.x, y: object.y, status: object.status, nets: object.nets ?? [] }]
        : [])
  }, [importOverlay, r.import_diagnostics])
  const inspectImportNet = useCallback((net: string) => {
    setImportOverlay(true)
    setSelectedNet(net)
    mapRef.current?.scrollIntoView({ behavior: 'smooth', block: 'center' })
  }, [setSelectedNet])

  if (!r.ok) {
    return (
      <div className="h-full overflow-y-auto view-enter">
        <div className="max-w-3xl mx-auto px-6 pt-8 pb-16">
          <div
            data-testid="report-verdict"
            className="rounded-lg px-4 py-3.5"
            style={{ border: '1px solid var(--err-border)', background: 'var(--err-bg)', color: 'var(--err-strong)' }}
          >
            {r.error ? withoutEngineFormatList(r.error) : 'Could not read the file.'}
          </div>
          {r.import_failure && (
            <div
              data-testid="import-failure"
              className="mt-3 rounded-lg px-4 py-3 text-[13px] leading-relaxed"
              style={{ border: '1px solid var(--warn-border)', background: 'var(--warn-bg)', color: 'var(--silk)' }}
            >
              <div className="text-[10px] font-bold tracking-widest uppercase" style={{ color: 'var(--warn-strong)' }}>
                Import stopped at {r.import_failure.stage}
              </div>
              {r.import_failure.excerpt && (
                <pre className="mt-2 overflow-x-auto rounded px-3 py-2 text-[11px]" style={{ background: 'var(--canvas)', border: '1px solid var(--hairline)' }}>
                  {r.import_failure.excerpt}
                </pre>
              )}
              <div className="mt-2"><b>Suggested fix:</b> {r.import_failure.suggested_fix}</div>
            </div>
          )}
          {/* The dead end must not be dead: offer the retry inline instead of
              sending the user hunting for the header button. */}
          <div className="mt-4 flex flex-wrap items-center gap-3">
            <label
              htmlFor="board-file"
              data-testid="try-another-file"
              role="button"
              tabIndex={0}
              onKeyDown={e => {
                if (e.key === 'Enter' || e.key === ' ') {
                  e.preventDefault()
                  document.getElementById('board-file')?.click()
                }
              }}
              className="hb-btn-primary hb-press inline-flex items-center px-3.5 text-[13px] cursor-pointer"
              style={{ height: 32 }}
            >
              Try another file
            </label>
            {/* The one accepted-formats list (lib/board-formats). This line is
                read by exactly one audience: someone whose file was just
                refused. It is the worst possible place for a list to be short
                by a format, and it had been: the engine's own refusal text
                omits Altium, so an Altium user reading only that concluded the
                tool cannot open their file at all. */}
            <span className="text-[12px]" style={{ color: 'var(--silk-dim)' }}>
              accepted: {acceptedFormatsSentence()}
            </span>
          </div>
        </div>
      </div>
    )
  }

  const bindOpen = !!(r.bind?.active_path_unresolved?.length)
  const evidenceSummary = summarizeEvidence(r.evidence)
  const runCommand = `hauksbee run ${boardLabel ?? r.file_name} --serve`
  const { border: verdictBorder, background: verdictBg } = reportVerdictPalette(r)

  return (
    <div className="h-full overflow-y-auto view-enter" data-testid="report">
      <div className="max-w-4xl mx-auto px-6 pt-5 pb-16">
        {/* Re-analysis in progress (firmware added or swapped): the report
            stays visible, this line says what is happening. */}
        {busy && (
          <div
            role="status"
            aria-live="polite"
            className="mb-4 text-sm flex items-center justify-center gap-2"
            style={{ color: 'var(--copper-hi)' }}
          >
            <span className="slot-spin" />
            {busy.firmware
              ? <>Analyzing <span style={{ fontFamily: 'var(--font-mono)' }}>{busy.board}</span> + co-sim of <span style={{ fontFamily: 'var(--font-mono)' }}>{busy.firmware}</span> ...</>
              : <>Analyzing <span style={{ fontFamily: 'var(--font-mono)' }}>{busy.board}</span> ...</>}
          </div>
        )}
        {/* A drop the app re-routed rather than refused (a firmware project zip
            landing on the board zone). Same wording as the intake's banner. */}
        {uploadNotice && (
          <ArriveOnce
            className="mb-4 rounded-lg px-4 py-3 text-[13px] leading-relaxed"
            style={{ background: 'var(--warn-bg)', border: '1px solid var(--warn-border)', color: 'var(--silk)' }}
          >
            <div data-testid="upload-notice" aria-live="polite">
              {uploadNotice}
              <button
                type="button"
                onClick={dismissNotice}
                className="hb-press ml-2 text-[12px] cursor-pointer"
                style={{ background: 'none', border: 'none', color: 'var(--copper)' }}
              >
                Got it
              </button>
            </div>
          </ArriveOnce>
        )}
        {uploadError && (
          <ArriveOnce
            className="mb-4 rounded-lg px-4 py-3 text-sm text-center"
            style={{ background: 'var(--err-bg)', border: '1px solid var(--err-border)', color: 'var(--err-strong)' }}
          >
            <div data-testid="upload-error" aria-live="polite">{uploadError}</div>
          </ArriveOnce>
        )}

        {/* Verdict headline. The report's parts arrive staggered ONCE, when the
            report itself is new: keyed on the run, so clicking a net on the map
            does not re-run the entry. */}
        <StaggerItem index={0}>
        <div
          data-testid="report-verdict"
          className="rounded-xl px-4 py-3.5"
          style={{ border: `1px solid ${verdictBorder}`, background: verdictBg, fontSize: 15.5 }}
        >
          {reportVerdictHeadline(r)}
          <div
            className="text-xs mt-1.5 tnum"
            data-testid="report-inventory"
            style={{ color: 'var(--silk-dim)' }}
          >
            {(r.board_name || r.file_name)} · {r.num_components}{' '}
            {r.num_components === 1 ? 'part' : 'parts'} · {r.num_nets}{' '}
            {r.num_nets === 1 ? 'net' : 'nets'}
          </div>
        </div>
        </StaggerItem>

        {!restoredFrom && (
          <div
            data-testid="static-next-step"
            className="mt-3 rounded-lg px-4 py-2.5 text-[13px] leading-relaxed"
            style={{ border: '1px solid var(--hairline)', borderLeft: '4px solid var(--copper)', background: 'var(--surface)', color: 'var(--silk-dim)' }}
          >
            <b style={{ color: 'var(--copper-hi)', fontWeight: 600 }}>Next:</b>{' '}
            turn this report into repeatable pass/fail rules and CI.
            This static report does not prove powered behavior, brownout, overheating, or
            firmware timing; add firmware and run those checks before treating them as verified.
            {boardFile && (
              <> Terminal scaffold:{' '}
                <code className="hb-inline break-all">hauksbee-ci init {boardFile.name}</code>.
              </>
            )}
            <div className="mt-2">
              <button
                type="button"
                data-testid="open-checks-next"
                onClick={onOpenChecks}
                className="hb-btn-primary hb-press px-3 py-1.5 text-[12px]"
              >
                Set up checks
              </button>
            </div>
          </div>
        )}

        {/* A report that came out of storage rather than out of a run. It says
            so for as long as it is on screen, and it says which actions are
            unavailable, because everything that needs the board's bytes (a
            re-run, a checks run, a live launch) cannot work from a saved report
            and the buttons for them are gone rather than broken. */}
        {restoredFrom && (
          <div
            data-testid="restored-notice"
            className="mt-3 rounded-lg px-4 py-3 text-[13px] leading-relaxed"
            style={{ background: 'var(--warn-bg)', border: '1px solid var(--warn-border)', color: 'var(--silk)' }}
          >
            <span className="text-[10px] font-bold tracking-widest uppercase block mb-1" style={{ color: 'var(--warn-strong)' }}>
              Restored from a saved session
            </span>
            This is the report from{' '}
            <b style={{ fontWeight: 600 }}>{restoredFrom.sessionName}</b>, kept in this browser.
            The findings, the bind table and your composed checks are all here and can be
            exported.{' '}
            {restoredFrom.firmwareName || restoredFrom.schematicName
              ? <>The board file and its companion files ({[restoredFrom.firmwareName, restoredFrom.schematicName].filter(Boolean).join(', ')}) are not: </>
              : 'The board file itself is not: '}
            running the checks again, or driving it live, needs{' '}
            <span style={{ fontFamily: 'var(--font-mono)' }}>{restoredFrom.boardName}</span> dropped
            once more.
            <div className="mt-2.5">
              <label
                htmlFor="board-file"
                data-testid="restored-redrop"
                role="button"
                tabIndex={0}
                onKeyDown={e => {
                  if (e.key === 'Enter' || e.key === ' ') {
                    e.preventDefault()
                    document.getElementById('board-file')?.click()
                  }
                }}
                className="hb-btn-primary hb-press inline-flex items-center px-3 text-[12px] cursor-pointer"
                style={{ height: 30 }}
              >
                Drop the board again
              </label>
            </div>
          </div>
        )}

        {/* No live capability registered: the CLI hint remains (the header's
            primary action covers the launch/reconnect cases). */}
        {liveMode === 'none' && (
          <div
            data-testid="run-it-hint"
            className="mt-3 rounded-lg px-4 py-3 text-xs"
            style={{ border: '1px solid var(--hairline)', background: 'var(--surface)', color: 'var(--silk-dim)' }}
          >
            <div>To bring this board to life (live scope, 2D/3D view, transport controls) run:</div>
            <div className="mt-1.5 flex items-center flex-wrap">
              <code className="hb-code" style={{ padding: '2px 6px', fontSize: 11 }}>
                {runCommand}
              </code>
              <CopyButton text={runCommand} />
            </div>
          </div>
        )}

        {/* Bind-honesty line. The verdict above is the page's one accent
            surface; this keeps its amber and its place above the fold, but as a
            single row under the verdict rather than a second shouting box. */}
        {bindOpen && (
          <div
            className="mt-2 flex items-start gap-2 px-1 text-sm"
            style={{ color: 'var(--warn-strong)' }}
          >
            <span className="shrink-0" style={{ display: 'inline-flex', marginTop: 3 }}>
              <WarningIcon size={14} />
            </span>
            <span style={{ color: 'var(--silk-dim)' }}>
              <span style={{ color: 'var(--warn-strong)' }}>
                {r.bind!.active_path_unresolved!.join(', ')}
              </span>{' '}
              could not be bound or are left open on the live circuit. Analog / AC / thermal
              results on their nets are not trustworthy.
            </span>
          </div>
        )}

        {/* Everything this report can become. Below the verdict AND below every
            line that qualifies it (the restored-session caveat, the unbound-parts
            warning): those say how much to trust what is about to be exported,
            so they are not something to read after the download button. */}
        <div className="mt-3">
          <ExportMenu
            report={r}
            boardLabel={boardLabel}
            firmwareName={firmwareFile?.name ?? restoredFrom?.firmwareName ?? null}
            analyzedAt={session.analyzedAt}
            engineVersion={engineVersion}
            spec={spec}
            checks={checks}
            sessionName={sessionName}
            restored={restoredFrom !== null}
          />
        </div>


        {/* Directly under the line that says a model is missing: the offer to
            draft one. This is the moment the user learns they need it, and the
            only moment they have the part number and the datasheet in mind. */}
        {!restoredFrom && (
          <>
            <DatasheetExtract
              openParts={r.bind?.open_parts ?? []}
              onSaved={session.reanalyzeCurrent}
            />
            <div className="mt-3">
              <WritePart
                onSaved={session.reanalyzeCurrent}
                suggested={r.bind?.open_parts?.find(part => !part.bound)}
              />
            </div>
          </>
        )}

        {/* Top-level honesty notes. The bind-role note restates exactly what
            the amber unresolved-parts line above already says (the JSON carries
            both for CLI parity), so render it once, keeping the stronger one. */}
        {(r.notes || []).filter(n => !(bindOpen && n.kind === 'bind_role')).map((n, i) => (
          <div
            key={i}
            className="mt-3 rounded-lg px-4 py-2.5"
            style={{ border: '1px solid var(--hairline)', borderLeft: '4px solid var(--note-accent)', background: 'var(--surface)' }}
          >
            <span className="text-[10px] font-bold tracking-widest uppercase" style={{ color: 'var(--note)' }}>Note</span>
            <div className="text-sm mt-0.5" style={{ color: 'var(--silk)' }}>{n.message}</div>
          </div>
        ))}

        {r.import_diagnostics && (
          <ImportDiagnosticsPanel
            diagnostics={r.import_diagnostics}
            overlay={importOverlay}
            selectedNet={selectedNet}
            onToggleOverlay={() => setImportOverlay(value => !value)}
            onLocate={locate}
            onInspectNet={inspectImportNet}
          />
        )}

        {((r.inventory?.length ?? 0) > 0 || (r.assumptions?.length ?? 0) > 0 || (r.evidence?.length ?? 0) > 0) && (
          <details
            data-testid="evidence-panel"
            open={evidenceSummary.undermined > 0}
            className="mt-3 rounded-lg px-4 py-3"
            style={{
              border: `1px solid ${evidenceSummary.undermined > 0 ? 'var(--warn-border)' : 'var(--hairline)'}`,
              background: 'var(--surface)',
            }}
          >
            <summary className="cursor-pointer text-sm font-semibold" style={{ color: 'var(--silk)' }}>
              Evidence &amp; limitations
              <span className="ml-2 text-[11px] font-normal tnum" style={{ color: 'var(--silk-dim)' }}>
                {evidenceSummary.clean} fully supported · {evidenceSummary.qualified} supported with limitations · {evidenceSummary.undermined} invalid
              </span>
            </summary>
            <p className="mt-2 text-[12px] leading-relaxed" style={{ color: 'var(--silk-dim)' }}>
              These are the engine's derived evidence statuses. An invalid assertion is not
              entitled to a pass/fail verdict until the limitation below is closed.
            </p>
            {(r.inventory?.length ?? 0) > 0 && (
              <div className="mt-3 text-[12px]" style={{ color: 'var(--silk-dim)' }}>
                <div className="font-semibold" style={{ color: 'var(--silk)' }}>Input artifacts</div>
                {(r.inventory ?? []).map((artifact, index) => (
                  <div key={`${index}:${artifact.path}`} className="mt-1 grid gap-x-2 sm:grid-cols-[minmax(0,1fr)_auto]">
                    <span className="truncate" title={artifact.path}>{artifact.path}</span>
                    <span className="tnum" style={{ fontFamily: 'var(--font-mono)' }}>
                      {artifact.sha256 ? `sha256:${artifact.sha256.slice(0, 12)}…` : 'digest unavailable'}
                    </span>
                  </div>
                ))}
              </div>
            )}
            {(r.assumptions ?? []).map(assumption => (
              <div
                key={assumption.id}
                className="mt-2 rounded-md px-3 py-2.5 text-[12px] leading-relaxed"
                style={{ border: '1px solid var(--hairline)', background: 'var(--canvas)' }}
              >
                <div className="text-[10px] font-semibold" style={{ color: 'var(--warn-strong)', fontFamily: 'var(--font-mono)' }}>
                  {assumption.id}
                </div>
                <div className="mt-1 font-semibold" style={{ color: 'var(--silk)' }}>{assumption.statement}</div>
                <div className="mt-1" style={{ color: 'var(--silk-dim)' }}><b>Why:</b> {assumption.because}</div>
                <div style={{ color: 'var(--silk-dim)' }}><b>Effect:</b> {assumption.consequence}</div>
                <div style={{ color: 'var(--silk-dim)' }}><b>What closes it:</b> {assumption.replacement}</div>
              </div>
            ))}
            {(r.evidence ?? []).filter(map => map.status !== 'clean').length > 0 && (
              <div className="mt-3 text-[12px]" style={{ color: 'var(--silk-dim)' }}>
                <div className="font-semibold" style={{ color: 'var(--silk)' }}>Affected assertions</div>
                {(r.evidence ?? []).filter(map => map.status !== 'clean').slice(0, 20).map((map, index) => (
                  <div key={`${index}:${map.assertion}:${map.status}`} className="mt-1 flex gap-2">
                    <span style={{ color: map.status === 'undermined' ? 'var(--warn-strong)' : 'var(--note)' }}>{map.status}</span>
                    <span>{map.assertion}</span>
                  </div>
                ))}
                {evidenceSummary.caveated > 20 && (
                  <div className="mt-1">…and {evidenceSummary.caveated - 20} more in the JSON export.</div>
                )}
              </div>
            )}
          </details>
        )}

        {/* Board map: the real renderer (pads, outline, pan/zoom, layers)
            whenever the uploaded file is KiCad layout text; the dot map only
            as the fallback for formats the client cannot draw. */}
        {boardUrl ? (
          <section className="mt-6">
            <div
              ref={mapRef}
              className={mapFullscreen ? 'overflow-hidden' : 'rounded-xl overflow-hidden'}
              style={mapFullscreen
                ? {
                    position: 'fixed', inset: 0, zIndex: 60,
                    background: 'var(--instrument)',
                  }
                : {
                    height: 'clamp(420px, 52vh, 620px)',
                    position: 'relative',
                    border: '1px solid var(--hairline)',
                    boxShadow: 'var(--shadow-card)',
                  }}
            >
              <BoardViewer
                boardFile={boardUrl}
                frame={null}
                selectedNet={selectedNet}
                netOptions={r.nets}
                // The map sits inside a scrolling report, so it must not eat
                // the page wheel; it zooms once clicked, or with ctrl/cmd held.
                wheelMode="capture-on-focus"
                partCount={r.num_components}
                focusPoint={focusPoint}
                importMarkers={importMarkers}
                onViewModeChange={setViewerMode}
                fullscreen={mapFullscreen}
                onToggleFullscreen={() => setMapFullscreen(v => !v)}
                onNetClick={setSelectedNet}
                onFootprintClick={fp => setSelectedComponent({
                  ref: fp.ref, value: fp.value, lib_id: fp.lib_id,
                  padNet: fp.padNet, padNets: fp.padNets,
                })}
                onEmptyBoard={onEmptyBoard}
              />
              {/* Floating selection card, same language as the live sim. */}
              {(selectedNet || selectedComponent) && (
                // A strip that starts below the viewer toolbar and ends at the
                // bottom of the map. The card sits at its foot and grows up
                // into it, so a part with many nets scrolls inside the card
                // instead of sliding under the 2D/3D and Fit controls.
                <div
                  className="absolute left-3 z-10 flex items-end pointer-events-none"
                  style={{ top: TOOLBAR_CLEARANCE, bottom: 12 }}
                >
                  <div className="pointer-events-auto" style={{ maxHeight: '100%', display: 'flex' }}>
                  <SelectionCard
                    net={selectedNet}
                    component={selectedComponent}
                    boundKind={selectedComponent ? r.component_kinds?.[selectedComponent.ref] ?? null : null}
                    onQueueCheck={onQueueCheck}
                    onClose={() => { setSelectedNet(null); setSelectedComponent(null) }}
                    onPickNet={setSelectedNet}
                  />
                  </div>
                </div>
              )}
            </div>
            <div className="mt-1.5 text-[11px]" style={{ color: 'var(--silk-faint)' }}>
              {viewerMode === '3d'
                ? 'Drag to orbit · scroll to zoom · shift-drag to pan · switch to 2D to select traces and parts'
                : 'Click the map (or hold ctrl) to zoom · drag to pan · hover a trace to see its net · click a trace or a part to start a check on it'}
            </div>
          </section>
        ) : r.components?.length > 0 ? (
          <section className="mt-6" ref={mapRef}>
            <h2 className="text-[11px] font-bold tracking-widest uppercase mb-2" style={{ color: 'var(--silk-faint)' }}>
              Board map (2D)
            </h2>
            <BoardMap
              components={r.components}
              importDiagnostics={r.import_diagnostics ?? undefined}
              showImportOverlay={importOverlay}
              selectedNet={selectedNet}
            />
          </section>
        ) : null}

        {/* Check sections. Staggered arrival, capped (see ../motion/tokens): a
            report with fourteen sections must become readable in a quarter of a
            second, not walk down the page. The stagger index starts at 1
            because the verdict above is index 0, so the whole report reads as
            one arrival rather than two. */}
        {r.sections.map((s, i) => (
          <StaggerItem key={i} index={i + 1}>
            <SectionBlock section={s} onLocate={boardUrl ? locate : undefined} />
          </StaggerItem>
        ))}

        {r.refusal && (
          <section
            className="mt-7 rounded-lg px-4 py-3"
            data-testid="analysis-refusal-contract"
            style={{ border: '1px solid var(--warn-border)', borderLeft: '4px solid var(--warn)', background: 'var(--warn-bg)' }}
          >
            <h2 className="text-[11px] font-bold tracking-widest uppercase mb-2" style={{ color: 'var(--warn-strong)' }}>
              Analysis could not make this claim
            </h2>
            {refusalLines(r.refusal).map(([label, value]) => (
              <div key={label} className="text-sm mt-1" style={{ color: 'var(--silk)' }}>
                <b style={{ color: 'var(--silk-dim)', fontWeight: 600 }}>{label}:</b>{' '}{value}
              </div>
            ))}
          </section>
        )}

        {/* Firmware co-sim */}
        {r.cosim && (
          <CosimBlock
            cosim={r.cosim}
            timingRefusals={uncoveredTimingRefusals(r.cosim.timing_refusals, r.refusal)}
            liveAvailable={liveMode !== 'none'}
            onDriveLive={onDriveLive}
            simMounted={simMounted}
          />
        )}

        {/* The board file is still in hand, so firmware can be added or
            swapped without starting the board over. */}
        {boardFile && !busy && (
          <div className="mt-5">
            <FirmwareJack
              firmware={firmwareFile}
              placement="report"
              onFile={handleFirmware}
              onClear={clearFirmware}
              locked={!!busy}
              cosimRan={r.cosim?.ran}
            />
            <SchematicJack
              schematic={schematicFile}
              onFile={handleSchematic}
              onClear={clearSchematic}
              locked={!!busy}
            />
          </div>
        )}
      </div>
    </div>
  )
}

/** Pan-the-map callback for findings that carry board coordinates. */
type LocateFn = (x: number, y: number, label: string) => void

function SectionBlock({ section: s, onLocate }: { section: WebSection; onLocate?: LocateFn }) {
  const groups = groupFindings(s.findings)
  return (
    <section className="mt-7">
      <h2 className="text-[11px] font-bold tracking-widest uppercase mb-1" style={{ color: 'var(--silk-faint)' }}>
        {s.title}
      </h2>
      <div className="text-sm mb-2" style={{ color: 'var(--silk-dim)' }}>{s.verdict}</div>
      {groups.map((g, i) =>
        g.items.length === 1
          ? (
            <FindingCard
              key={i}
              finding={{ level: g.level, what: g.items[0].what, why: g.why, fix: g.fix, x: g.items[0].x, y: g.items[0].y }}
              onLocate={onLocate}
            />
          )
          : <GroupedFindingCard key={i} group={g} onLocate={onLocate} />
      )}
      {(s.heads_up || []).map((h, i) => <HeadsUpCard key={i} note={h} />)}
    </section>
  )
}

// A heads-up note with the finding's what / why / what-to-do gloss. `why`/`fix`
// render only when present (self-contained notes carry just `what`).
function HeadsUpCard({ note: h }: { note: WebHeadsUp }) {
  return (
    <div
      className="rounded-lg px-4 py-2.5 mb-2"
      style={{ border: '1px solid var(--hairline)', borderLeft: '4px solid var(--copper)', background: 'var(--surface)' }}
    >
      <span className="text-[10px] font-bold tracking-widest uppercase" style={{ color: 'var(--copper)' }}>Heads up</span>
      <div className="text-sm mt-0.5" style={{ color: 'var(--silk)' }}>{h.what}</div>
      {h.why && (
        <div className="text-sm mt-1" style={{ color: 'var(--silk)' }}>
          <b style={{ color: 'var(--copper-hi)', fontWeight: 600 }}>Why it matters:</b> {h.why}
        </div>
      )}
      {h.fix && (
        <div className="text-sm mt-0.5" style={{ color: 'var(--silk)' }}>
          <b style={{ color: 'var(--copper-hi)', fontWeight: 600 }}>What to do:</b> {h.fix}
        </div>
      )}
    </div>
  )
}

// A collapsed group of same-shaped findings: the shared level + explanation are
// shown once, and the individual items live inside an expandable list so a long
// run (e.g. 128 clearance warnings) never walls the page, yet hides nothing.
function GroupedFindingCard({ group: g, onLocate }: { group: FindingGroup; onLocate?: LocateFn }) {
  const accent = LEVEL_ACCENT[g.level] ?? 'var(--note-accent)'
  const tagColor = LEVEL_TEXT[g.level] ?? 'var(--note)'
  const n = g.items.length
  return (
    <div
      data-testid="grouped-finding"
      className="rounded-lg px-4 py-3 mb-2"
      style={{ border: '1px solid var(--hairline)', borderLeft: `4px solid ${accent}`, background: 'var(--surface)' }}
    >
      <span className="text-[10px] font-bold tracking-widest uppercase" style={{ color: tagColor }}>
        {g.level} · {n} similar
      </span>
      <div className="font-semibold text-sm mt-1 mb-1.5">
        {n} similar findings, same cause, listed once below.
      </div>
      <details className="mb-1.5">
        <summary className="text-sm cursor-pointer" style={{ color: 'var(--silk-dim)' }}>
          Show all {n}
        </summary>
        <ul className="mt-1.5 pl-4 text-sm" style={{ color: 'var(--silk)', listStyleType: 'disc' }}>
          {g.items.map((it, i) => (
            <li key={i} className="my-0.5">
              {it.what}
              {onLocate && it.x !== undefined && it.y !== undefined && (
                <button
                  type="button"
                  data-testid="finding-locate"
                  onClick={() => onLocate(it.x!, it.y!, it.what)}
                  /* Named for the finding it locates. Fifty buttons reading
                     "show on board" is one name fifty times over in the
                     accessibility tree, which is fifty controls a screen
                     reader cannot tell apart. */
                  aria-label={`Show on board: ${it.what}`}
                  className="hb-press ml-2 cursor-pointer text-[11px]"
                  style={{ background: 'none', border: 'none', padding: 0, color: 'var(--copper-hi)', textDecoration: 'underline', textDecorationColor: 'var(--copper-deep)' }}
                >
                  show on board
                </button>
              )}
            </li>
          ))}
        </ul>
      </details>
      {g.why && (
        <div className="text-sm my-0.5">
          <b style={{ color: 'var(--silk-dim)', fontWeight: 600 }}>Why it matters:</b> {g.why}
        </div>
      )}
      {g.fix && (
        <div className="text-sm my-0.5">
          <b style={{ color: 'var(--silk-dim)', fontWeight: 600 }}>What to do:</b> {g.fix}
        </div>
      )}
    </div>
  )
}

function FindingCard({ finding: f, onLocate }: { finding: WebFinding; onLocate?: LocateFn }) {
  const accent = LEVEL_ACCENT[f.level] ?? 'var(--note-accent)'
  const tagColor = LEVEL_TEXT[f.level] ?? 'var(--note)'
  const locatable = onLocate !== undefined && f.x !== undefined && f.y !== undefined
  const locate = locatable ? () => onLocate!(f.x!, f.y!, f.what) : undefined
  return (
    <div
      data-testid="finding-card"
      onClick={locate}
      className="rounded-lg px-4 py-3 mb-2"
      style={{
        border: '1px solid var(--hairline)', borderLeft: `4px solid ${accent}`,
        background: 'var(--surface)', cursor: locatable ? 'pointer' : undefined,
      }}
    >
      <span className="text-[10px] font-bold tracking-widest uppercase" style={{ color: tagColor }}>
        {f.level}
      </span>
      {locatable && (
        <button
          type="button"
          data-testid="finding-locate"
          onClick={e => { e.stopPropagation(); locate!() }}
          aria-label={`Show on board: ${f.what}`}
          className="hb-press ml-2 cursor-pointer text-[11px]"
          style={{ background: 'none', border: 'none', padding: 0, color: 'var(--copper-hi)', textDecoration: 'underline', textDecorationColor: 'var(--copper-deep)' }}
        >
          show on board
        </button>
      )}
      <div className="font-semibold text-sm mt-1 mb-1.5">{f.what}</div>
      <div className="text-sm my-0.5">
        <b style={{ color: 'var(--silk-dim)', fontWeight: 600 }}>Why it matters:</b> {f.why}
      </div>
      <div className="text-sm my-0.5">
        <b style={{ color: 'var(--silk-dim)', fontWeight: 600 }}>What to do:</b> {f.fix}
      </div>
    </div>
  )
}

function CosimBlock({ cosim: c, timingRefusals, liveAvailable, onDriveLive, simMounted }: {
  cosim: WebCosimSection
  timingRefusals: string[]
  liveAvailable: boolean
  onDriveLive: () => void
  simMounted: boolean
}) {
  return (
    <section className="mt-7" data-testid="cosim-section">
      <h2 className="text-[11px] font-bold tracking-widest uppercase mb-1" style={{ color: 'var(--silk-faint)' }}>
        Firmware co-sim
      </h2>
      {c.ran ? (
        <>
          <div className="text-sm mb-2" style={{ color: 'var(--silk-dim)' }}>
            Ran the firmware for {(c.seconds_simulated || 0).toFixed(3)}s on the board's microcontroller.
          </div>
          {(c.timing_coverage?.length ?? 0) > 0 && (
            <details className="rounded-lg px-3 py-2 mb-2 text-xs" style={{ border: '1px solid var(--hairline)', background: 'var(--surface-2)', color: 'var(--silk-dim)' }}>
              <summary className="cursor-pointer font-semibold" style={{ color: 'var(--silk)' }}>Timing coverage</summary>
              <div className="mt-1.5">{c.timing_coverage!.map(row => <div key={`${row.mcu_ref}:${row.backend}`}>{timingCoverageLine(row)}</div>)}</div>
            </details>
          )}
          {timingRefusals.length > 0 && (
            <div className="rounded-lg px-4 py-2.5 mb-2" style={{ border: '1px solid var(--err-border)', borderLeft: '4px solid var(--err)', background: 'var(--err-bg)' }}>
              <span className="text-[10px] font-bold tracking-widest uppercase" style={{ color: 'var(--err-strong)' }}>TIMING INVALID</span>
              {timingRefusals.map((line, i) => <div key={i} className="text-sm mt-1">{line}</div>)}
            </div>
          )}
          {(c.fallback_windows?.length ?? 0) > 0 && (
            <details className="rounded-lg px-3 py-2 mb-2 text-xs" style={{ border: '1px solid var(--warn-border)', background: 'var(--warn-bg)', color: 'var(--silk-dim)' }}>
              <summary className="cursor-pointer font-semibold" style={{ color: 'var(--warn-strong)' }}>Fallback-qualified windows</summary>
              <div className="mt-1.5">{c.fallback_windows!.map((window, i) => <div key={i}>{fallbackWindowLine(window)}</div>)}</div>
            </details>
          )}
          {c.error_budget && (
            <details className="rounded-lg px-3 py-2 mb-2 text-xs" style={{ border: '1px solid var(--hairline)', background: 'var(--surface-2)', color: 'var(--silk-dim)' }}>
              <summary className="cursor-pointer font-semibold" style={{ color: 'var(--silk)' }}>Numerical qualification</summary>
              <div className="mt-1.5" data-testid="cosim-error-budget">
                {summarizeErrorBudget(c.error_budget).map(row => <div key={row}>{row}</div>)}
              </div>
            </details>
          )}
          {(!c.findings || c.findings.length === 0) && (
            <div
              className="rounded-lg px-4 py-2.5 mb-2"
              style={{ border: '1px solid var(--hairline)', borderLeft: '4px solid var(--note-accent)', background: 'var(--surface)' }}
            >
              <span className="text-[10px] font-bold tracking-widest uppercase" style={{ color: 'var(--note)' }}>Note</span>
              <div className="text-sm mt-0.5" style={{ color: 'var(--silk)' }}>
                No electrical-stress faults during the run.
              </div>
            </div>
          )}
          {(c.findings || []).map((f, i) => <FindingCard key={i} finding={f} />)}
          {c.uart_output && (
            <>
              <div className="text-sm mb-1"><b style={{ color: 'var(--silk-dim)', fontWeight: 600 }}>UART output:</b></div>
              <pre
                className="rounded-lg px-3 py-2.5 mb-2 text-xs overflow-x-auto whitespace-pre-wrap"
                style={{
                  background: 'var(--instrument)',
                  border: '1px solid var(--instrument-edge)',
                  color: 'var(--instrument-text)',
                  fontFamily: 'var(--font-mono)',
                }}
              >
                {c.uart_output}
              </pre>
            </>
          )}
          {c.gpio_nets && c.gpio_nets.length > 0 && (
            <table className="w-full text-xs mt-1" style={{ borderCollapse: 'collapse' }}>
              <thead>
                <tr>
                  {['Net', 'Volts', 'Activity'].map(h => (
                    <th
                      key={h}
                      className="text-left px-2 py-1 font-semibold"
                      style={{ color: 'var(--silk-dim)', borderBottom: '1px solid var(--hairline)' }}
                    >
                      {h}
                    </th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {c.gpio_nets.map((g, i) => (
                  <tr key={i}>
                    <td className="px-2 py-1" style={{ borderBottom: '1px solid var(--rule)' }}>{displayNet(g.name)}</td>
                    <td className="px-2 py-1 tnum" style={{ borderBottom: '1px solid var(--rule)', fontFamily: 'var(--font-mono)' }}>
                      {(g.volts || 0).toFixed(3)}
                    </td>
                    <td
                      className="px-2 py-1"
                      style={{ borderBottom: '1px solid var(--rule)', color: g.driven ? 'var(--ok)' : 'var(--silk-faint)' }}
                    >
                      {g.driven ? 'driven' : 'idle'}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
          {/* The story continues past the summary: what to do with a finished
              co-sim, so landing firmware is a beginning, not a dead end. */}
          <div
            data-testid="cosim-next"
            className="mt-3 rounded-lg px-4 py-2.5 text-[13px]"
            style={{ border: '1px solid var(--hairline)', borderLeft: '4px solid var(--copper)', background: 'var(--surface)', color: 'var(--silk-dim)' }}
          >
            <b style={{ color: 'var(--copper-hi)', fontWeight: 600 }}>Where to go from here:</b>{' '}
            {liveAvailable ? (
              <>
                <button
                  type="button"
                  onClick={onDriveLive}
                  className="hb-press cursor-pointer"
                  style={{
                    background: 'none', border: 'none', padding: 0, color: 'var(--copper-hi)',
                    textDecoration: 'underline', textDecorationColor: 'var(--copper-deep)', fontSize: 13,
                  }}
                >
                  {simMounted ? 'open the live sim' : 'drive it live'}
                </button>
                {' '}to boot this firmware interactively (scope, serial console, sliders), or{' '}
              </>
            ) : ''}
            turn what you just saw into repeatable checks in the Checks view; a UART print, a
            blink, a rail that must hold. The same spec then runs in CI on every push.
          </div>
        </>
      ) : (
        (c.findings && c.findings.length > 0 ? c.findings : [{
          level: 'note', what: 'Co-sim not available for this board.', why: '', fix: '',
        }]).map((f, i) => (
          <div
            key={i}
            className="rounded-lg px-4 py-2.5 mb-2"
            style={{ border: '1px solid var(--hairline)', borderLeft: '4px solid var(--note-accent)', background: 'var(--surface)' }}
          >
            <span className="text-[10px] font-bold tracking-widest uppercase" style={{ color: 'var(--note)' }}>
              Co-sim not available
            </span>
            <div className="text-sm mt-0.5" style={{ color: 'var(--silk)' }}>{`${f.what} ${f.why}`.trim()}</div>
          </div>
        ))
      )}
    </section>
  )
}

// Simple 2D footprint dot map, drawn from the report's component positions
// (board mm), for formats the client-side renderer cannot draw. Sits on the
// instrument surface and follows the theme via the --map-* tokens.
function BoardMap({
  components, importDiagnostics, showImportOverlay, selectedNet,
}: {
  components: WebComponent[]
  importDiagnostics?: WebImportDiagnostics
  showImportOverlay?: boolean
  selectedNet?: string | null
}) {
  const canvasRef = useRef<HTMLCanvasElement>(null)

  useEffect(() => {
    const cv = canvasRef.current
    if (!cv) return
    const ctx = cv.getContext('2d')
    if (!ctx) return
    const draw = () => {
      const W = cv.width, H = cv.height, pad = 28
      let minX = Infinity, minY = Infinity, maxX = -Infinity, maxY = -Infinity
      for (const c of components) {
        minX = Math.min(minX, c.x); minY = Math.min(minY, c.y)
        maxX = Math.max(maxX, c.x); maxY = Math.max(maxY, c.y)
      }
      const spanX = Math.max(1e-6, maxX - minX)
      const spanY = Math.max(1e-6, maxY - minY)
      const scale = Math.min((W - 2 * pad) / spanX, (H - 2 * pad) / spanY)
      ctx.clearRect(0, 0, W, H)
      // Labels only while they can be read: past a few hundred parts every
      // reference overlaps its neighbours and the map collapses into a grey
      // smear. Dense boards get clean position dots; the real geometry lives in
      // the BoardViewer path.
      const drawLabels = components.length <= 300
      const dotR = components.length > 1000 ? 1.5 : 3
      const importStatus = new Map((importDiagnostics?.objects ?? []).map(object => [object.id, object.status]))
      const selectedObjects = new Set((importDiagnostics?.objects ?? [])
        .filter(object => selectedNet && object.nets?.includes(selectedNet))
        .map(object => object.id))
      for (const c of components) {
        const x = pad + (c.x - minX) * scale
        const y = pad + (c.y - minY) * scale
        const status = showImportOverlay ? importStatus.get(c.reference) : undefined
        ctx.fillStyle = status === 'recovered' ? '#22c55e' : status === 'partial' ? '#f59e0b' : cssToken('--map-dot')
        ctx.beginPath(); ctx.arc(x, y, status ? dotR + 2 : dotR, 0, Math.PI * 2); ctx.fill()
        if (selectedObjects.has(c.reference)) {
          ctx.strokeStyle = cssToken('--copper-hi')
          ctx.lineWidth = 2
          ctx.beginPath(); ctx.arc(x, y, dotR + 6, 0, Math.PI * 2); ctx.stroke()
        }
        if (drawLabels) {
          ctx.fillStyle = cssToken('--map-label'); ctx.font = '10px sans-serif'
          ctx.fillText(c.reference, x + 5, y + 3)
        }
      }
      if (!drawLabels) {
        ctx.fillStyle = cssToken('--map-note'); ctx.font = '11px sans-serif'
        ctx.fillText(`${components.length} parts (labels hidden at this density)`, pad, H - 10)
      }
    }
    draw()
    // Canvas pixels do not restyle themselves when the theme flips; redraw.
    return onThemeChange(draw)
  }, [components, importDiagnostics, selectedNet, showImportOverlay])

  return (
    <canvas
      ref={canvasRef}
      width={760}
      height={460}
      className="w-full rounded-lg block"
      style={{ background: 'var(--instrument)', border: '1px solid var(--instrument-edge)' }}
    />
  )
}
