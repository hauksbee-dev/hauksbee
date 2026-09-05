import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import type { ModelCoverageComponent } from '../types/report'
import { uncoveredTimingRefusals } from '../lib/cosim-coverage'
import type { BoardSession } from '../hooks/useBoardSession'
import { WarningIcon } from './Icons'
import { BoardViewer, TOOLBAR_CLEARANCE } from './BoardViewer'
import { SelectionCard } from './SelectionCard'
import { FirmwareJack } from './FirmwareJack'
import { SchematicJack } from './SchematicJack'
import { DatasheetExtract } from './DatasheetExtract'
import { WritePart } from './WritePart'
import { acceptedFormatsSentence, withoutEngineFormatList } from '../lib/board-formats'
import { StaggerItem } from '../motion'
import { ExportMenu } from './ExportMenu'
import type { SpecSnapshot } from '../hooks/useSessions'
import { reportVerdictHeadline, reportVerdictPalette } from '../lib/report-verdict'
import { refusalLines } from '../lib/refusal-contract'
import { Callout, CopyButton, UploadBanners } from './ui'
import { NoteBlock, SectionBlock } from './report/Findings'
import { EvidencePanel, ImportDiagnosticsPanel, ModelCoveragePanel } from './report/Panels'
import { CosimBlock } from './report/CosimBlock'
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
  session, onQueueCheck, onQueuePeripheral, onQueueSensor, onQueueSupply, onOpenChecks,
  onDriveLive, simMounted, engineVersion, spec, checks, sessionName,
}: {
  session: BoardSession
  onQueueCheck: (check: { kind: string; net?: string; ref?: string }) => void
  onQueuePeripheral?: (peripheral: { id?: string; kind: 'stimulus' | 'pushbutton' | 'toggle'; net?: string; ref?: string }) => void
  onQueueSensor?: (sensor: { id: string; ref?: string; modelId?: string | null }) => void
  onQueueSupply?: (supply: { net: string; volts?: number }) => void
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
  const [authoringComponent, setAuthoringComponent] = useState<ModelCoverageComponent | null>(null)
  const [authoringSignal, setAuthoringSignal] = useState(0)
  const authoringRef = useRef<HTMLDivElement>(null)
  const focusSeq = useRef(0)
  const mapRef = useRef<HTMLDivElement>(null)

  useEffect(() => {
    if (authoringSignal === 0) return
    requestAnimationFrame(() => authoringRef.current?.scrollIntoView({ behavior: 'smooth', block: 'center' }))
  }, [authoringSignal])
  useEffect(() => setImportOverlay(false), [session.runEpoch])

  const locate = useCallback((x: number, y: number, label: string) => {
    focusSeq.current += 1
    setFocusPoint({ x, y, label, seq: focusSeq.current })
    mapRef.current?.scrollIntoView({ behavior: 'smooth', block: 'center' })
  }, [])
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
  const author = useCallback((component: ModelCoverageComponent) => {
    setAuthoringComponent(component)
    setAuthoringSignal(value => value + 1)
  }, [])

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

  const bindOpen = !!(r.bind?.active_path_unresolved?.length)
  const runCommand = `hauksbee run ${boardLabel ?? r.file_name} --serve`
  const { border: verdictBorder, background: verdictBg } = reportVerdictPalette(r)
  const hasEvidence = (r.inventory?.length ?? 0) > 0
    || (r.assumptions?.length ?? 0) > 0
    || (r.evidence?.length ?? 0) > 0

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
            <div className="text-xs mt-1.5 tnum" data-testid="report-inventory" style={{ color: 'var(--silk-dim)' }}>
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
        {bindOpen && (
          <div className="mt-2 flex items-start gap-2 px-1 text-sm" style={{ color: 'var(--warn-strong)' }}>
            <span className="shrink-0" style={{ display: 'inline-flex', marginTop: 3 }}>
              <WarningIcon size={14} />
            </span>
            <span style={{ color: 'var(--silk-dim)' }}>
              <span style={{ color: 'var(--warn-strong)' }}>{r.bind!.active_path_unresolved!.join(', ')}</span>{' '}
              could not be bound or are left open on the live circuit. Analog / AC / thermal
              results on their nets are not trustworthy.
            </span>
          </div>
        )}

        {r.model_coverage && (
          <ModelCoveragePanel
            coverage={r.model_coverage}
            onSelect={component => {
              const padNets = [...new Set(component.pins.flatMap(pin => pin.net ? [pin.net] : []))]
              setSelectedNet(null)
              setSelectedComponent({
                ref: component.reference,
                value: component.value,
                lib_id: component.lib_id,
                padNet: padNets[0] ?? null,
                padNets,
              })
              const located = component.pins.find(pin => pin.position_mm)?.position_mm
              if (located) locate(located[0], located[1], `${component.reference}: ${component.stage.replaceAll('_', ' ')}`)
            }}
            onAuthor={author}
          />
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
            <DatasheetExtract openParts={r.bind?.open_parts ?? []} onSaved={session.reanalyzeCurrent} />
            <div className="mt-3" ref={authoringRef}>
              <WritePart
                onSaved={session.reanalyzeCurrent}
                suggested={authoringComponent ?? r.bind?.open_parts?.find(part => !part.bound)}
                openSignal={authoringSignal > 0 ? `${authoringComponent?.reference}:${authoringSignal}` : null}
                boardLabel={boardLabel ?? r.file_name}
              />
            </div>
          </>
        )}

        {/* Top-level honesty notes. The bind-role note restates exactly what
            the amber unresolved-parts line above already says (the JSON carries
            both for CLI parity), so render it once, keeping the stronger one. */}
        <div className="mt-3">
          {(r.notes || []).filter(n => !(bindOpen && n.kind === 'bind_role')).map((n, i) => (
            <NoteBlock key={i}>{n.message}</NoteBlock>
          ))}
        </div>

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

        {hasEvidence && (
          <EvidencePanel
            inventory={r.inventory ?? []}
            assumptions={r.assumptions ?? []}
            evidence={r.evidence ?? []}
          />
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
                      modelCoverage={selectedComponent
                        ? r.model_coverage?.components.find(c => c.reference === selectedComponent.ref) ?? null
                        : null}
                      netModels={selectedNet
                        ? (r.model_coverage?.components ?? []).filter(c => c.pins.some(pin => pin.net === selectedNet))
                        : []}
                      onQueueCheck={check => { onQueueCheck(check); onOpenChecks() }}
                      onQueuePeripheral={onQueuePeripheral && (p => { onQueuePeripheral(p); onOpenChecks() })}
                      onQueueSensor={onQueueSensor && (s => { onQueueSensor(s); onOpenChecks() })}
                      onQueueSupply={onQueueSupply && (s => { onQueueSupply(s); onOpenChecks() })}
                      onAuthorModel={author}
                      onClose={() => { setSelectedNet(null); setSelectedComponent(null) }}
                      onPickNet={setSelectedNet}
                    />
                  </div>
                </div>
              )}
            </div>
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
