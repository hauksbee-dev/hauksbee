import { useCallback, useRef, useState } from 'react'
import type { WebReport } from '../types/report'
import { PlusIcon, BoardTargetIcon } from './Icons'
import { ChecksPanel, checksStorageKey } from './ChecksPanel'
import { DepsPanel } from './DepsPanel'
import { FirmwareJack } from './FirmwareJack'
import { ReportView } from './ReportView'

// The landing state: the drop-a-board flow, absorbed from the old
// server-rendered front door into the React app. It gets a board (and
// optionally firmware) to /api/analyze and hands the WebReport to ReportView.
// The "run it" affordance expands the report into the live-sim view (SimView).

interface LandingProps {
  /** Report preloaded by the server (`run --serve`), if any. */
  preloadedReport: WebReport | null
  /** Board name the server preloaded, if any. */
  preloadedBoardName: string | null
  /** True when a live sim is being served on /ws (run --serve). */
  canRunLive: boolean
  /** Expand into the live-sim view. */
  onRunIt: () => void
}

// Bundled one-click samples (files under frontend/public/samples/, see its
// README for provenance). A ladder: tiny clean board → real product → a
// board + firmware pair that exercises the co-sim.
// Fetch aborts (a newer run superseded this one) are expected, not errors.
const isAbort = (e: unknown) => e instanceof Error && e.name === 'AbortError'

const SAMPLES: { label: string; desc: string; board: string; firmware?: string }[] = [
  { label: 'Blinky', desc: 'small clean board', board: '/samples/blinky.kicad_pcb' },
  { label: 'Watchy', desc: 'a real smartwatch', board: '/samples/watchy.kicad_pcb' },
  {
    label: 'Boot gate + firmware',
    desc: 'live co-sim',
    board: '/samples/boot_gate.kicad_pcb',
    firmware: '/samples/boot_gate.hex',
  },
]

// Keyboard activation for the label-wrapped file inputs (U3 a11y): a styled
// <label> is clickable by mouse and screen-reader but is not keyboard-focusable
// on its own, so we give it role="button" + tabIndex and trigger the hidden
// input on Enter/Space (the same activation the label already does on click).
function activateOnEnterSpace(inputId: string) {
  return (e: React.KeyboardEvent) => {
    if (e.key === 'Enter' || e.key === ' ') {
      e.preventDefault()
      document.getElementById(inputId)?.click()
    }
  }
}

export function Landing({ preloadedReport, preloadedBoardName, canRunLive, onRunIt }: LandingProps) {
  const [report, setReport] = useState<WebReport | null>(preloadedReport)
  const [busy, setBusy] = useState<string | null>(null)
  const [uploadError, setUploadError] = useState<string | null>(null)
  const [dragOver, setDragOver] = useState(false)
  const [firmwareFile, setFirmwareFile] = useState<File | null>(null)
  const lastBoardFile = useRef<File | null>(null)
  // Object URL of the uploaded board WHEN it is KiCad layout text: the report's
  // board map then uses the real BoardViewer renderer (pads, outline, pan/zoom)
  // instead of the dot map. Null for formats the client can't draw (Eagle,
  // Altium, gerber zip, .board DSL).
  const [boardUrl, setBoardUrl] = useState<string | null>(null)
  // Net last clicked on the board render — the checks panel offers a one-click
  // check on it ("click a trace and say what the voltage must be").
  const [selectedNet, setSelectedNet] = useState<string | null>(null)

  // Request-race guard for analyze/runSample: when the user starts a new run
  // mid-flight (drops another board, swaps firmware, clicks a sample), the
  // in-flight responses can land out of order and a stale response would
  // overwrite the newer report (and a stale `finally` would clear the newer
  // busy spinner). Each run takes a monotonic id and only the latest run may
  // touch state; the previous run's fetch is aborted so it cannot land late.
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

  const analyze = useCallback(async (board: File, firmware: File | null) => {
    const { signal, isCurrent } = beginRun()
    setUploadError(null)
    setBusy(firmware ? `Analyzing ${board.name} + co-sim of ${firmware.name} ...` : `Analyzing ${board.name} ...`)
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
    try {
      let res: Response
      if (firmware) {
        const fd = new FormData()
        fd.append('board', board, board.name)
        fd.append('firmware', firmware, firmware.name)
        res = await fetch('/api/analyze-with-firmware', { method: 'POST', body: fd, signal })
      } else {
        res = await fetch('/api/analyze', {
          method: 'POST',
          headers: { 'X-Board-Filename': board.name, 'Content-Type': 'application/octet-stream' },
          body: await board.arrayBuffer(),
          signal,
        })
      }
      // Read the body ONCE as text, then parse defensively. The server always
      // returns a WebReport JSON (even for a bad board: `{ok:false,...}`), but a
      // stale build, a proxy, or a body-limit/panic can return plaintext ("413
      // Payload Too Large", a "Failed to ..." message). Parsing that with
      // res.json() throws a cryptic "Unexpected token 'F'" SyntaxError; reading
      // text first lets us show the real message verbatim.
      const text = await res.text()
      if (!res.ok) {
        throw new Error(text.trim().slice(0, 400) || `${res.status} ${res.statusText}`)
      }
      let parsed: WebReport
      try {
        parsed = JSON.parse(text) as WebReport
      } catch {
        throw new Error(
          text.trim().slice(0, 400) ||
            'the server returned an empty or non-JSON response',
        )
      }
      if (isCurrent()) setReport(parsed)
    } catch (e) {
      // An abort means a newer run superseded this one: not an error to show.
      if (isAbort(e)) return
      if (isCurrent()) setUploadError(`Analysis failed: ${e instanceof Error ? e.message : String(e)}`)
    } finally {
      if (isCurrent()) setBusy(null)
    }
  }, [beginRun])

  const looksLikeFirmware = (name: string) => /\.(elf|hex)$/i.test(name)

  const handleFirmware = useCallback((f: File) => {
    setFirmwareFile(f)
    if (lastBoardFile.current) void analyze(lastBoardFile.current, f)
  }, [analyze])

  const handleBoard = useCallback((f: File) => {
    // A firmware file in the board slot is a mis-drop, not a board: route it to
    // the firmware jack instead of sending an ELF to the board extractor. This
    // holds whether or not a board is already loaded ("check another board"
    // reuses the same input, and an .elf dropped there is still firmware).
    if (looksLikeFirmware(f.name)) {
      handleFirmware(f)
      return
    }
    // Switching boards must not carry the previous board's firmware or clicked
    // net along: the new board would silently co-sim the OLD firmware image.
    // Firmware staged before the FIRST board is a deliberate pairing, keep it.
    const switchingBoards = lastBoardFile.current !== null
    lastBoardFile.current = f
    if (switchingBoards) {
      setFirmwareFile(null)
      setSelectedNet(null)
      void analyze(f, null)
    } else {
      void analyze(f, firmwareFile)
    }
  }, [analyze, firmwareFile, handleFirmware])

  // One-click samples: fetch a bundled board (and optionally its firmware)
  // from /samples/ and push it through the exact same analyze path a dropped
  // file takes, so the first report needs no file at all.
  const runSample = useCallback(async (sample: { board: string; firmware?: string }) => {
    const { signal, isCurrent } = beginRun()
    setUploadError(null)
    setBusy('Fetching the sample ...')
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
      // A newer run started while the sample files were downloading: hand
      // nothing over (the newer run owns lastBoardFile/firmware now).
      if (!isCurrent()) return
      lastBoardFile.current = board
      setFirmwareFile(fw)
      await analyze(board, fw)
    } catch (e) {
      if (isAbort(e)) return
      if (isCurrent()) {
        setUploadError(`Could not load the sample: ${e instanceof Error ? e.message : String(e)}`)
        setBusy(null)
      }
    }
  }, [analyze, beginRun])

  return (
    <div
      // h-screen (not min-h-screen): #root is height:100% + overflow:hidden for
      // the full-screen live viewer, so a min-h container grows past the clipped
      // viewport and the report below the fold becomes unreachable. A fixed
      // viewport-height container makes overflow-y-auto actually scroll.
      className="landing h-screen overflow-y-auto"
      style={{
        background: 'radial-gradient(130% 90% at 50% -20%, #13151c 0%, var(--canvas) 55%)',
        color: 'var(--silk)',
        fontFamily: 'var(--font-sans)',
      }}
    >
      {/* Top bar */}
      <div className="w-full max-w-5xl mx-auto px-6 flex items-center justify-between" style={{ height: 64 }}>
        <div className="flex items-center gap-2.5">
          <span
            style={{
              width: 9, height: 9, borderRadius: 2, background: 'var(--copper)',
              boxShadow: '0 0 12px var(--copper-hi)', display: 'inline-block',
            }}
          />
          <span
            className="text-[13px] font-semibold tracking-[0.28em]"
            style={{ color: 'var(--silk)', fontFamily: 'var(--font-mono)' }}
          >
            HAUKSBEE
          </span>
        </div>
        <span
          className="text-[10px] font-semibold tracking-[0.22em] px-2.5 py-1 rounded-md"
          style={{
            color: 'var(--copper)', border: '1px solid var(--hairline)',
            background: 'rgba(224,138,78,0.05)', fontFamily: 'var(--font-mono)',
          }}
        >
          CI FOR HARDWARE
        </span>
      </div>

      <div className={`${report ? 'max-w-3xl' : 'max-w-xl'} mx-auto px-6 pb-24`} style={{ paddingTop: report ? '2rem' : 'clamp(2.5rem, 9vh, 6rem)' }}>
        {/* Hero — a calm, centered thesis */}
        {!report && (
          <div className="text-center">
            <h1
              className="font-semibold"
              style={{ color: 'var(--silk)', fontSize: 'clamp(1.9rem, 5vw, 2.6rem)', lineHeight: 1.1, letterSpacing: '-0.02em' }}
            >
              Point it at a real board.
            </h1>
            <p className="mt-4 text-[15px] leading-relaxed mx-auto" style={{ color: 'var(--silk-dim)', maxWidth: '30rem' }}>
              Hauksbee reconstructs the circuit from the copper the board actually
              ships, co-simulates your firmware on an emulated MCU, and checks it like a
              test suite. Not a DRC. Not schematic SPICE.
            </p>
          </div>
        )}

        {/* The upload card — the single focal action */}
        {!report && (
          <div className="mt-9">
            <label
              data-testid="drop-zone"
              htmlFor="board-file"
              role="button"
              tabIndex={0}
              aria-label="Choose a board file to analyze"
              onKeyDown={activateOnEnterSpace('board-file')}
              onDragEnter={e => { e.preventDefault(); setDragOver(true) }}
              onDragOver={e => { e.preventDefault(); setDragOver(true) }}
              onDragLeave={e => { e.preventDefault(); setDragOver(false) }}
              onDrop={e => {
                e.preventDefault()
                setDragOver(false)
                const f = e.dataTransfer.files[0]
                if (f) handleBoard(f)
              }}
              className="drop-card block cursor-pointer px-8 py-11 text-center"
              data-active={dragOver ? 'true' : 'false'}
            >
              {/* icon in a soft copper disc */}
              <span
                className="inline-flex items-center justify-center mx-auto"
                style={{
                  width: 56, height: 56, borderRadius: 14,
                  background: 'rgba(224,138,78,0.10)',
                  border: '1px solid rgba(224,138,78,0.28)',
                  color: dragOver ? 'var(--copper-hi)' : 'var(--copper)',
                }}
              >
                <BoardTargetIcon size={26} />
              </span>

              <div className="mt-5 text-[17px] font-semibold" style={{ color: 'var(--silk)' }}>
                {dragOver ? 'Drop to analyze' : 'Drop a board to analyze it'}
              </div>
              <div className="mt-1 text-[13px]" style={{ color: 'var(--silk-dim)' }}>
                or click anywhere in this card to choose a file
              </div>

              {/* primary action — visual only; the label handles activation */}
              <span
                className="inline-flex items-center gap-2 mt-6 px-5 py-2.5 rounded-lg text-sm font-semibold"
                style={{
                  background: 'linear-gradient(180deg, var(--copper-hi), var(--copper))',
                  color: '#2a1c0f',
                  boxShadow: '0 6px 18px -6px rgba(224,138,78,0.5)',
                }}
              >
                <BoardTargetIcon size={15} /> Choose a board
              </span>

              {/* accepted formats */}
              <div className="mt-6 text-[12px] leading-relaxed" style={{ color: 'var(--silk-faint)' }}>
                KiCad <code>.kicad_pcb</code> <code>.kicad_sch</code> · Eagle <code>.brd</code> ·
                Altium <code>.PcbDoc</code> · IPC <code>.d356</code> · gerber <code>.zip</code> ·
                board-as-code <code>.board</code>
              </div>
            </label>

            {/* Firmware: a quiet secondary jack below the card */}
            <FirmwareJack firmware={firmwareFile} placement="intake" onFile={handleFirmware} />

            {/* Samples — a first report with no file needed at all */}
            <div className="mt-8 text-center" data-testid="samples">
              <div className="text-[11px] uppercase" style={{ color: 'var(--silk-faint)', letterSpacing: '0.08em' }}>
                No board handy? Try a sample
              </div>
              <div className="mt-3 flex flex-wrap justify-center gap-2">
                {SAMPLES.map(s => (
                  <button
                    key={s.label}
                    type="button"
                    onClick={() => void runSample(s)}
                    className="px-3.5 py-2 rounded-lg text-[13px] cursor-pointer"
                    style={{ background: 'var(--surface)', border: '1px solid var(--hairline)', color: 'var(--silk-dim)' }}
                  >
                    <span className="font-semibold" style={{ color: 'var(--silk)' }}>{s.label}</span>
                    <span className="ml-1.5">{s.desc}</span>
                  </button>
                ))}
              </div>
            </div>

            {/* Prepare your own project — one line per ECAD, no jargon */}
            <details className="mt-6 mx-auto text-[13px]" style={{ color: 'var(--silk-dim)', maxWidth: '34rem' }}>
              <summary className="cursor-pointer text-center" style={{ listStylePosition: 'inside' }}>
                Where do I find my board and firmware files?
              </summary>
              <div className="mt-3 space-y-2 leading-relaxed text-left rounded-lg px-4 py-3" style={{ background: 'var(--surface)', border: '1px solid var(--hairline)' }}>
                <div>
                  <strong style={{ color: 'var(--silk)' }}>KiCad</strong> — drop the{' '}
                  <code>.kicad_pcb</code> straight from your project folder. No export step.
                </div>
                <div>
                  <strong style={{ color: 'var(--silk)' }}>Eagle</strong> — the <code>.brd</code>.{' '}
                  <strong style={{ color: 'var(--silk)' }}>Altium</strong> — the <code>.PcbDoc</code>.
                </div>
                <div>
                  <strong style={{ color: 'var(--silk)' }}>Only fab files?</strong> Zip the folder
                  with the gerbers and drill file, drop the zip. The circuit is reverse-extracted
                  from the copper itself.
                </div>
                <div>
                  <strong style={{ color: 'var(--silk)' }}>Firmware</strong> — the compiled image.
                  PlatformIO: <code>.pio/build/&lt;env&gt;/firmware.elf</code>, or just zip the whole
                  project and it is found (or built) for you. ESP-IDF/CMake:{' '}
                  <code>build/&lt;app&gt;.elf</code>. Arduino IDE: Sketch → Export Compiled Binary.
                </div>
                <div>
                  Want this in a pipeline instead? <code>hauksbee-ci init your_board.kicad_pcb</code>{' '}
                  scaffolds a checked-in spec — see <code>docs/ci/CI.md</code>.
                </div>
              </div>
            </details>

            {/* Privacy reassurance */}
            <div className="mt-5 text-center text-[12px]" style={{ color: 'var(--silk-faint)' }}>
              Runs entirely on this machine — nothing is uploaded.
            </div>
          </div>
        )}

        {/* Progress / error */}
        {busy && (
          <div
            role="status"
            aria-live="polite"
            className="mt-6 text-sm flex items-center justify-center gap-2"
            style={{ color: 'var(--copper-hi)' }}
          >
            <span className="slot-spin" /> {busy}
          </div>
        )}
        {uploadError && (
          <div
            aria-live="polite"
            className="mt-6 rounded-lg px-4 py-3 text-sm text-center"
            style={{ background: 'rgba(239,68,68,0.08)', border: '1px solid #7f1d1d', color: '#fca5a5' }}
          >
            {uploadError}
          </div>
        )}

        {/* Hidden file inputs — OUTSIDE the pre-report card so the report view
            can keep offering both jacks. Losing them with the card was the
            "analyzed a board, now I can't add firmware" dead end. */}
        <input
          id="board-file"
          type="file"
          accept=".kicad_pcb,.kicad_sch,.brd,.PcbDoc,.d356,.zip,.txt,.board"
          className="hidden"
          onChange={e => { const f = e.target.files?.[0]; if (f) handleBoard(f); e.target.value = '' }}
        />
        <input
          id="firmware-file"
          type="file"
          accept=".elf,.hex,.zip"
          className="hidden"
          onChange={e => { const f = e.target.files?.[0]; if (f) handleFirmware(f); e.target.value = '' }}
        />

        {/* Report */}
        {report && (
          <ReportView
            report={report}
            boardLabel={preloadedBoardName}
            canRunLive={canRunLive}
            onRunIt={onRunIt}
            // Preloaded (`run --serve`) boards are served at /boards/<name> for
            // the live viewer; reuse that for the report map too.
            boardUrl={
              boardUrl ??
              (preloadedBoardName?.endsWith('.kicad_pcb') ? `/boards/${preloadedBoardName}` : null)
            }
            onNetClick={setSelectedNet}
            // A KiCad file that parses to nothing drawable falls back to the
            // dot map rather than an empty void.
            onEmptyBoard={() => setBoardUrl(null)}
          />
        )}

        {/* Checks builder: compose the spec, run it through the real
            hauksbee-ci, keep the file. */}
        {report?.ok && (
          <ChecksPanel
            // Remount per board (the key doubles as the localStorage key): the
            // panel's mount-time restore is then authoritative, and one
            // board's builder state can never leak into another board's.
            key={checksStorageKey(report)}
            report={report}
            boardFile={lastBoardFile.current}
            firmwareFile={firmwareFile}
            selectedNet={selectedNet}
          />
        )}

        {/* After a report: the board file is still in hand, so firmware can be
            added (or swapped) without starting over, and another board is one
            click away. */}
        {report && !busy && (
          <div className="mt-3 flex flex-col sm:flex-row gap-2">
            {lastBoardFile.current && (
              <FirmwareJack firmware={firmwareFile} placement="report" onFile={handleFirmware} />
            )}
            <label
              htmlFor="board-file"
              data-testid="report-another-board"
              role="button"
              tabIndex={0}
              aria-label="Check another board"
              onKeyDown={activateOnEnterSpace('board-file')}
              className="fw-row flex items-center gap-2.5 px-4 py-3 cursor-pointer text-[13px]"
              data-active="false"
            >
              <span style={{ color: 'var(--silk-faint)', display: 'inline-flex', flexShrink: 0 }}><PlusIcon size={15} /></span>
              <span style={{ color: 'var(--silk-dim)' }}>Check another board</span>
            </label>
          </div>
        )}

        {/* What it checks — a calm, secondary row (only before a report). */}
        {!report && (
          <div className="mt-14">
            <div
              className="text-[11px] font-semibold tracking-[0.2em] uppercase text-center mb-5"
              style={{ color: 'var(--silk-faint)', fontFamily: 'var(--font-mono)' }}
            >
              What it checks
            </div>
            <div className="grid grid-cols-1 sm:grid-cols-3 gap-3">
              {[
                ['As-built circuit', 'Reconstructed from the copper the board actually ships — not the schematic you drew.'],
                ['Firmware co-sim', 'Your firmware on an emulated MCU, coupled to the live analog solve.'],
                ['Beyond a test suite', 'Rails, faults, shorts, USB-C, signal integrity, thermal, loop stability.'],
              ].map(([h, b]) => (
                <div
                  key={h}
                  className="rounded-xl p-4"
                  style={{ background: 'var(--surface)', border: '1px solid var(--hairline)' }}
                >
                  <div className="text-[13px] font-semibold mb-1" style={{ color: 'var(--copper)' }}>{h}</div>
                  <div className="text-[12px] leading-relaxed" style={{ color: 'var(--silk-dim)' }}>{b}</div>
                </div>
              ))}
            </div>
          </div>
        )}

        {/* Which co-sim backends and oracles this machine has, with one-click
            installs for the missing ones (the engine's own discovery decides
            red vs green). Landing state only: once a report is up, the report
            owns the page. */}
        {!report && <DepsPanel />}

        <div
          className="mt-12 text-center text-[12px]"
          style={{ color: 'var(--silk-faint)', fontFamily: 'var(--font-mono)' }}
        >
          <code>hauksbee serve</code> · same engine as <code>hauksbee run</code> and <code>hauksbee-ci</code>
        </div>
      </div>
    </div>
  )
}
