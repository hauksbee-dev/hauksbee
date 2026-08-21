import { useCallback, useEffect, useRef, useState } from 'react'
import { buildBoardUpload, type SupplementalDesignFiles } from '../lib/board-upload'
import type { LiveLaunchResponse, LiveStatus, WebReport } from '../types/report'
import type { SelectedComponent } from '../components/SelectionCard'
import { analysisFailureMessage, precheckBoardFile } from '../lib/upload-guard'
import { inspectZip, zipIsFirmwareOnly } from '../lib/zip-inspect'

// The board session: one uploaded (or preloaded) board, its report, its staged
// firmware, and the live-sim affordance for it. The app shell needs this shared
// across the Board, Checks and Live Sim views, so it is a hook the shell owns
// rather than state inside Landing.

// Fetch aborts (a newer run superseded this one) are expected, not errors.
const isAbort = (e: unknown) => e instanceof Error && e.name === 'AbortError'

export interface SampleSpec {
  board: string
  firmware?: string
}

export type LaunchState =
  | { phase: 'idle' }
  | { phase: 'launching' }
  /** A live session for a DIFFERENT (or foreign: stale tab, pre-reload) board
   *  is running server-side; the UI asks in-app before replacing it. A native
   *  window.confirm here was a dead click under any driver that auto-dismisses
   *  dialogs, and unstylable besides. */
  | { phase: 'confirm'; activeBoard: string; targetBoard: string }
  | { phase: 'error'; error: string }

/** The server's one global live session, as last observed. `null` until the
 *  first status fetch resolves (or when the endpoint is unavailable). */
export interface ServerLive {
  active: boolean
  boardName: string | null
}

export type LiveMode = 'connected' | 'launch' | 'none'

/** A report put back on screen from a saved browser session, with no uploaded
 *  file behind it. Held separately from a real run because the difference is
 *  load-bearing: everything that needs the bytes (re-analysis, a checks run, a
 *  live launch) is unavailable, and the surfaces have to say so instead of
 *  offering buttons that cannot work. */
export interface RestoredFrom {
  /** The board file the report was produced from, by name. */
  boardName: string
  /** The firmware that was staged at the time, by name. */
  firmwareName: string | null
  /** The companion schematic/project that was staged at the time, by name. */
  schematicName: string | null
  /** The saved session's own name. */
  sessionName: string
}

export interface BoardSession {
  report: WebReport | null
  /** In-progress upload; while set, further uploads are blocked. */
  busy: { board: string; firmware: string | null } | null
  uploadError: string | null
  /** Something the app DID on the user's behalf that they did not ask for and
   *  would otherwise have to infer (a project zip dropped on the board zone
   *  wired up as firmware instead). Not an error: the drop worked, it just did
   *  not do the literal thing. Cleared by the next run. */
  uploadNotice: string | null
  dismissNotice: () => void
  firmwareFile: File | null
  schematicFile: File | null
  supplementalFiles: SupplementalDesignFiles
  /** The uploaded board File (null when the server preloaded the board). */
  boardFile: File | null
  /** Best display name for the current board. */
  boardLabel: string | null
  /** URL the BoardViewer can draw (KiCad text only), else null. */
  boardUrl: string | null
  /** Client clock when the current report landed. */
  analyzedAt: number | null
  launch: LaunchState
  liveMode: LiveMode
  /** The server's one global live session (board name + active), kept fresh
   *  on load / focus / a slow poll, so every live affordance binds to what
   *  the server actually runs, never to a stale client-side guess. */
  serverLive: ServerLive | null
  /** Re-fetch `/api/live/status` now (e.g. after opening the sim view). */
  refreshLiveStatus: () => void
  /** Resolve a pending 'confirm' launch: replace the running session. */
  confirmReplace: () => void
  /** Resolve a pending 'confirm' launch: keep the running session. */
  cancelLaunch: () => void
  /** Launch the current board, REPLACING any running session without asking
   *  (the user already said so, e.g. "relaunch with this board"). */
  forceLaunch: (onReady: () => void) => void
  selectedNet: string | null
  selectedComponent: SelectedComponent | null
  setSelectedNet: (net: string | null) => void
  setSelectedComponent: (c: SelectedComponent | null) => void
  handleBoard: (f: File) => void
  handleFirmware: (f: File) => void
  handleSchematic: (f: File) => void
  handleBom: (f: File | null) => void
  handlePlacement: (f: File | null) => void
  handleVariant: (f: File | null) => void
  handleAsbuilt: (f: File | null) => void
  handleModels: (files: File[]) => void
  /** Unstage the firmware WITHOUT touching the board, and re-analyse the board
   *  on its own so the report stops describing a co-sim that is no longer
   *  loaded. No-op when nothing is staged. */
  clearFirmware: () => void
  clearSchematic: () => void
  /** Re-run the exact current board and staged companions, e.g. after a model
   *  is saved, without making the user upload the board again. */
  reanalyzeCurrent: () => void
  runSample: (s: SampleSpec) => void
  resetFlow: () => void
  /** Put a saved session's report back on screen with no file behind it. */
  restoreReport: (from: RestoredFrom & { report: WebReport; analyzedAt: number | null }) => void
  /** Set while the report on screen came from storage rather than a run. */
  restoredFrom: RestoredFrom | null
  /** Bumped once per analysis run. Anything outside this hook that caches
   *  run-derived state keys off it to drop that state at the same instant the
   *  session drops its own, so no surface can show two runs at once. */
  runEpoch: number
  /** Launch (or reconnect to) the live sim; onReady fires once it is live. */
  launchLive: (onReady: () => void) => void
  /** The KiCad file parsed to nothing drawable: fall back to the dot map. */
  onEmptyBoard: () => void
}

export function useBoardSession(opts: {
  preloadedReport: WebReport | null
  preloadedBoardName: string | null
  sessionPreloaded: boolean
  canLaunchLive: boolean
}): BoardSession {
  const { preloadedReport, preloadedBoardName, sessionPreloaded, canLaunchLive } = opts

  const [report, setReport] = useState<WebReport | null>(preloadedReport)
  const [busy, setBusy] = useState<{ board: string; firmware: string | null } | null>(null)
  const [uploadError, setUploadError] = useState<string | null>(null)
  const [uploadNotice, setUploadNotice] = useState<string | null>(null)
  const [firmwareFile, setFirmwareFile] = useState<File | null>(null)
  const [schematicFile, setSchematicFile] = useState<File | null>(null)
  const [supplementalFiles, setSupplementalFiles] = useState<SupplementalDesignFiles>({
    bom: null,
    placement: null,
    variant: null,
    asbuilt: null,
    models: [],
  })
  const supplementalRef = useRef<SupplementalDesignFiles>(supplementalFiles)
  const [boardFile, setBoardFile] = useState<File | null>(null)
  const [analyzedAt, setAnalyzedAt] = useState<number | null>(preloadedReport ? Date.now() : null)
  const lastBoardFile = useRef<File | null>(null)
  // Object URL of the uploaded board WHEN it is KiCad layout text: the report's
  // board map then uses the real BoardViewer renderer instead of the dot map.
  const [boardUrl, setBoardUrl] = useState<string | null>(null)
  const [selectedNet, setSelectedNet] = useState<string | null>(null)
  const [selectedComponent, setSelectedComponentRaw] = useState<SelectedComponent | null>(null)
  const [runEpoch, setRunEpoch] = useState(0)
  const [restoredFrom, setRestoredFrom] = useState<RestoredFrom | null>(null)

  // The board whose live session is currently on /ws AND was launched by THIS
  // page-load (or preloaded by `run --serve`). Only this counts as "connected":
  // a session left by a previous page-load or another tab is a foreign session
  // even when the file name happens to match (the content may differ).
  const [liveBoard, setLiveBoard] = useState<string | null>(
    sessionPreloaded ? preloadedBoardName : null,
  )
  const [launch, setLaunch] = useState<LaunchState>({ phase: 'idle' })

  // The server's one global live session, observed from /api/live/status.
  // Fetched on load, on window focus, and on a slow poll while visible, so
  // the header chips and the nav item stay honest about what /ws is serving.
  const [serverLive, setServerLive] = useState<ServerLive | null>(null)
  const refreshLiveStatus = useCallback(() => {
    void (async () => {
      try {
        const res = await fetch('/api/live/status')
        if (!res.ok) throw new Error(String(res.status))
        const st = await res.json() as LiveStatus
        setServerLive({ active: st.active === true, boardName: st.board_name ?? null })
      } catch {
        // No status endpoint (older server): leave unknown rather than lying.
        setServerLive(null)
      }
    })()
  }, [])
  useEffect(() => {
    refreshLiveStatus()
    const onFocus = () => refreshLiveStatus()
    window.addEventListener('focus', onFocus)
    const t = setInterval(() => {
      if (!document.hidden) refreshLiveStatus()
    }, 15000)
    return () => {
      window.removeEventListener('focus', onFocus)
      clearInterval(t)
    }
  }, [refreshLiveStatus])

  // One floating selection at a time: a net click clears the part selection
  // and vice versa.
  const selectNet = useCallback((net: string | null) => {
    setSelectedNet(net)
    if (net) setSelectedComponentRaw(null)
  }, [])
  const selectComponent = useCallback((c: SelectedComponent | null) => {
    setSelectedComponentRaw(c)
    if (c) setSelectedNet(null)
  }, [])

  // Request-race guard: uploads are BLOCKED while one is in flight, but
  // "Analyze another board" aborts the current run, so a stale response must
  // never overwrite a newer state. Monotonic run ids; only the latest wins.
  const runIdRef = useRef(0)
  const abortRef = useRef<AbortController | null>(null)
  const beginRun = useCallback(() => {
    abortRef.current?.abort()
    const ctrl = new AbortController()
    abortRef.current = ctrl
    runIdRef.current += 1
    const id = runIdRef.current
    return { signal: ctrl.signal, isCurrent: () => runIdRef.current === id }
  }, [])

  // Everything on screen that was DERIVED from the previous run, dropped in one
  // place. Without it, uploading again leaves the old report, its error banner
  // and the net you had clicked on the old board sitting there while the new
  // analysis runs, so the page shows two runs at once and there is no way to
  // tell which half you were reading. A new run starts from nothing.
  //
  // This deliberately does NOT touch the run's INPUTS (boardFile, firmwareFile,
  // lastBoardFile), which the caller has just set, nor the live session
  // (liveBoard/serverLive), which belongs to the server and outlives an
  // analysis. `runEpoch` lets the shell drop its own run-derived state
  // (queued checks, the checks summary) off the same signal.
  const clearRunState = useCallback(() => {
    setReport(null)
    setAnalyzedAt(null)
    setUploadError(null)
    setUploadNotice(null)
    setSelectedNet(null)
    setSelectedComponentRaw(null)
    setLaunch({ phase: 'idle' })
    // A real run replaces a restored report, and with it the "this came from
    // storage" caveat: the surfaces would otherwise keep telling the user to
    // re-drop a file they just dropped.
    setRestoredFrom(null)
    setRunEpoch(n => n + 1)
  }, [])

  const analyze = useCallback(async (
    board: File,
    firmware: File | null,
    schematic: File | null,
  ) => {
    const { signal, isCurrent } = beginRun()
    clearRunState()
    setBusy({ board: board.name, firmware: firmware?.name ?? null })
    // Sniff the head for KiCad layout text to pick the report map's renderer.
    try {
      const head = new TextDecoder().decode(await board.slice(0, 64).arrayBuffer())
      const isKicadPcb = /^\s*\(kicad_pcb/.test(head)
      if (isCurrent()) {
        setBoardUrl(prev => {
          if (prev?.startsWith('blob:')) URL.revokeObjectURL(prev)
          return isKicadPcb ? URL.createObjectURL(board) : null
        })
      }
    } catch {
      if (isCurrent()) setBoardUrl(null)
    }
    // Carried out of the try so the catch can name the status it failed on.
    let status: number | undefined
    try {
      let res: Response
      if (firmware || schematic || supplementalRef.current.bom
        || supplementalRef.current.placement || supplementalRef.current.variant || supplementalRef.current.asbuilt
        || supplementalRef.current.models.length > 0) {
        const fd = buildBoardUpload(board, firmware, schematic, supplementalRef.current)
        res = await fetch('/api/analyze-with-firmware', { method: 'POST', body: fd, signal })
      } else {
        res = await fetch('/api/analyze', {
          method: 'POST',
          headers: { 'X-Board-Filename': board.name, 'Content-Type': 'application/octet-stream' },
          // The File itself, NOT `await board.arrayBuffer()`. A Blob body is
          // streamed off disk by the browser; arrayBuffer() pulled the whole
          // upload into the JS heap on the main thread first, which froze the
          // page for over seven minutes on a 300 MB file and showed nothing
          // while it did. The wire format is byte-identical either way, so the
          // `/api/analyze` contract (raw body + X-Board-Filename) is untouched.
          body: board,
          signal,
        })
      }
      status = res.status
      // Read the body ONCE as text, then parse defensively: a stale build, a
      // proxy, or a body-limit/panic can return plaintext, and res.json() on
      // that throws a cryptic SyntaxError instead of showing the real message.
      const text = await res.text()
      if (!res.ok) {
        throw new Error(text.trim().slice(0, 400) || `${res.status} ${res.statusText}`)
      }
      let parsed: WebReport
      try {
        parsed = JSON.parse(text) as WebReport
      } catch {
        throw new Error(
          text.trim().slice(0, 400) || 'the server returned an empty or non-JSON response',
        )
      }
      if (isCurrent()) {
        setReport(parsed)
        setAnalyzedAt(Date.now())
      }
    } catch (e) {
      // An abort means the flow was reset mid-run: not an error to show.
      if (isAbort(e)) return
      // A body-limit refusal, a dropped connection and a real analysis failure
      // are three different problems with three different next steps; they all
      // used to arrive as "Analysis failed: Failed to fetch".
      if (isCurrent()) setUploadError(analysisFailureMessage(e, { status, size: board.size }))
    } finally {
      if (isCurrent()) setBusy(null)
    }
  }, [beginRun, clearRunState])

  const looksLikeFirmware = (name: string) => /\.(elf|hex)$/i.test(name)

  const handleFirmware = useCallback((f: File) => {
    // One upload at a time: a swap mid-analysis would race the report it
    // replaces.
    if (busy) return
    setFirmwareFile(f)
    if (lastBoardFile.current) void analyze(lastBoardFile.current, f, schematicFile)
  }, [analyze, busy, schematicFile])

  const handleSchematic = useCallback((file: File) => {
    if (busy) return
    setSchematicFile(file)
    if (lastBoardFile.current) void analyze(lastBoardFile.current, firmwareFile, file)
  }, [analyze, busy, firmwareFile])

  const updateSupplemental = useCallback((next: SupplementalDesignFiles) => {
    if (busy) return
    supplementalRef.current = next
    setSupplementalFiles(next)
    if (lastBoardFile.current) {
      void analyze(lastBoardFile.current, firmwareFile, schematicFile)
    }
  }, [analyze, busy, firmwareFile, schematicFile])

  const handleBom = useCallback((file: File | null) => {
    updateSupplemental({ ...supplementalRef.current, bom: file })
  }, [updateSupplemental])
  const handlePlacement = useCallback((file: File | null) => {
    updateSupplemental({ ...supplementalRef.current, placement: file })
  }, [updateSupplemental])
  const handleVariant = useCallback((file: File | null) => {
    updateSupplemental({ ...supplementalRef.current, variant: file })
  }, [updateSupplemental])
  const handleAsbuilt = useCallback((file: File | null) => {
    updateSupplemental({ ...supplementalRef.current, asbuilt: file })
  }, [updateSupplemental])
  const handleModels = useCallback((files: File[]) => {
    updateSupplemental({ ...supplementalRef.current, models: files })
  }, [updateSupplemental])

  const reanalyzeCurrent = useCallback(() => {
    if (busy || !lastBoardFile.current) return
    void analyze(lastBoardFile.current, firmwareFile, schematicFile)
  }, [analyze, busy, firmwareFile, schematicFile])

  const clearFirmware = useCallback(() => {
    // Removing the firmware is a real change to what was analysed, not just a
    // change to a form field: the standing report describes a co-sim of the
    // image being removed. Re-run the board bare so the two agree.
    if (busy) return
    setFirmwareFile(prev => {
      if (!prev) return prev
      if (lastBoardFile.current) void analyze(lastBoardFile.current, null, schematicFile)
      return null
    })
  }, [analyze, busy, schematicFile])

  const clearSchematic = useCallback(() => {
    if (busy) return
    setSchematicFile(previous => {
      if (!previous) return previous
      if (lastBoardFile.current) void analyze(lastBoardFile.current, firmwareFile, null)
      return null
    })
  }, [analyze, busy, firmwareFile])

  /** Accept `f` as the board and run it. Split out of `handleBoard` so the
   *  zip-classification path (which has to await a read) reaches the same
   *  code, rather than a second copy of it that can drift. */
  const acceptBoard = useCallback((f: File) => {
    // Switching boards must not carry the previous board's firmware or clicked
    // net along: the new board would silently co-sim the OLD firmware image.
    // Firmware staged before the FIRST board is a deliberate pairing, keep it.
    const switchingBoards = lastBoardFile.current !== null
    lastBoardFile.current = f
    setBoardFile(f)
    if (switchingBoards) {
      setFirmwareFile(null)
      setSchematicFile(null)
      const empty = { bom: null, placement: null, variant: null, asbuilt: null, models: [] }
      supplementalRef.current = empty
      setSupplementalFiles(empty)
      setSelectedNet(null)
      setSelectedComponentRaw(null)
      void analyze(f, null, null)
    } else {
      void analyze(f, firmwareFile, schematicFile)
    }
  }, [analyze, firmwareFile, schematicFile])

  const handleBoard = useCallback((f: File) => {
    if (busy) return
    // Everything the browser can know for certain about this file, before a
    // byte of it is read or sent: empty, past the server's body limit, or a
    // large file with an extension nothing claims. A 300 MB CAD export used to
    // get all the way to a seven-minute frozen tab and then a 413.
    const refusal = precheckBoardFile(f)
    if (refusal) {
      abortRef.current?.abort()
      runIdRef.current += 1
      clearRunState()
      setBusy(null)
      setUploadError(refusal)
      return
    }
    // A firmware file in the board slot is a mis-drop, not a board: route it
    // to the firmware jack instead of sending an ELF to the board extractor.
    if (looksLikeFirmware(f.name)) {
      handleFirmware(f)
      return
    }
    // A zip is ambiguous by design: a gerber package and a firmware project
    // both arrive as one. Read the archive's own file list (a tail read, not a
    // decompression) and route on what is actually in it. Without this, a
    // PlatformIO project dropped here got a gerber complaint about a zip that
    // has no copper in it and never will.
    if (/\.zip$/i.test(f.name)) {
      void (async () => {
        const z = await inspectZip(f)
        if (zipIsFirmwareOnly(z)) {
          handleFirmware(f)
          setUploadNotice(
            `“${f.name}” holds ${z.firmwareMarkers.join(', ')} and no board or fab files, `
            + 'so it was wired up as FIRMWARE rather than as the board. Drop a board '
            + `(or pick a sample) and it will be co-simulated. If you meant it as a `
            + 'gerber package, re-export the copper layers and drill file into the zip.',
          )
          return
        }
        acceptBoard(f)
      })()
      return
    }
    acceptBoard(f)
  }, [acceptBoard, busy, clearRunState, handleFirmware])

  // "Analyze another board": resolve the finished flow back to the drop zone.
  // A running live session keeps running server-side until a new launch
  // replaces it.
  const resetFlow = useCallback(() => {
    abortRef.current?.abort()
    runIdRef.current += 1
    lastBoardFile.current = null
    clearRunState()
    setBoardFile(null)
    setBusy(null)
    setFirmwareFile(null)
    setSchematicFile(null)
    const empty = { bom: null, placement: null, variant: null, asbuilt: null, models: [] }
    supplementalRef.current = empty
    setSupplementalFiles(empty)
    setBoardUrl(prev => {
      if (prev?.startsWith('blob:')) URL.revokeObjectURL(prev)
      return null
    })
  }, [clearRunState])

  // Put a saved session's report back on screen. Deliberately the same teardown
  // `resetFlow` does (no file, no firmware, no board URL, a fresh run epoch)
  // followed by the stored report: a restored session must not inherit a single
  // artifact of whatever was loaded before it, and it must not pretend to have a
  // file. `restoredFrom` is what every surface reads to say so.
  const restoreReport = useCallback((from: RestoredFrom & { report: WebReport; analyzedAt: number | null }) => {
    abortRef.current?.abort()
    runIdRef.current += 1
    lastBoardFile.current = null
    clearRunState()
    setBoardFile(null)
    setBusy(null)
    setFirmwareFile(null)
    setSchematicFile(null)
    const empty = { bom: null, placement: null, variant: null, asbuilt: null, models: [] }
    supplementalRef.current = empty
    setSupplementalFiles(empty)
    setBoardUrl(prev => {
      if (prev?.startsWith('blob:')) URL.revokeObjectURL(prev)
      return null
    })
    setReport(from.report)
    setAnalyzedAt(from.analyzedAt)
    setRestoredFrom({
      boardName: from.boardName,
      firmwareName: from.firmwareName,
      schematicName: from.schematicName,
      sessionName: from.sessionName,
    })
  }, [clearRunState])

  // One-click samples: fetch a bundled board (and optionally its firmware)
  // and push it through the exact same analyze path a dropped file takes.
  const runSample = useCallback(async (sample: SampleSpec) => {
    if (busy) return
    const { signal, isCurrent } = beginRun()
    // Cleared here as well as inside `analyze`: the sample's files are fetched
    // first, and a fetch that fails must not leave the previous board's report
    // on screen under the new error.
    clearRunState()
    const empty = { bom: null, placement: null, variant: null, asbuilt: null, models: [] }
    supplementalRef.current = empty
    setSupplementalFiles(empty)
    setSelectedNet(null)
    setSelectedComponentRaw(null)
    setBusy({
      board: sample.board.split('/').pop() ?? 'sample',
      firmware: sample.firmware?.split('/').pop() ?? null,
    })
    try {
      const bres = await fetch(sample.board, { signal })
      if (!bres.ok) throw new Error(`could not fetch ${sample.board}: ${bres.status}`)
      const bname = sample.board.split('/').pop() ?? 'sample.kicad_pcb'
      const board = new File([await bres.blob()], bname)
      let fw: File | null = null
      if (sample.firmware) {
        const fres = await fetch(sample.firmware, { signal })
        if (!fres.ok) throw new Error(`could not fetch ${sample.firmware}: ${fres.status}`)
        const fname = sample.firmware.split('/').pop() ?? 'firmware.hex'
        fw = new File([await fres.blob()], fname)
      }
      // The flow was reset while the sample files were downloading: hand
      // nothing over (a newer run owns the board/firmware slots now).
      if (!isCurrent()) return
      lastBoardFile.current = board
      setBoardFile(board)
      setFirmwareFile(fw)
      await analyze(board, fw, null)
    } catch (e) {
      if (isAbort(e)) return
      if (isCurrent()) {
        setUploadError(`Could not load the sample: ${e instanceof Error ? e.message : String(e)}`)
        setBusy(null)
      }
    }
  }, [analyze, beginRun, busy, clearRunState])

  // The actual POST to /api/live/launch (no questions asked). Every path
  // resolves the phase; never a spinner forever.
  const performLaunch = useCallback(async (board: File, onReady: () => void) => {
    setLaunch({ phase: 'launching' })
    try {
      const fd = buildBoardUpload(board, firmwareFile, schematicFile, supplementalRef.current)
      const res = await fetch('/api/live/launch', { method: 'POST', body: fd })
      const text = await res.text()
      let parsed: LiveLaunchResponse
      try {
        parsed = JSON.parse(text) as LiveLaunchResponse
      } catch {
        throw new Error(text.trim().slice(0, 400) || `${res.status} ${res.statusText}`)
      }
      if (!parsed.ok) throw new Error(parsed.error || 'the live launch failed')
      setLiveBoard(parsed.board_name ?? board.name)
      setServerLive({ active: true, boardName: parsed.board_name ?? board.name })
      setLaunch({ phase: 'idle' })
      onReady()
    } catch (e) {
      setLaunch({ phase: 'error', error: e instanceof Error ? e.message : String(e) })
    }
  }, [firmwareFile, schematicFile])

  // A launch waiting on the in-app replace confirmation.
  const pendingReady = useRef<(() => void) | null>(null)

  // Launch (or reconnect to) the live sim for the current report's board.
  // Server-side session, one at a time. Launching over a session THIS page
  // did not start for this board (another board, a stale tab, a pre-reload
  // launch) surfaces an in-app confirm instead of silently doing nothing:
  // window.confirm was auto-dismissed by automation drivers, which made the
  // whole live surface a dead click on the second board.
  const launchLive = useCallback(async (onReady: () => void) => {
    const board = lastBoardFile.current
    // Preloaded (`run --serve`) report with no re-upload: the session is
    // already running; just expand into it. Same when this exact upload was
    // already launched.
    if (!board) {
      if (sessionPreloaded) onReady()
      return
    }
    if (liveBoard === board.name) {
      onReady()
      return
    }
    try {
      const st = await fetch('/api/live/status').then(r => r.json()) as LiveStatus
      if (st.active) {
        setServerLive({ active: true, boardName: st.board_name ?? null })
        pendingReady.current = onReady
        setLaunch({
          phase: 'confirm',
          activeBoard: st.board_name ?? 'another board',
          targetBoard: board.name,
        })
        return
      }
      setServerLive({ active: false, boardName: null })
    } catch {
      // Status unavailable: proceed; the launch itself is authoritative.
    }
    await performLaunch(board, onReady)
  }, [liveBoard, performLaunch, sessionPreloaded])

  const confirmReplace = useCallback(() => {
    const board = lastBoardFile.current
    const onReady = pendingReady.current
    pendingReady.current = null
    if (!board) {
      setLaunch({ phase: 'idle' })
      return
    }
    void performLaunch(board, onReady ?? (() => {}))
  }, [performLaunch])

  const cancelLaunch = useCallback(() => {
    pendingReady.current = null
    setLaunch({ phase: 'idle' })
  }, [])

  // Replace the running session with the current board WITHOUT asking again:
  // used by affordances whose label already states the replacement ("Relaunch
  // with this board" on the sim's wrong-board banner).
  const forceLaunch = useCallback((onReady: () => void) => {
    const board = lastBoardFile.current
    if (!board) {
      if (sessionPreloaded) onReady()
      return
    }
    void performLaunch(board, onReady)
  }, [performLaunch, sessionPreloaded])

  // What the live affordance is for THIS board:
  //  - 'connected': the session on /ws is this board; the action reconnects.
  //  - 'launch': the server can boot a session for this upload.
  //  - 'none': no live capability; the CLI hint remains.
  const liveMode: LiveMode = (() => {
    const board = lastBoardFile.current
    if (!board) return sessionPreloaded ? 'connected' : 'none'
    if (liveBoard === board.name) return 'connected'
    return canLaunchLive ? 'launch' : 'none'
  })()

  const onEmptyBoard = useCallback(() => setBoardUrl(null), [])

  return {
    report,
    busy,
    uploadError,
    uploadNotice,
    dismissNotice: () => setUploadNotice(null),
    firmwareFile,
    schematicFile,
    supplementalFiles,
    boardFile,
    // A failed analysis must not crown its (possibly garbage) filename as the
    // header's board title: the title names a board this app can speak about,
    // and a rejected upload is not one. While the upload is still analyzing
    // the name may show (the busy line names it anyway).
    boardLabel: (report && !report.ok) || (uploadError && !report)
      ? null
      : boardFile?.name ?? restoredFrom?.boardName ?? preloadedBoardName,
    boardUrl: boardUrl ?? (
      // Preloaded (`run --serve`) boards are served at /boards/<name> for the
      // live viewer; reuse that for the report map too.
      preloadedBoardName?.endsWith('.kicad_pcb') && !boardFile
        ? `/boards/${preloadedBoardName}`
        : null
    ),
    analyzedAt,
    launch,
    liveMode,
    serverLive,
    refreshLiveStatus,
    confirmReplace,
    cancelLaunch,
    forceLaunch,
    selectedNet,
    selectedComponent,
    setSelectedNet: selectNet,
    setSelectedComponent: selectComponent,
    handleBoard,
    handleFirmware,
    handleSchematic,
    handleBom,
    handlePlacement,
    handleVariant,
    handleAsbuilt,
    handleModels,
    clearFirmware,
    clearSchematic,
    reanalyzeCurrent,
    runSample: (s: SampleSpec) => void runSample(s),
    resetFlow,
    restoreReport,
    restoredFrom,
    runEpoch,
    launchLive: (onReady: () => void) => void launchLive(onReady),
    onEmptyBoard,
  }
}
