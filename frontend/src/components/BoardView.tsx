import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import type { BoardRequest, ModelCoverageComponent } from '../types/report'
import { coverageFor, explainReport, modelsOnNet, padNetsOf, plural, reportView, stageWords } from '../lib/report-view'
import type { BoardSession } from '../hooks/useBoardSession'
import { WarningIcon } from './Icons'
import { BoardViewer, TOOLBAR_CLEARANCE } from './BoardViewer'
import { OpenSelectionsBanner, SelectionCard } from './SelectionCard'
import { AdditionalEvidencePanel, evidenceSlots } from './AdditionalEvidencePanel'
import { QueuedConstraintModal } from './ConstraintModal'
import type { ConstraintDraft } from './ConstraintEditor'
import { DatasheetExtract } from './DatasheetExtract'
import { WritePart } from './WritePart'
import { acceptedFormatsSentence, withoutEngineFormatList } from '../lib/board-formats'
import { StaggerItem } from '../motion'
import { ExportMenu } from './ExportMenu'
import type { SpecSnapshot } from '../hooks/useSessions'
import { Callout, CopyButton, Term, UploadBanners, useGlossary } from './ui'
import { FixFirstCard, NoteBlock, PreLayoutStrip, SectionBlock } from './report/Findings'
import { centerInScroller } from '../lib/scroll'
import { EvidencePanel, ImportDiagnosticsPanel, ModelCoveragePanel, ReportPanel } from './report/Panels'
import { CosimBlock } from './report/CosimBlock'
import { NextStep } from './report/NextStep'
import { ResultPanel } from './report/ResultPanel'
import { ResultLinkContext } from './report/ResultLinks'
import type { ResultLinkHandlers } from './report/ResultLinks'
import { overlayHasContent, resultOverlay, vocabulary } from '../lib/result-links'
import type { ResultEntity } from '../lib/result-links'
import { cliCommand } from '../lib/result-export'
import { BoardMap } from './report/BoardMap'

// The Board view with a report in hand: the viewer as the hero surface (with
// its toolbar and layers panel), the plain-language verdict, and the findings.
// Landing/UploadView owns getting a board in; everything here owns saying what
// came back and letting the board be explored. The findings, the qualification
// panels, the co-sim block and the fallback dot map live in ./report.

/** The file-picker label the dead-end paths offer, styled as a button. */
function DropBoardAgain({ testId, label, height }: { testId: string; label: string; height: number }) {
  return (
    <label
      htmlFor="board-file"
      data-testid={testId}
      role="button"
      tabIndex={0}
      onKeyDown={e => {
        if (e.key === 'Enter' || e.key === ' ') {
          e.preventDefault()
          document.getElementById('board-file')?.click()
        }
      }}
      className="hb-btn-primary hb-press inline-flex items-center px-3.5 text-[13px] cursor-pointer"
      style={{ height }}
    >
      {label}
    </label>
  )
}

export function BoardView({
  session, onQueue, onOpenChecks,
  onDriveLive, simMounted, engineVersion, spec, checks, sessionName, onOpenSettings,
}: {
  session: BoardSession
  /** A click on the map asked the checks builder for something. */
  onQueue: (request: BoardRequest) => void
  onOpenChecks: () => void
  onDriveLive: () => void
  simMounted: boolean
  /** The hauksbee that produced the report, for the exported file's provenance. */
  engineVersion: string | null
  /** The spec the Checks pane composed, offered alongside the report. */
  spec: SpecSnapshot | null
  checks: { passed: number; failed: number; invalid: number } | null
  sessionName: string | null
  /** Opens the Settings view, from the datasheet extraction panel's "Change in
   *  Settings" link once a backend is configured there. */
  onOpenSettings?: () => void
}) {
  const r = session.report!
  // A part with variant cards waiting on a pick is the picker's to settle, not
  // a datasheet draft's: the open-selections banner above already offers it.
  const awaitingPick = new Set(r.bind?.open_selections?.map(s => s.reference))
  const openParts = (r.bind?.open_parts ?? []).filter(p => !awaitingPick.has(p.reference))
  const {
    boardUrl, selectedNet, selectedComponent, setSelectedNet, setSelectedComponent,
    busy, uploadError, uploadNotice, dismissNotice, firmwareFile,
    boardFile, boardLabel, liveMode, onEmptyBoard, restoredFrom,
  } = session

  // Every hook lives ABOVE the unreadable-file branch below: a session that
  // goes from a refused file to a good one must not change the NUMBER of hooks
  // it renders.
  //
  // "Show on board": pan/zoom the map to a finding's board location and drop a
  // labeled marker there. Only wired when the real renderer is drawing (the dot
  // map has no camera to move).
  const [focusPoint, setFocusPoint] = useState<{ x: number; y: number; label: string; seq: number } | null>(null)
  // Expand-to-viewport for the map. Per-view and deliberately not persisted:
  // it is a "let me look at this properly" gesture, not a setting.
  const [mapFullscreen, setMapFullscreen] = useState(false)
  const [importOverlay, setImportOverlay] = useState(false)
  const [constraintDraft, setConstraintDraft] = useState<(Partial<ConstraintDraft> & Pick<ConstraintDraft, 'kind'>) | null>(null)
  const [authoringComponent, setAuthoringComponent] = useState<ModelCoverageComponent | null>(null)
  const [authoringSignal, setAuthoringSignal] = useState(0)
  // The two collapsed groups something else can open: the parts the run had to
  // guess at (the banner's link, an Extend click) and the evidence slots (the
  // Next card's "add firmware").
  const [guessOpen, setGuessOpen] = useState(false)
  const [evidenceOpen, setEvidenceOpen] = useState(false)
  const authoringRef = useRef<HTMLDivElement>(null)
  const guessRef = useRef<HTMLDivElement>(null)
  const focusSeq = useRef(0)
  const mapRef = useRef<HTMLDivElement>(null)
  const v = useMemo(() => reportView(r), [r])
  const explanation = useMemo(() => (r.ok ? explainReport(r) : null), [r])
  useGlossary(r.glossary)

  useEffect(() => {
    if (authoringSignal === 0) return
    requestAnimationFrame(() => centerInScroller(authoringRef.current))
  }, [authoringSignal])
  useEffect(() => setImportOverlay(false), [session.runEpoch])

  const locate = useCallback((x: number, y: number, label: string) => {
    focusSeq.current += 1
    setFocusPoint({ x, y, label, seq: focusSeq.current })
    centerInScroller(mapRef.current)
  }, [])
  const importMarkers = useMemo(() => (importOverlay ? v.importMarkers : []), [importOverlay, v])
  const inspectImportNet = useCallback((net: string) => {
    setImportOverlay(true)
    setSelectedNet(net)
    centerInScroller(mapRef.current)
  }, [setSelectedNet])
  const author = useCallback((component: ModelCoverageComponent) => {
    setAuthoringComponent(component)
    setGuessOpen(true)
    setAuthoringSignal(value => value + 1)
  }, [])

  // The result panel's reach into the board: what a link points at (hover),
  // what it framed last (click), and the result's own layer on the map.
  const [overlayOn, setOverlayOn] = useState(true)
  const [pointed, setPointed] = useState<ResultEntity | null>(null)
  const [frameTarget, setFrameTarget] = useState<{ refs: string[]; nets: string[]; seq: number } | null>(null)
  const frameSeq = useRef(0)
  const overlay = useMemo(() => resultOverlay(r, v), [r, v])
  const showGuessed = useCallback(() => {
    setGuessOpen(true)
    requestAnimationFrame(() => centerInScroller(guessRef.current))
  }, [])
  const resultLinks = useMemo<ResultLinkHandlers>(() => ({
    vocab: vocabulary(r),
    onPoint: setPointed,
    onGo: entity => {
      frameSeq.current += 1
      if (entity.kind === 'part') {
        const row = coverageFor(r.model_coverage, entity.ref)
        const padNets = row ? padNetsOf(row) : []
        setSelectedNet(null)
        setSelectedComponent({
          ref: entity.ref,
          value: row?.value ?? r.components?.find(c => c.reference === entity.ref)?.value ?? '',
          lib_id: row?.lib_id ?? '',
          padNet: padNets[0] ?? null,
          padNets,
        })
        setFrameTarget({ refs: [entity.ref], nets: [], seq: frameSeq.current })
      } else {
        setSelectedComponent(null)
        setSelectedNet(entity.net)
        setFrameTarget({ refs: [], nets: [entity.net], seq: frameSeq.current })
      }
      centerInScroller(mapRef.current)
    },
    onGiveModel: restoredFrom ? undefined : ref => {
      const row = coverageFor(r.model_coverage, ref)
      if (row) author(row)
      else showGuessed()
    },
    variantRefs: new Set((r.bind?.open_selections ?? []).map(s => s.reference)),
    onPickVariant: restoredFrom ? undefined : showGuessed,
    checkOffer: entity => {
      if (entity.kind === 'net') return { kind: 'voltage', net: entity.net, label: `net ${entity.net} must sit at a voltage` }
      const caps = r.component_assertions?.[entity.ref] ?? []
      if (caps.includes('max_current')) return { kind: 'max_current', ref: entity.ref, label: `${entity.ref} must stay under a current` }
      if (caps.includes('max_temp')) return { kind: 'max_temp', ref: entity.ref, label: `${entity.ref} must stay cool` }
      return null
    },
    onAddCheck: offer => setConstraintDraft({ kind: offer.kind, net: offer.net ?? '', ref: offer.ref ?? '' }),
  }), [r, restoredFrom, author, showGuessed, setSelectedNet, setSelectedComponent])
  const pointedRefs = useMemo(() => (pointed?.kind === 'part' ? [pointed.ref] : []), [pointed])
  const pointedNets = useMemo(() => (pointed?.kind === 'net' ? [pointed.net] : []), [pointed])

  if (!r.ok) {
    return (
      <div className="h-full overflow-y-auto view-enter">
        <div className="max-w-3xl mx-auto px-6 pt-8 pb-16">
          <Callout tone="err" testId="report-verdict" className="px-4 py-3.5">
            {r.error ? withoutEngineFormatList(r.error) : 'Could not read the file.'}
          </Callout>
          {r.import_failure && (
            <Callout
              tone="warn"
              testId="import-failure"
              className="mt-3"
              title={`Import stopped at ${r.import_failure.stage}`}
            >
              {r.import_failure.excerpt && (
                <pre className="mt-2 overflow-x-auto rounded px-3 py-2 text-[11px]" style={{ background: 'var(--canvas)', border: '1px solid var(--hairline)' }}>
                  {r.import_failure.excerpt}
                </pre>
              )}
              <div className="mt-2"><b>Suggested fix:</b> {r.import_failure.suggested_fix}</div>
            </Callout>
          )}
          {/* The dead end must not be dead: offer the retry inline instead of
              sending the user hunting for the header button. The accepted-format
              list is the app's one copy (lib/board-formats), because the engine's
              own refusal text omits formats it can in fact read. */}
          <div className="mt-4 flex flex-wrap items-center gap-3">
            <DropBoardAgain testId="try-another-file" label="Try another file" height={32} />
            <span className="text-[12px]" style={{ color: 'var(--silk-dim)' }}>
              accepted: {acceptedFormatsSentence()}
            </span>
          </div>
        </div>
      </div>
    )
  }

  // Everything the run had to assume about a part, in one place: the parts with
  // no model, the variants the board leaves open, and what each model covers.
  const selections = r.bind?.open_selections ?? []
  const coverageGaps = r.model_coverage?.components.filter(c => c.actionable_behavior_gap).length ?? 0
  const guessN = v.openParts.length + selections.length + coverageGaps
  const guessSummary = [
    v.openParts.length > 0 && `${v.openParts.length} left out of the circuit`,
    selections.length > 0 && `${plural(selections.length, 'variant')} not pinned`,
    coverageGaps > 0 && `${plural(coverageGaps, 'model')} with gaps`,
  ].filter(Boolean).join(' · ')
  const guessBody = (
    <div ref={guessRef}>
      <OpenSelectionsBanner
        selections={selections}
        onQueue={restoredFrom ? undefined : request => { onQueue(request); onOpenChecks() }}
      />

      {r.model_coverage && (
        <ModelCoveragePanel
          coverage={r.model_coverage}
          onSelect={component => {
            const padNets = padNetsOf(component)
            setSelectedNet(null)
            setSelectedComponent({
              ref: component.reference,
              value: component.value,
              lib_id: component.lib_id,
              padNet: padNets[0] ?? null,
              padNets,
            })
            const located = component.pins.find(pin => pin.position_mm)?.position_mm
            if (located) locate(located[0], located[1], `${component.reference}: ${stageWords(component.stage)}`)
          }}
          onAuthor={author}
        />
      )}

      {/* Directly under the line that says a model is missing: the offer to
          draft one. This is the moment the user learns they need it, and the
          only moment they have the part number and the datasheet in mind. */}
      {!restoredFrom && (
        <>
          <DatasheetExtract
            openParts={openParts}
            onSaved={session.reanalyzeCurrent}
            onOpenSettings={onOpenSettings}
          />
          <div className="mt-3" ref={authoringRef}>
            <WritePart
              onSaved={session.reanalyzeCurrent}
              suggested={authoringComponent ?? openParts.find(part => !part.bound)}
              openSignal={authoringSignal > 0 ? `${authoringComponent?.reference}:${authoringSignal}` : null}
              boardLabel={boardLabel ?? r.file_name}
            />
          </div>
        </>
      )}
    </div>
  )

  const runCommand = `hauksbee run ${boardLabel ?? r.file_name} --serve`

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
            Analyzing <span style={{ fontFamily: 'var(--font-mono)' }}>{busy.board}</span>
            {busy.firmware && <>{' + co-sim of '}<span style={{ fontFamily: 'var(--font-mono)' }}>{busy.firmware}</span></>}
            {' ...'}
          </div>
        )}
        <UploadBanners
          notice={uploadNotice}
          error={uploadError}
          onDismiss={dismissNotice}
          className="mb-4"
        />

        {/* The board result: verdict, coverage, the rows that qualify it, and
            what it can become (board overlay, export, CLI command). Its parts
            and nets are live links to the map below. The report's parts arrive
            staggered ONCE, when the report itself is new: keyed on the run, so
            clicking a net on the map does not re-run the entry. Everything this
            report can become sits in the panel's action bar: below every line
            that qualifies the verdict, since those say how much to trust what
            is about to be exported. */}
        <StaggerItem index={0}>
          <ResultLinkContext.Provider value={resultLinks}>
            <ResultPanel
              report={r}
              view={v}
              explanation={explanation}
              onLocate={boardUrl ? locate : undefined}
              overlay={boardUrl && overlayHasContent(overlay) ? { on: overlayOn, onToggle: () => setOverlayOn(on => !on) } : undefined}
              command={cliCommand({ board: boardLabel ?? r.file_name, firmware: firmwareFile?.name ?? restoredFrom?.firmwareName ?? null })}
              onOpenChecks={onOpenChecks}
              exportSlot={(
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
              )}
            />
          </ResultLinkContext.Provider>
        </StaggerItem>

        {v.preLayout && (
          <PreLayoutStrip notice={v.preLayout.notice} copper={v.preLayout.copper} onLocate={boardUrl ? locate : undefined} />
        )}
        {v.fixFirst && <FixFirstCard fixFirst={v.fixFirst} onLocate={boardUrl ? locate : undefined} />}

        {!restoredFrom && (
          <NextStep
            report={r}
            firmwareName={firmwareFile?.name ?? null}
            boardName={boardFile?.name ?? null}
            checks={checks}
            specFile={spec?.fileName ?? null}
            onQueue={onQueue}
            onOpenChecks={onOpenChecks}
            onOpenEvidence={() => setEvidenceOpen(true)}
          />
        )}

        {/* A report that came out of storage rather than out of a run. It says
            so for as long as it is on screen, and it says which actions are
            unavailable: everything that needs the board's bytes (a re-run, a
            checks run, a live launch) is gone rather than broken. */}
        {restoredFrom && (
          <Callout tone="warn" testId="restored-notice" className="mt-3" title="Restored from a saved session">
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
              <DropBoardAgain testId="restored-redrop" label="Drop the board again" height={30} />
            </div>
          </Callout>
        )}

        {/* No live capability registered: the CLI hint remains (the header's
            primary action covers the launch/reconnect cases). */}
        {liveMode === 'none' && (
          <div
            data-testid="run-it-hint"
            className="mt-3 rounded-lg px-4 py-3 text-xs"
            style={{ border: '1px solid var(--hairline)', background: 'var(--surface)', color: 'var(--silk-dim)' }}
          >
            <div>To bring this board to life (live scope, board view, transport controls) run:</div>
            <div className="mt-1.5 flex items-center flex-wrap">
              <code className="hb-code" style={{ padding: '2px 6px', fontSize: 11 }}>{runCommand}</code>
              <CopyButton text={runCommand} testId="copy-cli" />
            </div>
          </div>
        )}

        {/* Bind-honesty line. The verdict above is the page's one accent
            surface; this keeps its amber and its place above the fold, but as a
            single row under the verdict rather than a second shouting box. */}
        {v.bindOpen && (
          <div
            data-testid="open-parts-banner"
            data-open-count={v.openParts.length || v.unresolved.length}
            className="mt-2 flex items-start gap-2 px-1 text-sm"
            style={{ color: 'var(--warn-strong)' }}
          >
            <span className="shrink-0" style={{ display: 'inline-flex', marginTop: 3 }}>
              <WarningIcon size={14} />
            </span>
            <span style={{ color: 'var(--silk-dim)' }}>
              <span style={{ color: 'var(--warn-strong)' }}>
                <Term id="open_part">{plural(v.openParts.length || v.unresolved.length, 'part')} left out of the circuit</Term>
              </span>{' '}
              (no model). {v.unresolved.join(', ')} {v.unresolved.length === 1 ? 'is an active part' : 'are active parts'},
              so analog / AC / thermal results on their nets are not trustworthy.{' '}
              <button
                type="button"
                data-testid="show-guessed-parts"
                className="hb-press underline"
                style={{ color: 'var(--copper-hi)', background: 'none', border: 'none', padding: 0 }}
                onClick={() => { setGuessOpen(true); requestAnimationFrame(() => centerInScroller(guessRef.current)) }}
              >
                Show these parts
              </button>
            </span>
          </div>
        )}

        {/* Top-level honesty notes (minus the one the amber line above already
            says; see reportView). */}
        <div className="mt-3">
          {v.notes.map((n, i) => <NoteBlock key={i}>{n.message}</NoteBlock>)}
        </div>

        {/* Board map: the real renderer (pads, outline, pan/zoom, layers)
            whenever the uploaded file is KiCad layout text; the dot map only
            as the fallback for formats the client cannot draw. */}
        {boardUrl ? (
          <section className="mt-6">
            <div
              ref={mapRef}
              className={mapFullscreen ? 'overflow-hidden' : 'rounded-xl overflow-hidden'}
              style={mapFullscreen
                ? { position: 'fixed', inset: 0, zIndex: 60, background: 'var(--instrument)' }
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
                fullscreen={mapFullscreen}
                onToggleFullscreen={() => setMapFullscreen(v => !v)}
                onNetClick={setSelectedNet}
                onFootprintClick={fp => setSelectedComponent({
                  ref: fp.ref, value: fp.value, lib_id: fp.lib_id,
                  padNet: fp.padNet, padNets: fp.padNets,
                })}
                onEmptyBoard={onEmptyBoard}
                pointedRefs={pointedRefs}
                pointedNets={pointedNets}
                frameTarget={frameTarget}
                resultOverlay={overlayOn ? overlay : null}
              />
              {/* Floating selection card, same language as the live sim. A
                  strip that starts below the viewer toolbar and ends at the
                  bottom of the map: the card sits at its foot and grows up into
                  it, so a part with many nets scrolls inside the card instead
                  of sliding under the Fit control. */}
              {(selectedNet || selectedComponent) && (
                <div className="absolute left-3 z-10 flex items-end pointer-events-none" style={{ top: TOOLBAR_CLEARANCE, bottom: 12 }}>
                  <div className="pointer-events-auto" style={{ maxHeight: '100%', display: 'flex' }}>
                    <SelectionCard
                      net={selectedNet}
                      component={selectedComponent}
                      boundKind={selectedComponent ? r.component_kinds?.[selectedComponent.ref] ?? null : null}
                      assertionCapabilities={selectedComponent ? r.component_assertions?.[selectedComponent.ref] ?? [] : []}
                      modelCoverage={coverageFor(r.model_coverage, selectedComponent?.ref)}
                      openSelection={r.bind?.open_selections?.find(s => s.reference === selectedComponent?.ref)}
                      netModels={modelsOnNet(r.model_coverage, selectedNet)}
                      onQueue={request => {
                        if (request.type === 'check') setConstraintDraft(request)
                        else { onQueue(request); onOpenChecks() }
                      }}
                      onAuthorModel={author}
                      onClose={() => { setSelectedNet(null); setSelectedComponent(null) }}
                      onPickNet={setSelectedNet}
                    />
                  </div>
                </div>
              )}
            </div>
            {overlayOn && overlayHasContent(overlay) && (
              <div data-testid="result-legend" className="mt-1.5 flex flex-wrap items-center gap-x-4 gap-y-1 text-[11px]" style={{ color: 'var(--silk-dim)' }}>
                <span className="font-semibold" style={{ color: 'var(--silk-faint)' }}>Result on the board:</span>
                {overlay.openRefs.length > 0 && (
                  <span className="inline-flex items-center gap-1.5">
                    <span aria-hidden="true" style={{ width: 12, height: 9, border: '1.5px dashed var(--warn)', borderRadius: 2 }} />no model
                  </span>
                )}
                {overlay.unjudgedNets.length > 0 && (
                  <span className="inline-flex items-center gap-1.5">
                    <span aria-hidden="true" style={{ width: 14, height: 0, borderTop: '2px dashed var(--warn)' }} />not judged
                  </span>
                )}
                {Object.values(overlay.findingNets).includes('serious') || Object.values(overlay.findingRefs).includes('serious') ? (
                  <span className="inline-flex items-center gap-1.5">
                    <span aria-hidden="true" style={{ width: 14, height: 3, background: 'var(--err)', borderRadius: 2 }} />serious finding
                  </span>
                ) : null}
                {Object.values(overlay.findingNets).includes('warning') || Object.values(overlay.findingRefs).includes('warning') ? (
                  <span className="inline-flex items-center gap-1.5">
                    <span aria-hidden="true" style={{ width: 14, height: 3, background: 'var(--warn)', borderRadius: 2 }} />warning
                  </span>
                ) : null}
              </div>
            )}
            <div className="mt-1.5 text-[11px]" style={{ color: 'var(--silk-faint)' }}>
              Click the map (or hold ctrl) to zoom · drag to pan · hover a trace to see its net · click a trace or part to inspect it, drive it, probe it or check it
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
            second, not walk down the page. The index starts at 1 because the
            verdict above is index 0, so the whole report reads as one arrival. */}
        {v.sections.filter(sec => sec !== v.preLayout?.copper).map((s, i) => (
          <StaggerItem key={i} index={i + 1}>
            <SectionBlock section={s} onLocate={boardUrl ? locate : undefined} />
          </StaggerItem>
        ))}

        {v.refusalRows && (
          <section
            className="mt-7 rounded-lg px-4 py-3"
            data-testid="analysis-refusal-contract"
            style={{ border: '1px solid var(--warn-border)', borderLeft: '4px solid var(--warn)', background: 'var(--warn-bg)' }}
          >
            <h2 className="text-[11px] font-bold tracking-widest uppercase mb-2" style={{ color: 'var(--warn-strong)' }}>
              Analysis could not make this claim
            </h2>
            {v.refusalRows.map(([label, value]) => (
              <div key={label} className="text-sm mt-1" style={{ color: 'var(--silk)' }}>
                <b style={{ color: 'var(--silk-dim)', fontWeight: 600 }}>{label}:</b>{' '}{value}
              </div>
            ))}
          </section>
        )}

        {v.cosim && (
          <CosimBlock
            cosim={v.cosim}
            liveAvailable={liveMode !== 'none'}
            onDriveLive={onDriveLive}
            simMounted={simMounted}
          />
        )}

        {guessN > 0 ? (
          <ReportPanel
            testId="guessed-parts"
            title="Parts Hauksbee had to guess at"
            summary={guessSummary}
            open={guessOpen}
            warn
            onToggle={setGuessOpen}
          >
            {guessBody}
          </ReportPanel>
        ) : guessBody}

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

        {v.evidence.has && <EvidencePanel evidence={v.evidence} />}

        <AdditionalEvidencePanel
          placement="report"
          {...evidenceSlots(session)}
          boardName={boardLabel ?? r.file_name}
          cosimRan={r.cosim?.ran}
          showWebControls={!!boardFile && !restoredFrom}
          open={evidenceOpen}
          onOpenChange={setEvidenceOpen}
        />

        {constraintDraft && (
          <QueuedConstraintModal initial={constraintDraft} onQueue={onQueue}
            onClose={() => setConstraintDraft(null)} onOpenChecks={onOpenChecks} />
        )}
      </div>
    </div>
  )
}
