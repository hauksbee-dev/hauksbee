import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import App from '../../frontend/src/App'
import { BoardViewer } from '../../frontend/src/components/BoardViewer'
import { SimSourceContext } from '../../frontend/src/demo/simSource'
import { useReplaySimulation } from '../../frontend/src/demo/useReplaySimulation'
import { parseSession } from '../../frontend/src/demo/manifest'
import type { LoadedSession, ManifestSession } from '../../frontend/src/demo/manifest'
import { readNet } from '../../frontend/src/lib/net-state'
import type { SimFrame } from '../../frontend/src/types/protocol'
import { buildToml, checksStorageKey, fullRow } from '../shared/checks-spec'
import type { CacheIndexEntry, LoadedBoard, RecordedRun, SessionManifest } from './cache'
import { loadBoard, loadIndex, loadManifest } from './cache'
import { installCachedTransport } from './transport'
import type { CachedTransport } from './transport'
import { HONESTY_LINE, SUGGESTED_HEIGHT } from './contract'
import type { EmbedEvent, EmbedEventName, EmbedState } from './contract'

// The embeddable widget: two states over one cache.
//
//   COMPACT   the real 2D board map, already rendered, already moving, one
//             prompt and one caption. Nothing to sign up for and nothing to
//             wait for: the first click is a real net selection on a real
//             recorded frame.
//   EXPANDED  the real app. Not a tour of it, not a video of it: App.tsx, the
//             same component the installed engine serves, driven by the
//             recorded cache instead of a local engine.
//
// The widget owns three things the app does not: the compact state, the
// honesty caption, and the feature flag that hides every surface whose answers
// were never recorded (demo/embed/embed.css).

export { HONESTY_LINE, SUGGESTED_HEIGHT } from './contract'
export type { EmbedEvent, EmbedEventName, EmbedState } from './contract'

/** No pointer, key or wheel inside the widget for this long fires `idle`. */
const IDLE_AFTER_MS = 30_000

export interface WidgetHandle {
  setState: (s: EmbedState) => void
  loadBoard: (id: string) => void
  reset: () => void
}

// ── Compact ──────────────────────────────────────────────────────────────────

/** The compact map. Runs the same replay source the live surface runs, so the
 *  copper carries the recorded frame's real net state and the activity moves. */
function CompactMap({
  session, boardFileUrl, hintNet, onNetClick, onPartClick, prompt, subject,
}: {
  session: LoadedSession | null
  boardFileUrl: string
  hintNet: string | null
  onNetClick: (net: string | null) => void
  onPartClick: (ref: string) => void
  prompt: string
  subject: string
}) {
  const [selected, setSelected] = useState<string | null>(null)
  const [touched, setTouched] = useState(false)
  const [promptStale, setPromptStale] = useState(false)

  // The hint: the flagged net highlighted once, dropped the moment the visitor
  // does anything. It is a pointer, not a state to be stuck in.
  const hinted = !touched && hintNet ? hintNet : selected

  /* The prompt is an instruction, and an instruction outlives its moment badly:
     left up, it is a label parked on the board, covering the copper it is
     pointing at. It goes on the first interaction, and on its own if that
     interaction never comes. */
  useEffect(() => {
    const timer = window.setTimeout(() => setPromptStale(true), 12_000)
    return () => window.clearTimeout(timer)
  }, [])
  const promptGone = touched || promptStale

  const body = (frame: SimFrame | null) => (
    <>
      <div className="hb-embed-compact-map">
        {/* Keyed by the board: the viewer fits its camera to the board it first
            parsed, so a board switch has to be a new viewer or the new board is
            drawn off-screen under the old board's camera. */}
        <MapCanvas
          key={boardFileUrl}
          boardFileUrl={boardFileUrl}
          frame={frame}
          selectedNet={hinted}
          onNetClick={net => { setTouched(true); setSelected(net); onNetClick(net) }}
          onPartClick={ref => { setTouched(true); onPartClick(ref) }}
        />
      </div>
      <div className="hb-embed-compact-overlay">
        <div
          className="hb-embed-prompt"
          data-testid="embed-prompt"
          data-gone={promptGone ? 'true' : undefined}
        >
          <span className="hb-embed-prompt-strong">{subject}</span> {prompt}
        </div>
        {hinted && <NetReadout frame={frame} net={hinted} pulse={!touched} />}
      </div>
    </>
  )

  return (
    <div className="hb-embed-compact" data-testid="embed-compact">
      {session
        ? <ReplayFrames session={session} render={body} />
        : body(null)}
    </div>
  )
}

/** The one net readout: the recorded frame's own measurement of the net the
 *  visitor (or the hint) has selected. Real numbers off a real run, which is the
 *  whole reason to click. */
function NetReadout({ frame, net, pulse }: {
  frame: SimFrame | null
  net: string
  pulse: boolean
}) {
  const r = readNet(frame, net)
  return (
    <div
      className={`hb-embed-readout${pulse ? ' hb-embed-readout-pulse' : ''}`}
      data-testid="embed-readout"
    >
      <span className="hb-embed-readout-net">{net}</span>
      <span className="hb-embed-readout-v">
        {r.kind === 'measured'
          ? `${r.volts.toFixed(3)} V${r.moving ? ' · moving' : ''}`
          : r.kind === 'unobserved' ? 'driven, not observed' : 'not in this frame'}
      </span>
    </div>
  )
}

/** Calls the replay hook once and hands the current frame down. One mount per
 *  surface: the hook owns a playhead, and two mounts would be two clocks over
 *  the same recording disagreeing with each other on screen. */
function ReplayFrames({ session, render }: {
  session: LoadedSession
  render: (frame: SimFrame | null) => React.ReactNode
}) {
  const sim = useReplaySimulation(session)
  return <>{render(sim.frame)}</>
}

function MapCanvas({ boardFileUrl, frame, selectedNet, onNetClick, onPartClick }: {
  boardFileUrl: string
  frame: SimFrame | null
  selectedNet: string | null
  onNetClick: (net: string | null) => void
  onPartClick: (ref: string) => void
}) {
  const faultedRefs = useMemo(
    () => new Set((frame?.faults ?? []).map(f => f.component)),
    [frame],
  )
  return (
    <BoardViewer
      boardFile={boardFileUrl}
      frame={frame}
      selectedNet={selectedNet}
      onNetClick={onNetClick}
      onFootprintClick={info => onPartClick(info.ref)}
      faultedRefs={faultedRefs}
      wheelMode="capture-on-focus"
    />
  )
}

// ── The widget ───────────────────────────────────────────────────────────────

export function EmbedWidget({
  assetBase, initialBoardId, initialState, onEvent, handleRef, version,
}: {
  assetBase: string
  initialBoardId?: string
  initialState: EmbedState
  onEvent: (e: EmbedEvent) => void
  handleRef: (h: WidgetHandle) => void
  version: string
}) {
  const [state, setState] = useState<EmbedState>(initialState)
  const [index, setIndex] = useState<CacheIndexEntry[]>([])
  const [manifest, setManifest] = useState<SessionManifest | null>(null)
  const [boardId, setBoardId] = useState<string | null>(initialBoardId ?? null)
  const [board, setBoard] = useState<LoadedBoard | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [assembledNote, setAssembledNote] = useState(false)
  const [appEpoch, setAppEpoch] = useState(0)
  /** Which recorded spec the checks panel opens on. Switching it reseeds the
   *  panel's saved state and remounts the app, which is the only way the panel's
   *  mount-time restore can be authoritative (it is keyed by board, not by
   *  spec). Everything it can then run is a recorded run. */
  const [presetId, setPresetId] = useState<string | null>(null)

  /** The app surface the visitor was last on. A preset switch remounts the app
   *  (the checks panel's restore only happens at mount), and coming back on the
   *  Board view when you were reading Checks reads as the widget resetting
   *  itself. The nav item is clicked again on the way in, which is exactly what
   *  a visitor would do. */
  const lastView = useRef<string | null>(null)
  const transport = useRef<CachedTransport | null>(null)
  const loaded = useRef(new Map<string, LoadedBoard>())
  const engagedRef = useRef(false)
  const readyRef = useRef(false)
  const rootRef = useRef<HTMLDivElement | null>(null)

  // ── The transport. Installed once, for the widget's lifetime: the app is
  // mounted and unmounted underneath it as boards change.
  useEffect(() => {
    transport.current = installCachedTransport({
      version,
      onEvent: e => {
        if (e.path === '/api/check') setAssembledNote(e.kind === 'assembled')
      },
    })
    return () => { transport.current?.uninstall(); transport.current = null }
  }, [version])

  const emit = useCallback((type: EmbedEventName, payload?: Record<string, unknown>) => {
    onEvent({ type, payload })
  }, [onEvent])

  const engage = useCallback((how: string) => {
    if (engagedRef.current) return
    engagedRef.current = true
    emit('engaged', { how, board: boardId, state })
  }, [emit, boardId, state])

  // ── Idle. Counted from the last interaction inside the widget, and only once
  // per idle stretch: a host that expands on `engaged` wants to know when
  // attention left, not a heartbeat.
  useEffect(() => {
    const el = rootRef.current
    if (!el) return
    let timer: ReturnType<typeof setTimeout> | null = null
    const arm = () => {
      if (timer) clearTimeout(timer)
      timer = setTimeout(
        () => emit('idle', { for_ms: IDLE_AFTER_MS, board: boardId, state }),
        IDLE_AFTER_MS,
      )
    }
    const events = ['pointerdown', 'pointermove', 'wheel', 'keydown'] as const
    for (const e of events) el.addEventListener(e, arm, { passive: true })
    arm()
    return () => {
      for (const e of events) el.removeEventListener(e, arm)
      if (timer) clearTimeout(timer)
    }
  }, [emit, boardId, state])

  // ── The cache index and the first board.
  useEffect(() => {
    let alive = true
    void (async () => {
      try {
        const [idx, man] = await Promise.all([loadIndex(assetBase), loadManifest(assetBase)])
        if (!alive) return
        setIndex(idx.boards)
        setManifest(man)
        const first = (initialBoardId && idx.boards.find(b => b.id === initialBoardId))
          ?? idx.boards[0]
        if (!first) throw new Error('the cache index lists no boards')
        setBoardId(first.id)
      } catch (e) {
        if (!alive) return
        const msg = e instanceof Error ? e.message : String(e)
        setError(msg)
        emit('error', { message: msg, phase: 'index' })
      }
    })()
    return () => { alive = false }
  }, [assetBase, initialBoardId, emit])

  // ── Load whichever board is selected (cached after the first time).
  useEffect(() => {
    if (!boardId || !manifest) return
    const entry = index.find(b => b.id === boardId)
    if (!entry) return
    const cached = loaded.current.get(boardId)
    if (cached) {
      const first = defaultPresetOf(cached)
      seedChecksPanel(cached, first)
      setPresetId(first?.id ?? null)
      setBoard(cached)
      transport.current?.setBoard(cached.cache)
      return
    }
    let alive = true
    void (async () => {
      try {
        const b = await loadBoard(assetBase, entry, manifest)
        if (!alive) return
        loaded.current.set(boardId, b)
        const first = defaultPresetOf(b)
        seedChecksPanel(b, first)
        setPresetId(first?.id ?? null)
        transport.current?.setBoard(b.cache)
        setBoard(b)
      } catch (e) {
        if (!alive) return
        const msg = e instanceof Error ? e.message : String(e)
        setError(msg)
        emit('error', { message: msg, phase: 'board', board: boardId })
      }
    })()
    return () => { alive = false }
  }, [assetBase, boardId, index, manifest, emit])

  // The board's object URL for the compact map (the app makes its own from the
  // File it is handed; this one is the compact state's, and it is revoked with
  // the board it belongs to).
  const boardObjectUrl = useMemo(
    () => (board ? URL.createObjectURL(board.boardFile) : null),
    [board],
  )
  useEffect(() => () => { if (boardObjectUrl) URL.revokeObjectURL(boardObjectUrl) }, [boardObjectUrl])

  const session = useMemo<LoadedSession | null>(() => {
    if (!board || !boardObjectUrl) return null
    const entry = board.sessionEntry as ManifestSession
    const parsed = parseSession(entry, board.sessionText, null)
    // The recorded BoardInfo points its board_url at the capture server, and
    // parseSession rewrites it to a site-relative session path. Neither exists
    // here: the layout the live surface draws is the same File the app was
    // handed, so the replay's board URL is that object URL.
    return { ...parsed, boardFileUrl: boardObjectUrl }
  }, [board, boardObjectUrl])

  // The replay source the app's live surface uses. Its identity must be
  // constant for the mount that consumes it, so it is memoised per session and
  // the app is remounted (keyed) when the session changes.
  const replayHook = useMemo(() => {
    if (!session) return null
    // Not a hook call: the SimSource contract is "hand down a hook", and the
    // consumer calls it from its own body. Keyed remount below keeps the
    // identity stable, which is what the contract actually requires.
    // eslint-disable-next-line react-hooks/rules-of-hooks
    return () => useReplaySimulation(session)
  }, [session])

  // ── ready, once the compact surface can be touched.
  useEffect(() => {
    if (readyRef.current || !board) return
    readyRef.current = true
    emit('ready', {
      board: board.cache.id,
      state,
      boards: index.map(b => ({ id: b.id, title: b.title })),
      suggested_height: SUGGESTED_HEIGHT[state],
      engine_version: board.cache.engine_version,
    })
  }, [board, emit, index, state])

  const goto = useCallback((next: EmbedState) => {
    setState(prev => {
      if (prev === next) return prev
      return next
    })
  }, [])

  // ── The host's handle.
  useEffect(() => {
    handleRef({
      setState: goto,
      loadBoard: id => {
        if (!index.some(b => b.id === id)) {
          emit('error', { message: `no cached board "${id}"`, phase: 'loadBoard' })
          return
        }
        setBoardId(id)
        setAppEpoch(n => n + 1)
      },
      reset: () => {
        engagedRef.current = false
        setAssembledNote(false)
        lastView.current = null
        const b = boardId ? loaded.current.get(boardId) : null
        if (b) {
          const first = defaultPresetOf(b)
          seedChecksPanel(b, first)
          setPresetId(first?.id ?? null)
          transport.current?.setBoard(b.cache)
        }
        setAppEpoch(n => n + 1)
        setState(initialState)
      },
    })
  }, [handleRef, goto, index, emit, initialState, boardId])

  const expand = useCallback((how: string) => {
    engage(how)
    emit('requestExpand', { board: boardId, how, suggested_height: SUGGESTED_HEIGHT.expanded })
    goto('expanded')
  }, [emit, engage, goto, boardId])

  const collapse = useCallback(() => {
    emit('requestCollapse', { board: boardId, suggested_height: SUGGESTED_HEIGHT.compact })
    goto('compact')
  }, [emit, goto, boardId])

  const cache = board?.cache ?? null
  const hintNet = cache?.feature_nets[0] ?? null
  const presets = useMemo(
    () => (cache?.checks ?? []).filter(r => r.kind === 'preset'),
    [cache],
  )
  const activePreset = presets.find(p => p.id === presetId) ?? null

  const choosePreset = useCallback((id: string) => {
    if (!board) return
    const preset = board.cache.checks.find(r => r.id === id)
    if (!preset) return
    engage(`preset:${id}`)
    seedChecksPanel(board, preset)
    setPresetId(id)
    // The app is about to be replaced, so the session it "launched" goes with
    // it: without this the transport still reports a live session and the new
    // mount labels it as somebody else's.
    transport.current?.setBoard(board.cache)
    // The panel restores its saved state at mount and the app stages the
    // firmware this spec was recorded with, so both are settled by remounting.
    setAppEpoch(n => n + 1)
  }, [board, engage])

  return (
    <div
      className="hb-embed-root"
      data-state={state}
      data-testid="embed-root"
      ref={rootRef}
    >
      {error && (
        <div className="hb-embed-error" data-testid="embed-error">
          <strong>The demo could not load its recording.</strong>
          <span>{error}</span>
        </div>
      )}

      {state === 'compact' && !error && (
        <>
          {board && boardObjectUrl ? (
            <CompactMap
              session={session}
              boardFileUrl={boardObjectUrl}
              hintNet={hintNet}
              subject={compactSubject(cache?.id ?? '')}
              prompt="Click a net."
              onNetClick={net => { engage(net ? `net:${net}` : 'net:none') }}
              onPartClick={ref => { engage(`part:${ref}`) }}
            />
          ) : (
            <div className="hb-embed-loading" data-testid="embed-loading">
              opening the recording ...
            </div>
          )}
          <div className="hb-embed-footer">
            <span className="hb-embed-caption" data-testid="embed-honesty">{HONESTY_LINE}</span>
            <button
              type="button"
              className="hb-embed-cta"
              data-testid="embed-expand"
              onClick={() => expand('button')}
            >
              Open the full surface
            </button>
          </div>
        </>
      )}

      {state === 'expanded' && !error && (
        <>
          <div className="hb-embed-bar">
            <span className="hb-embed-bar-mark">HAUKSBEE</span>
            <div className="hb-embed-boards" data-testid="embed-boards">
              {index.map(b => (
                <button
                  key={b.id}
                  type="button"
                  className="hb-embed-board"
                  data-active={b.id === boardId}
                  data-testid={`embed-board-${b.id}`}
                  title={b.tagline}
                  onClick={() => {
                    engage(`board:${b.id}`)
                    setBoardId(b.id)
                    setAppEpoch(n => n + 1)
                  }}
                >
                  {b.title}
                </button>
              ))}
            </div>
            <button
              type="button"
              className="hb-embed-collapse"
              data-testid="embed-collapse"
              onClick={collapse}
            >
              Collapse
            </button>
          </div>
          {presets.length > 1 && (
            <div className="hb-embed-specs" data-testid="embed-presets">
              <span className="hb-embed-specs-label">Recorded specs</span>
              {presets.map(p => (
                <button
                  key={p.id}
                  type="button"
                  className="hb-embed-spec"
                  data-active={p.id === presetId}
                  data-testid={`embed-preset-${p.id.replace('preset:', '')}`}
                  title={`${p.note}${p.firmware ? ` (firmware ${p.firmware} staged)` : ' (no firmware staged)'}`}
                  onClick={() => choosePreset(p.id)}
                >
                  {p.label}
                </button>
              ))}
              {activePreset && (
                <span className="hb-embed-specs-note" data-testid="embed-active-preset">
                  {activePreset.note}
                </span>
              )}
            </div>
          )}
          <div className="hb-embed-app" data-testid="embed-app">
            {board && replayHook ? (
              <SimSourceContext.Provider value={replayHook}>
                <AppMount
                  key={`${board.cache.id}:${presetId ?? 'none'}:${appEpoch}`}
                  board={board}
                  stageFirmware={activePreset ? activePreset.firmware !== null : board.firmwareFile !== null}
                  restoreView={lastView.current}
                  onView={v => { lastView.current = v }}
                  onEngage={engage}
                />
              </SimSourceContext.Provider>
            ) : (
              <div className="hb-embed-loading">opening the recording ...</div>
            )}
          </div>
          <div className="hb-embed-footer">
            <span className="hb-embed-caption" data-testid="embed-honesty">
              {HONESTY_LINE}
              {cache && (
                <span className="hb-embed-caption-dim">
                  {' '}Recorded {cache.recorded_at.slice(0, 10)} with {cache.engine_version}.
                </span>
              )}
              {assembledNote && (
                <span className="hb-embed-caption-dim" data-testid="embed-assembled">
                  {' '}That verdict was assembled row by row from recorded per-rule runs
                  of this spec.
                </span>
              )}
            </span>
          </div>
        </>
      )}
    </div>
  )
}

/** The app, plus the one thing the app cannot do for itself here: receive the
 *  board. It is handed over through the app's own hidden file inputs, so the
 *  analysis, the report map, the checks panel and the live surface all take the
 *  exact code path a dropped file takes, holding the real bytes. */
function AppMount({ board, stageFirmware, restoreView, onView, onEngage }: {
  board: LoadedBoard
  /** Stage the recorded firmware alongside the board. False for the specs that
   *  were recorded with no firmware: a run must send what its recording had. */
  stageFirmware: boolean
  /** Nav item to re-enter on, when this mount replaces one the visitor had
   *  already navigated. */
  restoreView: string | null
  /** Reports which surface the visitor navigated to. */
  onView: (view: string) => void
  onEngage: (how: string) => void
}) {
  const hostRef = useRef<HTMLDivElement | null>(null)

  useEffect(() => {
    const host = hostRef.current
    if (!host) return
    let cancelled = false
    let tries = 0

    const put = (id: string, file: File) => {
      const input = host.querySelector<HTMLInputElement>(`#${id}`)
      if (!input) return false
      const dt = new DataTransfer()
      dt.items.add(file)
      input.files = dt.files
      input.dispatchEvent(new Event('change', { bubbles: true }))
      return true
    }

    /** Re-enter the surface the visitor was on, once the app will let it be
     *  opened (the nav item is disabled until the report has landed). */
    const restore = (view: string) => {
      let waited = 0
      const attempt = () => {
        if (cancelled) return
        const btn = host.querySelector<HTMLButtonElement>(`[data-testid="nav-${view}"]`)
        if (btn && !btn.disabled) { btn.click(); return }
        if (++waited > 300) return
        requestAnimationFrame(attempt)
      }
      attempt()
    }

    const tick = () => {
      if (cancelled) return
      if (put('board-file', board.boardFile)) {
        if (board.firmwareFile && stageFirmware) {
          // After the board: the app re-analyses with both, and the second
          // report is the co-sim one.
          const fw = board.firmwareFile
          setTimeout(() => { if (!cancelled) put('firmware-file', fw) }, 60)
        }
        if (restoreView && restoreView !== 'board') restore(restoreView)
        return
      }
      if (++tries > 200) return
      requestAnimationFrame(tick)
    }
    requestAnimationFrame(tick)
    return () => { cancelled = true }
  }, [board, stageFirmware, restoreView])

  // Any click inside the app is a real interaction with a real surface; a click
  // on a nav item is also which surface to come back to.
  return (
    <div
      className="hb-embed-appmount"
      ref={hostRef}
      onPointerDown={e => {
        onEngage('app')
        const nav = (e.target as HTMLElement).closest?.('[data-testid^="nav-"]')
        const id = nav?.getAttribute('data-testid')?.slice(4)
        if (id) onView(id)
      }}
    >
      <App />
    </div>
  )
}

/** Seed the checks panel's own saved state for this board, so the surface opens
 *  on the spec that was actually recorded rather than on an empty builder. The
 *  panel restores this at mount; it is the app's own persistence format. */
function seedChecksPanel(board: LoadedBoard, preset: RecordedRun | null) {
  const cache = board.cache
  // The panel keys its saved state on the board's identity (file name, part and
  // net counts), which is the same in the with- and without-firmware reports of
  // one board; the plain one is always present.
  const report = cache.analyze
  if (!preset) return
  const payload = {
    specName: cache.spec_name,
    duration: cache.duration_ms,
    supplies: cache.supplies,
    checks: preset.rows.map((r, i) => fullRow(r, i + 1)),
    rawMode: false,
    rawText: '',
  }
  try {
    localStorage.setItem(checksStorageKey(report), JSON.stringify(payload))
  } catch { /* storage blocked: the panel opens on its own default instead */ }
  // A cheap guard against the mirror drifting from the panel's composer: if the
  // spec this seed produces is not the one that was recorded, the run would
  // miss the cache. Say so in the console rather than fail silently at click
  // time.
  const mirrored = buildToml(cache.spec_name, cache.duration_ms, cache.supplies, preset.rows)
  if (mirrored.trim() !== preset.toml.trim()) {
    console.warn('[hauksbee-embed] seeded spec differs from the recorded spec text')
  }
}

/** The preset a board opens on. */
const defaultPresetOf = (b: LoadedBoard): RecordedRun | null =>
  b.cache.checks.find(r => r.id === `preset:${b.cache.default_preset}`)
  ?? b.cache.checks.find(r => r.kind === 'preset')
  ?? null

const compactSubject = (boardId: string): string => {
  switch (boardId) {
    case 'watchy': return 'This is the real report for a real smartwatch.'
    case 'boot_gate': return 'This is a real board with a floating MOSFET gate.'
    case 'blinky': return 'This is a real board, mid-blink.'
    default: return 'This is a real board, running.'
  }
}
