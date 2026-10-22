import { loadPercent } from './LoadBar'
import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import type { BoardRequest, ModelCoverageComponent, WebOpenPart } from '../types/report'
import { coverageFor, explainReport, findingEntries, modelsOnNet, padNetsOf, plural, reportView, sectionName, sectionStatus, stageWords, unmodelledHome } from '../lib/report-view'
import type { BoardSession } from '../hooks/useBoardSession'
import { BoardViewer, TOOLBAR_CLEARANCE } from './BoardViewer'
import { OpenSelectionsBanner, SelectionCard } from './SelectionCard'
import { AdditionalEvidencePanel, evidenceSlots } from './AdditionalEvidencePanel'
import { WritePart } from './WritePart'
import { DropBoardAgain, UploadFailure } from './UploadFailure'
import { StaggerItem } from '../motion'
import { ExportMenu } from './ExportMenu'
import type { SpecSnapshot } from '../hooks/useSessions'
import { Callout, CopyButton, UploadBanners, useGlossary } from './ui'
import { FindingsList } from './report/Findings'
import { centerInScroller } from '../lib/scroll'
import { EvidencePanel, ImportDiagnosticsPanel, ModelCoveragePanel, ReportPanel } from './report/Panels'
import { CosimBlock } from './report/CosimBlock'
import { NextStep, nextKind, nextStepActions } from './report/NextStep'
import { runnableActions, withoutShown, type ActionHost } from '../lib/web-actions'
import { requestModelTab } from './WritePart'
import { ResultPanel } from './report/ResultPanel'
import { ResultLinkContext } from './report/ResultLinks'
import type { ResultLinkHandlers } from './report/ResultLinks'
import { overlayHasContent, resultOverlay, vocabulary } from '../lib/result-links'
import type { ResultEntity } from '../lib/result-links'
import { cliCommand } from '../lib/result-export'
import { BoardMap } from './report/BoardMap'
import { useCosimJob } from '../hooks/useCosimJob'
import { buildBoardUpload } from '../lib/board-upload'

// The Board view with a report in hand: the viewer as the hero surface (with
// its toolbar and layers panel), the plain-language verdict, and the findings.
// Landing/UploadView owns getting a board in; everything here owns saying what
// came back and letting the board be explored. The findings, the qualification
// panels, the co-sim block and the fallback dot map live in ./report.

export function BoardView({
  session, onQueue, onOpenChecks, onOpenEnvironment,
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
  /** Opens the Environment page, where a missing simulator is installed. */
  onOpenEnvironment?: () => void
}) {
  const r = session.report!
  // A part with variant cards waiting on a pick is the picker's to settle, not
  // a datasheet draft's: the open-selections banner above already offers it.
  const awaitingPick = new Set(r.bind?.open_selections?.map(s => s.reference))
  const openParts = (r.bind?.open_parts ?? []).filter(p => !awaitingPick.has(p.reference))
  const unmodelled = openParts.filter(part => !part.bound)
  const {
    boardUrl, selectedNet, selectedComponent, setSelectedNet, setSelectedComponent,
    busy, loadProgress, uploadError, uploadNotice, dismissNotice, firmwareFile,
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
  const [authoringComponent, setAuthoringComponent] = useState<ModelCoverageComponent | WebOpenPart | null>(null)
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
  const author = useCallback((component: ModelCoverageComponent | WebOpenPart) => {
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
  // "Add as check" on a result row lands on the same CheckThis dock a click on
  // the map does, with that offer already open: one way to add a check.
  const [checkIntent, setCheckIntent] = useState<{ kind: string; seq: number } | null>(null)
  // The finding the map is showing: everything it names stays lit (on top of
  // the hover highlight) until another finding is shown or this one again.
  const [shown, setShown] = useState<{ key: string; refs: string[]; nets: string[] } | null>(null)
  const shownKeyRef = useRef<string | null>(null)
  shownKeyRef.current = shown?.key ?? null
  useEffect(() => setShown(null), [session.runEpoch])
  const resultLinks = useMemo<ResultLinkHandlers>(() => {
    const go = (entity: ResultEntity, opts?: { scroll?: boolean }) => {
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
      if (opts?.scroll !== false) centerInScroller(mapRef.current)
    }
    return {
    vocab: vocabulary(r),
    onPoint: setPointed,
    onGo: go,
    onGiveModel: restoredFrom ? undefined : ref => {
      const row = coverageFor(r.model_coverage, ref)
      const part = row ?? r.bind?.open_parts?.find(p => p.reference === ref)
      if (part) author(part)
      else showGuessed()
    },
    variantRefs: new Set((r.bind?.open_selections ?? []).map(s => s.reference)),
    onPickVariant: restoredFrom ? undefined : showGuessed,
    checkOffer: entity => {
      if (entity.kind === 'net') return { kind: 'voltage', net: entity.net, label: `net ${entity.net} must sit at a voltage` }
      const caps = r.component_assertions?.[entity.ref] ?? []
      if (caps.includes('max_current')) return { kind: 'max_current', ref: entity.ref, label: `${entity.ref} must stay under a current` }
      if (caps.includes('max_temp')) return { kind: 'max_temp', ref: entity.ref, label: `${entity.ref} must stay under a temperature` }
      return null
    },
    onAddCheck: offer => {
      go(offer.ref ? { kind: 'part', ref: offer.ref } : { kind: 'net', net: offer.net ?? '' })
      setCheckIntent(prev => ({ kind: offer.kind, seq: (prev?.seq ?? 0) + 1 }))
    },
    onShow: (entities, at, key) => {
      if (shownKeyRef.current === key) {
        setShown(null)
        setFocusPoint(null)
        return
      }
      const refs = entities.flatMap(e => (e.kind === 'part' ? [e.ref] : []))
      const nets = entities.flatMap(e => (e.kind === 'net' ? [e.net] : []))
      setShown({ key, refs, nets })
      if (at) {
        locate(at.x, at.y, at.label)
      } else {
        setFocusPoint(null)
        frameSeq.current += 1
        setFrameTarget({ refs, nets, seq: frameSeq.current })
        centerInScroller(mapRef.current)
      }
    },
    shownKey: shown?.key ?? null,
    onOpenChecks,
    }
  }, [r, restoredFrom, author, showGuessed, setSelectedNet, setSelectedComponent, locate, shown, onOpenChecks])
  const pointedRefs = useMemo(
    () => [...(shown?.refs ?? []), ...(pointed?.kind === 'part' ? [pointed.ref] : [])], [pointed, shown])
  // A set of nets another view asked to see together (the Checks page's
  // "show the unrouted nets"): lit with the pointed net, framed once per ask.
  const highlight = session.highlightNets
  const pointedNets = useMemo(
    () => [...(shown?.nets ?? []), ...(pointed?.kind === 'net' ? [pointed.net] : []), ...(highlight?.nets ?? [])],
    [pointed, shown, highlight])
  useEffect(() => {
    if (!highlight) return
    frameSeq.current += 1
    setFrameTarget({ refs: [], nets: highlight.nets, seq: frameSeq.current })
    requestAnimationFrame(() => centerInScroller(mapRef.current))
  }, [highlight])
  const entries = useMemo(() => findingEntries(v), [v])
  const status = useMemo(() => sectionStatus(v), [v])
  const moreRef = useRef<HTMLDivElement>(null)
  const openEvidence = useCallback(() => {
    setEvidenceOpen(true)
    requestAnimationFrame(() => centerInScroller(moreRef.current))
  }, [])
  const canShow = !!boardUrl
  // The emulator co-sim, for a board whose MCU the synchronous analysis cannot
  // run. It is offered only while the page still holds the files the analysis
  // used (a saved session or a server-preloaded board has none to send).
  const cosimJob = useCosimJob({
    upload: () => (boardFile
      ? buildBoardUpload(boardFile, firmwareFile, session.schematicFile, session.supplementalFiles)
      : null),
    onDone: session.replaceCosim,
    epoch: session.runEpoch,
  })
  const emulatorJob = boardFile && !restoredFrom ? cosimJob : null
  // What the engine's typed actions (next steps, a section's buttons) can do here.
  // A saved session has no files behind it, so it cannot open the model flow or
  // the evidence slots.
  const actionHost = useMemo<ActionHost>(() => ({
    giveModel: restoredFrom ? undefined : (ref, tab) => {
      if (tab) requestModelTab(tab)
      resultLinks.onGiveModel?.(ref)
    },
    openEvidence: restoredFrom ? undefined : openEvidence,
    openChecks: onOpenChecks,
    queue: onQueue,
    show: canShow
      ? ({ refs, net, at, label }) => {
          const entities: ResultEntity[] = [...refs.map(ref => ({ kind: 'part' as const, ref })), ...(net ? [{ kind: 'net' as const, net }] : [])]
          resultLinks.onShow?.(entities, at ? { ...at, label } : null, `action:${refs.join(',')}:${net ?? ''}:${at?.x ?? ''},${at?.y ?? ''}`)
        }
      : undefined,
    openEnvironment: onOpenEnvironment,
  }), [restoredFrom, openEvidence, onOpenChecks, onQueue, canShow, resultLinks, onOpenEnvironment])

  if (!r.ok) return <UploadFailure report={r} session={session} />

  // Everything the run had to assume about a part, in one place: the parts with
  // no model, the variants the board leaves open, and what each model covers.
  const selections = r.bind?.open_selections ?? []
  const coverageGaps = r.model_coverage?.components.filter(c => c.actionable_behavior_gap).length ?? 0
  const guessN = v.unmodelled.all.length + selections.length + coverageGaps
  // The parts with no usable model are stated once, above (the verdict card or
  // their finding, whose "List all" opens this); the summary names only what
  // is not said anywhere else.
  const guessSummary = [
    selections.length > 0 && `${plural(selections.length, 'variant')} not pinned`,
    coverageGaps > 0 && `${coverageGaps} modelled only in part`,
  ].filter(Boolean).join(' · ') || 'every part, and the way to give each a model'
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

      {/* Directly under the line that says a model is missing: the one
          "Model this part" surface (draft from a datasheet, extend, or write
          it), for whichever part the user picks. */}
      {!restoredFrom && (
        <>
          <div className="mt-3" ref={authoringRef}>
            <WritePart
              onSaved={session.reanalyzeCurrent}
              // The coverage row where there is one: its lib_id and footprint let the
              // editor infer the part's class (WebOpenPart carries neither).
              suggested={authoringComponent ?? (unmodelled[0] && (coverageFor(r.model_coverage, unmodelled[0].reference) ?? unmodelled[0]))}
              openSignal={authoringSignal > 0 ? `${authoringComponent?.reference}:${authoringSignal}` : null}
              boardLabel={boardLabel ?? r.file_name}
              onOpenSettings={onOpenSettings}
            />
          </div>
        </>
      )}
    </div>
  )

  // One card, docked beside or under the map (floating over it only when the map
  // is full screen). A check made here is queued without leaving the board; the
  // Checks view shows it when asked to.
  const card = (
    <SelectionCard
      net={selectedNet}
      component={selectedComponent}
      docked={!mapFullscreen}
      boundKind={selectedComponent ? r.component_kinds?.[selectedComponent.ref] ?? null : null}
      assertionCapabilities={selectedComponent ? r.component_assertions?.[selectedComponent.ref] ?? [] : []}
      modelCoverage={coverageFor(r.model_coverage, selectedComponent?.ref)}
      openSelection={r.bind?.open_selections?.find(s => s.reference === selectedComponent?.ref)}
      netModels={modelsOnNet(r.model_coverage, selectedNet)}
      openOffer={checkIntent}
      onQueue={request => {
        onQueue(request)
        if (request.type !== 'check' && request.type !== 'assert') onOpenChecks()
      }}
      onAuthorModel={author}
      onClose={() => { setSelectedNet(null); setSelectedComponent(null) }}
      onPickNet={setSelectedNet}
    />
  )

  const runCommand = `hauksbee run ${boardLabel ?? r.file_name} --serve`
  const firmwareName = firmwareFile?.name ?? restoredFrom?.firmwareName ?? null
  const hasMap = !!boardUrl || r.components?.length > 0
  const openRefs = new Set(overlay.openRefs)
  // The engine's next steps this app can run; a section's buttons skip any the
  // Next step already has, so one step is one button.
  const nextOffered = restoredFrom ? [] : nextStepActions(r, nextKind(r, v, { firmwareName: firmwareFile?.name ?? null, checks }), actionHost)
  const sectionActions = Object.fromEntries(v.sections
    .map(s => [sectionName(s.title), runnableActions(withoutShown(s.actions, nextOffered), actionHost)] as const)
    .filter(([, list]) => list.length > 0))

  // The legend of what the result draws on the board, with its on/off switch.
  const legend = boardUrl && overlayHasContent(overlay) && (
    <div data-testid="result-legend" className="mt-1.5 flex flex-wrap items-center gap-x-4 gap-y-1 text-[11px]" style={{ color: 'var(--silk-dim)' }}>
      <button
        type="button"
        role="switch"
        aria-checked={overlayOn}
        data-testid="result-overlay-toggle"
        onClick={() => setOverlayOn(on => !on)}
        title="Draw this result on the board: parts with no model, nets not judged, findings by severity"
        className="hb-press inline-flex items-center gap-1.5 font-semibold"
        style={{ background: 'none', border: 'none', padding: 0, color: 'var(--silk-dim)' }}
      >
        <span
          aria-hidden="true"
          className="relative inline-block rounded-full shrink-0"
          style={{ width: 22, height: 12, background: overlayOn ? 'var(--copper)' : 'var(--hairline)', transition: 'background 0.15s' }}
        >
          <span className="absolute rounded-full" style={{ top: 2, left: overlayOn ? 12 : 2, width: 8, height: 8, background: 'var(--surface)', transition: 'left 0.15s' }} />
        </span>
        Result on the board
      </button>
      {overlayOn && overlay.openRefs.length > 0 && (
        <span className="inline-flex items-center gap-1.5">
          <span aria-hidden="true" style={{ width: 12, height: 9, border: '1.5px dashed var(--warn)', borderRadius: 2 }} />no model
        </span>
      )}
      {overlayOn && overlay.unjudgedNets.length > 0 && (
        <span className="inline-flex items-center gap-1.5">
          <span aria-hidden="true" style={{ width: 14, height: 0, borderTop: '2px dashed var(--warn)' }} />not judged
        </span>
      )}
      {overlayOn && (Object.values(overlay.findingNets).includes('serious') || Object.values(overlay.findingRefs).includes('serious')) && (
        <span className="inline-flex items-center gap-1.5">
          <span aria-hidden="true" style={{ width: 14, height: 3, background: 'var(--err)', borderRadius: 2 }} />serious finding
        </span>
      )}
      {overlayOn && (Object.values(overlay.findingNets).includes('warning') || Object.values(overlay.findingRefs).includes('warning')) && (
        <span className="inline-flex items-center gap-1.5">
          <span aria-hidden="true" style={{ width: 14, height: 3, background: 'var(--warn)', borderRadius: 2 }} />warning
        </span>
      )}
    </div>
  )

  return (
    <div className="h-full overflow-y-auto view-enter" data-testid="report">
      <div className="max-w-[1680px] mx-auto px-4 sm:px-6 pt-4 pb-16">
        {/* Re-analysis in progress (firmware added or swapped): the report
            stays visible, this line says what is happening. */}
        {busy && (
          <div
            role="status"
            aria-live="polite"
            className="mb-3 text-sm flex items-center justify-center gap-2"
            style={{ color: 'var(--copper-hi)' }}
          >
            <span className="slot-spin" />
            Analyzing <span style={{ fontFamily: 'var(--font-mono)' }}>{busy.board}</span>
            {busy.firmware && <>{' + co-sim of '}<span style={{ fontFamily: 'var(--font-mono)' }}>{busy.firmware}</span></>}
            {' ...'}
            {loadProgress && (
              <span data-testid="reanalysis-progress" style={{ color: 'var(--silk-dim)' }}>
                {loadProgress.label}, {loadPercent(loadProgress.fraction)}%
              </span>
            )}
          </div>
        )}
        <UploadBanners
          notice={uploadNotice}
          error={uploadError}
          onDismiss={dismissNotice}
          className="mb-3"
        />

        {/* The board and its result. Where there is room (1100 px and up) the
            drawing fills the left and the result is a rail beside it: the
            verdict card, the part or net picked on the board, then every
            finding as a card that reaches the board. Narrower, the verdict
            card comes first, then the drawing, then the findings under it.
            The rail is `display: contents` when narrow so the same elements
            reorder rather than render twice. */}
        <ResultLinkContext.Provider value={resultLinks}>
          <div
            className="flex flex-col gap-3 min-[1100px]:flex-row min-[1100px]:items-stretch"
            style={{ '--row-h': 'clamp(520px, calc(100dvh - 120px), 1100px)' } as React.CSSProperties}
          >
            {hasMap && (
              <div className="order-2 min-[1100px]:order-none min-w-0 flex-1 flex flex-col min-[1100px]:h-[var(--row-h)]">
                {boardUrl ? (
                  <>
                    {highlight && (
                      <div
                        data-testid="board-highlight"
                        className="flex items-center gap-2 mb-2 px-3 py-1.5 rounded-lg text-[12px]"
                        style={{ background: 'var(--surface-2)', border: '1px solid var(--hairline)', color: 'var(--silk)' }}
                      >
                        <span className="min-w-0 flex-1">Lit on the board: {highlight.label}</span>
                        <button
                          type="button"
                          data-testid="board-highlight-clear"
                          className="hb-btn hb-press"
                          onClick={() => session.setHighlightNets(null)}
                        >
                          Clear
                        </button>
                      </div>
                    )}
                    <div
                      ref={mapRef}
                      data-testid="board-map-frame"
                      className={mapFullscreen ? 'overflow-hidden' : 'rounded-xl overflow-hidden h-[clamp(320px,62vh,560px)] min-[1100px]:h-auto min-[1100px]:flex-1 min-h-0'}
                      style={mapFullscreen
                        ? { position: 'fixed', inset: 0, zIndex: 60, background: 'var(--instrument)' }
                        : { position: 'relative', border: '1px solid var(--hairline)', boxShadow: 'var(--shadow-card)' }}
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
                        onToggleFullscreen={() => setMapFullscreen(on => !on)}
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
                      {mapFullscreen && (selectedNet || selectedComponent) && (
                        <div className="selection-dock absolute left-3 z-10 flex items-end pointer-events-none" style={{ top: TOOLBAR_CLEARANCE, bottom: 12 }}>
                          <div className="pointer-events-auto" style={{ maxHeight: '100%', display: 'flex' }}>{card}</div>
                        </div>
                      )}
                    </div>
                    {legend}
                    <div className="mt-1 text-[11px]" style={{ color: 'var(--silk-faint)' }}>
                      Click the board (or hold ctrl) to zoom · drag to pan · click a trace or part to inspect it, drive it or check it
                    </div>
                  </>
                ) : (
                  <section ref={mapRef}>
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
                )}
              </div>
            )}

            <div
              data-testid="result-rail"
              className={hasMap
                ? 'contents min-[1100px]:flex min-[1100px]:flex-col min-[1100px]:gap-3 min-[1100px]:order-1 min-[1100px]:w-[400px] min-[1100px]:shrink-0 min-[1100px]:h-[var(--row-h)] min-[1100px]:overflow-y-auto min-[1100px]:pr-1'
                : 'flex flex-col gap-3 w-full max-w-4xl mx-auto'}
            >
              {/* The verdict, how much of the board it speaks for, what it
                  rests on, the one next step, and what it can become (an
                  export). Its parts and nets are live links to the board. */}
              <div className="order-1">
                <StaggerItem index={0}>
                  <ResultPanel
                    report={r}
                    view={v}
                    explanation={explanation}
                    onOpenChecks={onOpenChecks}
                    onListParts={showGuessed}
                    next={restoredFrom ? undefined : (
                      <NextStep
                        report={r}
                        view={v}
                        firmwareName={firmwareFile?.name ?? null}
                        checks={checks}
                        specFile={spec?.fileName ?? null}
                        onQueue={onQueue}
                        onOpenChecks={onOpenChecks}
                        host={actionHost}
                        onLocate={boardUrl ? locate : undefined}
                        quiet={!!(selectedNet || selectedComponent)}
                      />
                    )}
                    exportSlot={(
                      <ExportMenu
                        report={r}
                        boardLabel={boardLabel}
                        firmwareName={firmwareName}
                        analyzedAt={session.analyzedAt}
                        engineVersion={engineVersion}
                        spec={spec}
                        checks={checks}
                        sessionName={sessionName}
                        restored={restoredFrom !== null}
                        command={cliCommand({ board: boardLabel ?? r.file_name, firmware: firmwareName })}
                      />
                    )}
                  />
                </StaggerItem>

                {/* A report that came out of storage rather than out of a run.
                    It says so for as long as it is on screen, and which
                    actions need the board's bytes again. */}
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
              </div>

              {!mapFullscreen && (selectedNet || selectedComponent) && (
                <div data-testid="selection-dock" className="order-3 w-full">
                  {card}
                </div>
              )}

              <div className="order-4">
                <StaggerItem index={1}>
                  <FindingsList
                    entries={entries}
                    status={status}
                    openParts={unmodelledHome(r, v.unmodelled) === 'findings' ? v.unmodelled : null}
                    openRefs={openRefs}
                    preLayout={v.preLayout}
                    sectionActions={sectionActions}
                    onListParts={showGuessed}
                    onLocate={boardUrl ? locate : undefined}
                  />
                </StaggerItem>
              </div>
            </div>
          </div>
        </ResultLinkContext.Provider>

        {v.refusalRows && (
          <section
            className="mt-6 rounded-lg px-4 py-3"
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
          <ResultLinkContext.Provider value={resultLinks}>
            <CosimBlock
              cosim={v.cosim}
              liveAvailable={liveMode !== 'none'}
              onDriveLive={onDriveLive}
              simMounted={simMounted}
              emulator={emulatorJob}
            />
          </ResultLinkContext.Provider>
        )}

        {/* What else the run holds, each folded: the parts it had to guess at
            (where a model is written), how the file was read, the evidence
            behind each claim, and the slots for more evidence. */}
        <div className="mt-6 max-w-4xl" ref={moreRef}>
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

          {/* No live capability registered: how to start one from a terminal. */}
          {liveMode === 'none' && (
            <div
              data-testid="run-it-hint"
              className="mt-3 rounded-lg px-4 py-3 text-xs"
              style={{ border: '1px solid var(--hairline)', background: 'var(--surface)', color: 'var(--silk-dim)' }}
            >
              <div>To drive this board live (scope, transport controls), start it with:</div>
              <div className="mt-1.5 flex items-center flex-wrap">
                <code className="hb-code" style={{ padding: '2px 6px', fontSize: 11 }}>{runCommand}</code>
                <CopyButton text={runCommand} testId="copy-cli" />
              </div>
            </div>
          )}
        </div>
      </div>
    </div>
  )
}
