import { useState } from 'react'
import type { BoardSession } from '../hooks/useBoardSession'
import { BoardTargetIcon } from './Icons'
import { FirmwareJack } from './FirmwareJack'

// The Board view before a board exists: the drop-a-board intake. One elevated
// card holds the drop area + primary action (the single focal point); firmware
// is a quiet secondary jack; the bundled samples give a first report with no
// file at all.

// Bundled one-click samples (files under frontend/public/samples/, see its
// README for provenance). A ladder: tiny clean board → real product → a
// board + firmware pair that exercises the co-sim.
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

export function UploadView({ session, onOpenLive }: {
  session: BoardSession
  /** Open the live session already running server-side (mounts the sim). */
  onOpenLive?: () => void
}) {
  const { busy, uploadError, firmwareFile, handleBoard, handleFirmware, runSample } = session
  const [dragOver, setDragOver] = useState(false)

  return (
    <div className="landing h-full overflow-y-auto view-enter">
      <div className="max-w-xl mx-auto px-6 pb-24" style={{ paddingTop: 'clamp(2rem, 7vh, 4.5rem)' }}>
        {/* A live session survives a page reload server-side. Landing on a
            bare drop zone while the server still runs a board reads as data
            loss; acknowledge the session and offer to open it. */}
        {!busy && session.serverLive?.active && session.serverLive.boardName && onOpenLive && (
          <div
            data-testid="landing-live-notice"
            className="mb-6 rounded-lg px-4 py-3 text-[13px] flex flex-wrap items-center gap-x-3 gap-y-2"
            style={{ background: 'var(--surface)', border: '1px solid var(--copper-deep)', color: 'var(--silk)' }}
          >
            <span>
              A live session for{' '}
              <b style={{ fontFamily: 'var(--font-mono)', fontWeight: 600 }}>
                {session.serverLive.boardName}
              </b>{' '}
              is still running on this server.
            </span>
            <button
              type="button"
              data-testid="landing-open-live"
              onClick={onOpenLive}
              className="hb-btn hb-press px-3 text-[12px]"
              style={{ height: 28 }}
            >
              Open the live sim
            </button>
          </div>
        )}
        {/* Hero, a calm, centered thesis */}
        {!busy && (
          <div className="text-center">
            {/* The hero speaks in the wordmark's type, not the system default:
                the mark in the rail and the thesis under it are one voice. */}
            <h1
              className="font-semibold"
              style={{
                color: 'var(--silk)',
                fontFamily: 'var(--font-mono)',
                fontSize: 'clamp(1.55rem, 3.6vw, 2.1rem)',
                lineHeight: 1.15,
                letterSpacing: '-0.035em',
                margin: 0,
              }}
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

        {/* The upload card; the single focal action. While a run is in flight
            it becomes the progress surface: the dropped file's identity, a
            spinner, and a lock on further uploads until this one resolves. */}
        <div className="mt-9">
          {busy ? (
            <div
              data-testid="drop-zone-busy"
              aria-live="polite"
              className="drop-card block px-8 py-11 text-center"
              data-active="false"
              style={{ cursor: 'default' }}
            >
              <span
                className="inline-flex items-center justify-center mx-auto"
                style={{
                  width: 56, height: 56, borderRadius: 14,
                  background: 'var(--copper-tint)',
                  border: '1px solid var(--copper-deep)',
                  color: 'var(--copper-hi)',
                }}
              >
                <span className="slot-spin" style={{ width: 22, height: 22 }} />
              </span>
              <div className="mt-5 text-[17px] font-semibold" style={{ color: 'var(--silk)' }}>
                Analyzing <span style={{ fontFamily: 'var(--font-mono)' }}>{busy.board}</span> ...
              </div>
              {busy.firmware && (
                <div className="mt-1 text-[13px]" style={{ color: 'var(--copper-hi)' }}>
                  with a co-sim of <span style={{ fontFamily: 'var(--font-mono)' }}>{busy.firmware}</span>
                </div>
              )}
              <div className="mt-3 text-[12px]" style={{ color: 'var(--silk-faint)' }}>
                Extracting the circuit from the copper and running the checks. Further
                uploads are paused until this finishes.
              </div>
            </div>
          ) : (
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
                  background: 'var(--copper-tint)',
                  border: '1px solid var(--copper-deep)',
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

              {/* primary action, visual only; the label handles activation */}
              <span className="hb-btn-primary inline-flex items-center gap-2 mt-6 px-5 py-2.5 rounded-lg text-sm">
                <BoardTargetIcon size={15} /> Choose a board
              </span>

              {/* accepted formats */}
              <div className="mt-6 text-[12px] leading-relaxed" style={{ color: 'var(--silk-faint)' }}>
                KiCad <code className="hb-inline">.kicad_pcb</code> <code className="hb-inline">.kicad_sch</code> ·
                Eagle <code className="hb-inline">.brd</code> ·
                Altium <code className="hb-inline">.PcbDoc</code> · IPC <code className="hb-inline">.d356</code> ·
                gerber <code className="hb-inline">.zip</code> ·
                board-as-code <code className="hb-inline">.board</code>
              </div>
            </label>
          )}

          {/* Firmware: a quiet secondary jack below the card */}
          <FirmwareJack firmware={firmwareFile} placement="intake" onFile={handleFirmware} locked={!!busy} />

          {uploadError && (
            <div
              aria-live="polite"
              className="mt-6 rounded-lg px-4 py-3 text-sm text-center"
              style={{ background: 'var(--err-bg)', border: '1px solid var(--err-border)', color: 'var(--err-strong)' }}
            >
              {uploadError}
            </div>
          )}

          {/* Samples, a first report with no file needed at all */}
          {!busy && (
            <div className="mt-8 text-center" data-testid="samples">
              <div className="text-[11px] uppercase" style={{ color: 'var(--silk-faint)', letterSpacing: '0.08em' }}>
                No board handy? Try a sample
              </div>
              {/* One deliberate row of three, or a clean stack when the column
                  is too narrow for it. Never a two-then-one rag. */}
              <div className="sample-row mt-3">
                {SAMPLES.map(s => (
                  <button
                    key={s.label}
                    type="button"
                    onClick={() => runSample(s)}
                    className="hb-btn hb-press px-3.5 py-2.5 text-[13px]"
                  >
                    <span className="block font-semibold" style={{ color: 'var(--silk)' }}>{s.label}</span>
                    <span className="block text-[12px] mt-0.5">{s.desc}</span>
                  </button>
                ))}
              </div>
            </div>
          )}

          {/* Prepare your own project, one line per ECAD, no jargon */}
          {!busy && (
            <details className="mt-6 mx-auto text-[13px]" style={{ color: 'var(--silk-dim)', maxWidth: '34rem' }}>
              <summary className="cursor-pointer text-center" style={{ listStylePosition: 'inside' }}>
                Where do I find my board and firmware files?
              </summary>
              <div className="mt-3 space-y-2 leading-relaxed text-left rounded-lg px-4 py-3" style={{ background: 'var(--surface)', border: '1px solid var(--hairline)' }}>
                <div>
                  <strong style={{ color: 'var(--silk)' }}>KiCad</strong>, drop the{' '}
                  <code className="hb-inline">.kicad_pcb</code> straight from your project folder. No export step.
                </div>
                <div>
                  <strong style={{ color: 'var(--silk)' }}>Eagle</strong>; the <code className="hb-inline">.brd</code>.{' '}
                  <strong style={{ color: 'var(--silk)' }}>Altium</strong>; the <code className="hb-inline">.PcbDoc</code>.
                </div>
                <div>
                  <strong style={{ color: 'var(--silk)' }}>Only fab files?</strong> Zip the folder
                  with the gerbers and drill file, drop the zip. The circuit is reverse-extracted
                  from the copper itself.
                </div>
                <div>
                  <strong style={{ color: 'var(--silk)' }}>Firmware</strong>; the compiled image.
                  PlatformIO: <code className="hb-inline">.pio/build/&lt;env&gt;/firmware.elf</code>, or just zip the whole
                  project and it is found (or built) for you. ESP-IDF/CMake:{' '}
                  <code className="hb-inline">build/&lt;app&gt;.elf</code>. Arduino IDE: Sketch → Export Compiled Binary.
                </div>
                <div>
                  Want this in a pipeline instead? <code className="hb-inline">hauksbee-ci init your_board.kicad_pcb</code>{' '}
                  scaffolds a checked-in spec, see <code className="hb-inline">docs/ci/CI.md</code>.
                </div>
              </div>
            </details>
          )}

          {/* Privacy reassurance */}
          {!busy && (
            <div className="mt-5 text-center text-[12px]" style={{ color: 'var(--silk-faint)' }}>
              Runs entirely on this machine, nothing is uploaded.
            </div>
          )}
        </div>

        {/* What it checks, a calm, secondary row */}
        {!busy && (
          <div className="mt-14">
            <div
              className="text-[11px] font-semibold tracking-[0.2em] uppercase text-center mb-5"
              style={{ color: 'var(--silk-faint)', fontFamily: 'var(--font-mono)' }}
            >
              What it checks
            </div>
            <div className="grid grid-cols-1 sm:grid-cols-3 gap-3 card-stagger">
              {[
                ['As-built circuit', 'Reconstructed from the copper the board actually ships, not the schematic you drew.'],
                ['Firmware co-sim', 'Your firmware on an emulated MCU, coupled to the live analog solve.'],
                ['Beyond a test suite', 'Rails, faults, shorts, USB-C, signal integrity, thermal, loop stability.'],
              ].map(([h, b]) => (
                <div key={h} className="hb-card p-4">
                  <div className="text-[13px] font-semibold mb-1" style={{ color: 'var(--copper)' }}>{h}</div>
                  <div className="text-[12px] leading-relaxed" style={{ color: 'var(--silk-dim)' }}>{b}</div>
                </div>
              ))}
            </div>
          </div>
        )}

        <div
          className="mt-12 text-center text-[12px]"
          style={{ color: 'var(--silk-faint)', fontFamily: 'var(--font-mono)' }}
        >
          <code className="hb-inline">hauksbee serve</code> · same engine as{' '}
          <code className="hb-inline">hauksbee run</code> and <code className="hb-inline">hauksbee-ci</code>
        </div>
      </div>
    </div>
  )
}
