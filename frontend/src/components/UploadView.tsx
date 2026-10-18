import React from 'react'
import type { BoardSession } from '../hooks/useBoardSession'
import type { SessionsState } from '../hooks/useSessions'
import { BoardTargetIcon, HistoryIcon } from './Icons'
import { relTime } from '../lib/rel-time'
import { hasDefaultName } from '../lib/session-store'
import { AdditionalEvidencePanel, evidenceSlots } from './AdditionalEvidencePanel'
import { ArriveOnce, PressCard, SkeletonBar, useDropTarget, useSkeletonSwap } from '../motion'
import { UploadBanners } from './ui'
import { LoadBar } from './LoadBar'
import { acceptedFormatsSentence, withoutEngineFormatList } from '../lib/board-formats'
import { motion, useReducedMotion } from 'motion/react'
import { CELL, INSTANT } from '../motion'

// The Board view before a board exists: the drop-a-board intake. One elevated
// card holds the drop area + primary action (the single focal point); firmware
// is a quiet secondary jack; the bundled samples give a first report with no
// file at all.

// Bundled one-click samples (files under frontend/public/samples/, see its
// README for provenance).
//
// Watchy leads deliberately. Most people try exactly one sample and decide from
// it, so the first one has to be a board someone actually fabricated: 86
// footprints resolving to 82 distinct parts, 685 copper segments, and a real
// spacing report with real net names in it. Blinky is one footprint and one
// trace, and its whole DRC report is the single line "Looks healthy", which
// shows nothing and reads as though the tool does nothing. It stays, last, as
// the minimal case to compare against.
const SAMPLES: { label: string; desc: string; board: string; firmware?: string }[] = [
  { label: 'Watchy', desc: 'a real smartwatch', board: '/samples/watchy.kicad_pcb' },
  {
    label: 'Boot gate + firmware',
    desc: 'live co-sim',
    board: '/samples/boot_gate.kicad_pcb',
    firmware: '/samples/boot_gate.hex',
  },
  { label: 'Blinky', desc: 'a minimal board', board: '/samples/blinky.kicad_pcb' },
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

/** The shape of the report that is coming, while it is still being computed.
 *
 *  A spinner says "wait"; this says "wait, and here is what will be here": the
 *  verdict bar, then the findings. It is drawn from the real report's layout, so
 *  when the analysis lands the content arrives INTO these positions instead of
 *  pushing the page around. That is the whole reason it is a skeleton and not a
 *  spinner, and it is why a generic three-grey-bars block would not have done. */
function ReportSkeleton() {
  return (
    <div aria-hidden data-testid="report-skeleton" className="mt-4">
      {/* the verdict headline, two lines and its board/parts/nets sub-line */}
      <div className="rounded-xl px-4 py-3.5" style={{ border: '1px solid var(--hairline)', background: 'var(--surface)' }}>
        <SkeletonBar width="88%" height={12} />
        <div className="mt-2"><SkeletonBar width="54%" height={12} /></div>
        <div className="mt-3"><SkeletonBar width="32%" height={9} /></div>
      </div>
      {/* the findings sections */}
      {[0, 1, 2].map(i => (
        <div
          key={i}
          className="mt-3 rounded-xl px-4 py-3.5"
          style={{ border: '1px solid var(--hairline)', background: 'var(--surface)' }}
        >
          <SkeletonBar width={i === 0 ? '28%' : i === 1 ? '36%' : '22%'} height={9} />
          <div className="mt-2.5"><SkeletonBar width="96%" height={10} /></div>
          <div className="mt-2"><SkeletonBar width={i === 1 ? '71%' : '84%'} height={10} /></div>
        </div>
      ))}
    </div>
  )
}

export function UploadView({ session, onOpenLive, sessions, onResume, avrAvailable = true }: {
  session: BoardSession
  /** Open the live session already running server-side (mounts the sim). */
  onOpenLive?: () => void
  /** Saved sessions, for the resume offer. */
  sessions?: SessionsState
  onResume?: (id: string) => void
  /** The permissive/Windows binary has no in-process AVR backend, so its first
   *  screen must not advertise an AVR firmware sample it cannot execute. */
  avrAvailable?: boolean
}) {
  const {
    busy, loadProgress, uploadError, uploadNotice, dismissNotice, handleBoard, runSample,
  } = session
  const reduced = useReducedMotion()

  // Drag feedback: 'over' when files are genuinely being dragged, 'reject'
  // when the drag carries something that is not a file at all (a text
  // selection, a link). There is no 'accept': the browser withholds file names
  // during a drag, so a green tick before the drop would be a guess. See
  // ../motion/DropField for the rest of that argument.
  const drop = useDropTarget(files => { if (files[0]) handleBoard(files[0]) })
  const dragOver = drop.state === 'over'
  const dragReject = drop.state === 'reject'

  // The report skeleton is derived from `busy`, so it cannot outlive the
  // request: the moment the analysis resolves (report OR error), `busy` clears
  // and the skeleton is on its way out in the same tick. It also does not
  // appear at all for a fast local board, and once it appears it stays long
  // enough not to flicker.
  const { showSkeleton } = useSkeletonSwap({ ready: !busy, delay: 140, minVisible: 400 })

  return (
    <div className="landing h-full overflow-y-auto view-enter">
      <div className="max-w-xl mx-auto px-6 pb-24" style={{ paddingTop: 'clamp(2rem, 7vh, 4.5rem)' }}>
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
              <LoadBar progress={loadProgress} />
              <div className="mt-3 text-[12px]" style={{ color: 'var(--silk-faint)' }}>
                Extracting the circuit from the copper and running the checks. Further
                uploads are paused until this finishes.
              </div>
            </div>
          ) : (
            <motion.label
              data-testid="drop-zone"
              htmlFor="board-file"
              role="button"
              tabIndex={0}
              aria-label="Choose a board file to check"
              onKeyDown={activateOnEnterSpace('board-file')}
              {...drop.bind}
              className="drop-card block cursor-pointer px-8 py-11 text-center"
              data-active={dragOver ? 'true' : 'false'}
              data-reject={dragReject ? 'true' : 'false'}
              // The card lifts a hair and settles when files come over it. The
              // move is 2 px and spring-damped: enough that the target reads as
              // armed, small enough that nothing on it becomes hard to read
              // mid-drag. A refused drag does not lift at all, which is the
              // point of distinguishing the two.
              initial={false}
              animate={reduced ? {} : { y: dragOver ? -2 : 0, scale: dragOver ? 1.004 : 1 }}
              transition={reduced ? INSTANT : CELL}
              style={dragReject
                ? { borderColor: 'var(--err-border)', background: 'var(--err-bg)' }
                : undefined}
            >
              {/* icon in a soft copper disc */}
              <motion.span
                className="inline-flex items-center justify-center mx-auto"
                initial={false}
                animate={reduced ? {} : { scale: dragOver ? 1.06 : 1 }}
                transition={reduced ? INSTANT : CELL}
                style={{
                  width: 56, height: 56, borderRadius: 14,
                  background: dragReject ? 'var(--err-bg)' : 'var(--copper-tint)',
                  border: `1px solid ${dragReject ? 'var(--err-border)' : 'var(--copper-deep)'}`,
                  color: dragReject ? 'var(--err)' : dragOver ? 'var(--copper-hi)' : 'var(--copper)',
                }}
              >
                <BoardTargetIcon size={26} />
              </motion.span>

              <div
                className="mt-5 text-[17px] font-semibold"
                style={{ color: dragReject ? 'var(--err-strong)' : 'var(--silk)' }}
              >
                {dragReject
                  ? 'That is not a file'
                  : dragOver ? 'Drop to check' : uploadError ? 'That file did not work' : 'Check my board'}
              </div>
              <div className="mt-1 text-[13px]" style={{ color: 'var(--silk-dim)' }}>
                {dragReject
                  ? 'Drag a board file out of your file manager, or click to choose one.'
                  : 'Drop the board design from your project folder, or click anywhere in this card.'}
              </div>
              {/* A refusal lives where the file was dropped, with the way out:
                  the same card is the retry (drop or click again). */}
              {uploadError && !dragReject && (
                <div
                  data-testid="upload-error"
                  role="alert"
                  className="mt-4 rounded-lg px-3 py-2.5 text-[13px] leading-relaxed text-left"
                  style={{ background: 'var(--err-bg)', border: '1px solid var(--err-border)', color: 'var(--err-strong)' }}
                >
                  {withoutEngineFormatList(uploadError)}
                </div>
              )}

              {/* primary action, visual only; the label handles activation */}
              <span className="hb-btn-primary inline-flex items-center gap-2 mt-6 px-5 py-2.5 rounded-lg text-sm">
                <BoardTargetIcon size={15} /> {uploadError ? 'Choose another file' : 'Choose my board'}
              </span>

              <div className="mt-6 text-[12px] leading-relaxed" style={{ color: 'var(--silk-faint)' }}>
                {uploadError && acceptedFormatsSentence()
                  ? `Accepted: ${acceptedFormatsSentence()}`
                  : 'Works with board designs from KiCad, Eagle and Altium, plus manufacturing packages.'}
              </div>
            </motion.label>
          )}

          {/* The report's shape while the report is still being computed. Lives
              under the busy card so the wait is not a bare spinner on an
              otherwise empty page. */}
          {showSkeleton && <ReportSkeleton />}

          <UploadBanners notice={uploadNotice} error={null} onDismiss={dismissNotice} />

          {/* A live session survives a page reload server-side. Landing on a
              bare drop zone while the server still runs a board reads as data
              loss, so this is the page's ONE statement of it, as a quiet line
              under the drop card rather than a second call to action. */}
          {!busy && session.serverLive?.active && session.serverLive.boardName && onOpenLive && (
            <div
              data-testid="landing-live-notice"
              className="mt-5 text-center text-[13px] flex flex-wrap items-center justify-center gap-x-3 gap-y-2"
              style={{ color: 'var(--silk-dim)' }}
            >
              <span>
                The live sim is still running{' '}
                <b style={{ fontFamily: 'var(--font-mono)', fontWeight: 600, color: 'var(--silk)' }}>
                  {session.serverLive.boardName}
                </b>{' '}
                on this server.
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

          {/* Samples, a first report with no file needed at all */}
          {!busy && <SampleRow onRun={runSample} avrAvailable={avrAvailable} />}

          {/* Saved sessions: a quiet list below the samples, never a second
              primary. The board the user just left is not offered back. */}
          {!busy && sessions && onResume && (
            <SavedSessions
              sessions={sessions}
              onResume={onResume}
              leftId={session.runEpoch > 0 ? sessions.current?.id ?? null : null}
            />
          )}

          {/* One companion-input contract feeds report, Checks and Live Sim. */}
          {!busy && (
            <details
              data-testid="intake-more"
              className="mt-6 mx-auto text-[13px]"
              style={{ color: 'var(--silk-dim)' }}
              open={!!(session.firmwareFile || session.schematicFile) || undefined}
            >
              <summary className="cursor-pointer text-center" style={{ listStylePosition: 'inside' }}>
                Add firmware, a schematic or other inputs
              </summary>
              <AdditionalEvidencePanel
                placement="intake"
                {...evidenceSlots(session)}
              />
            </details>
          )}

          {!busy && <FindFilesHelp />}

          {/* Privacy reassurance */}
          {!busy && (
            <div className="mt-5 text-center text-[12px]" style={{ color: 'var(--silk-faint)' }}>
              Board analysis stays on this machine. Datasheet extraction is the only off-machine
              feature, and it asks for explicit consent before sending a PDF.
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


/** The bundled sample boards: a first report with no file at all. Shared with
 *  the refusal card, which keeps them one click away after a bad drop. */
export function SampleRow({ onRun, avrAvailable = true }: {
  onRun: (s: { label: string; desc: string; board: string; firmware?: string }) => void
  avrAvailable?: boolean
}) {
  const samples = SAMPLES.filter(s => avrAvailable || !s.firmware)
  return (
    <div className="mt-8 text-center" data-testid="samples">
      <div className="text-[11px] uppercase" style={{ color: 'var(--silk-faint)', letterSpacing: '0.08em' }}>
        No board handy? Try a sample
      </div>
      {/* One deliberate row of three, or a clean stack when the column
          is too narrow for it. Never a two-then-one rag. */}
      {/* Fewer than three (no AVR backend, or the refusal card) fill the row
          instead of leaving an empty third column. */}
      <div
        className="sample-row mt-3"
        style={samples.length < 3 ? { gridTemplateColumns: `repeat(${samples.length}, minmax(0, 1fr))` } : undefined}
      >
        {samples.map(s => (
          // A card that rises 1 px on hover and sinks 1 px on press.
          // The press tracking is the substance here, not the movement:
          // it releases when the pointer leaves the card mid-press, when
          // the window loses focus, and on a keyboard Space-hold, none
          // of which :active gets right. See PressCard in ../motion.
          <PressCard
            key={s.label}
            data-testid={`sample-${s.label.toLowerCase().replace(/[^a-z]+/g, '-')}`}
            onPress={() => onRun(s)}
            className="hb-btn px-3.5 py-2.5 text-[13px]"
          >
            <span className="block font-semibold" style={{ color: 'var(--silk)' }}>{s.label}</span>
            <span className="block text-[12px] mt-0.5">{s.desc}</span>
          </PressCard>
        ))}
      </div>
    </div>
  )
}

/** "Where do I find my board and firmware files?", one line per ECAD. */
export function FindFilesHelp() {
  return (
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
          <strong style={{ color: 'var(--silk)' }}>EasyEDA</strong>; Standard: File → Export → EasyEDA source
          (the <code className="hb-inline">.json</code>). Pro: the <code className="hb-inline">.epcb</code> or
          the whole <code className="hb-inline">.epro</code> project.
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
          Want this in a pipeline instead? Set up checks from the report once your board loads, or run{' '}
          <code className="hb-inline">hauksbee-ci init your_board.kicad_pcb</code> to scaffold a checked-in spec.
        </div>
      </div>
    </details>
  )
}

/** How many saved sessions the drop page lists. The rest live in the rail's
 *  Sessions panel; this is a way back in, not a manager. */
const LISTED_SESSIONS = 3

/** Saved sessions on the drop page, as a quiet secondary list under the
 *  samples. Every row is a board you can recognise (its file name), when it was
 *  last touched and how big it was, with one plain button. Nothing here is a
 *  primary action: the drop card above it is. `leftId` is the session whose
 *  board the user just walked away from with "Analyze another board"; offering
 *  it straight back would undo the choice they just made. */
function SavedSessions({ sessions, onResume, leftId }: {
  sessions: SessionsState
  onResume: (id: string) => void
  leftId: string | null
}) {
  // `resumable` is null while a board is loaded and after "Hide", so it also
  // carries those two decisions.
  if (!sessions.resumable) return null
  const rows = sessions.rows.filter(r => r.hasReport && r.id !== leftId)
  if (rows.length === 0) return null
  const listed = rows.slice(0, LISTED_SESSIONS)
  const more = rows.length - listed.length
  const now = Date.now()

  return (
    <ArriveOnce className="mt-8">
      <div data-testid="session-resume">
      <div className="flex items-center justify-center gap-2 text-[11px] uppercase" style={{ color: 'var(--silk-faint)', letterSpacing: '0.08em' }}>
        <HistoryIcon size={12} /> Pick up a saved session
      </div>
      <ul className="mt-3 space-y-2 list-none p-0 m-0">
        {listed.map(row => {
          const facts = [
            `${row.board.numComponents} ${row.board.numComponents === 1 ? 'part' : 'parts'}`,
            row.checkCount > 0 ? `${row.checkCount} ${row.checkCount === 1 ? 'check' : 'checks'}` : null,
            row.firmwareName ? `firmware ${row.firmwareName}` : null,
          ].filter(Boolean).join(' · ')
          const renamed = !hasDefaultName(row)
          return (
            <li
              key={row.id}
              data-testid="session-resume-row"
              className="hb-card flex items-center gap-3 px-3.5 py-2.5"
            >
              <div className="min-w-0 flex-1">
                <div className="truncate text-[13px] font-semibold" title={row.name} style={{ color: 'var(--silk)', fontFamily: renamed ? undefined : 'var(--font-mono)' }}>
                  {row.name}
                </div>
                <div className="truncate text-[11.5px] tnum" style={{ color: 'var(--silk-faint)' }}>
                  {renamed ? `${row.board.fileName} · ` : ''}{facts} · saved {relTime(row.updatedAt, now)}
                </div>
              </div>
              <button
                type="button"
                data-testid="session-resume-open"
                onClick={() => onResume(row.id)}
                aria-label={`Open the saved session ${row.name}`}
                className="hb-btn hb-press px-3 text-[12px] shrink-0"
                style={{ height: 30 }}
              >
                Open
              </button>
            </li>
          )
        })}
      </ul>
      <div className="mt-2 flex flex-wrap items-center justify-center gap-x-3 gap-y-1 text-[11.5px]" style={{ color: 'var(--silk-faint)' }}>
        {more > 0 && <span>{more} more under Sessions in the sidebar</span>}
        <span>The report and checks come back; the board file does not, so re-running needs it dropped again.</span>
        <button
          type="button"
          data-testid="session-resume-dismiss"
          onClick={sessions.dismissResume}
          className="hb-press underline cursor-pointer"
          style={{ background: 'none', border: 'none', color: 'inherit', padding: 0 }}
        >
          Hide this list
        </button>
      </div>
      </div>
    </ArriveOnce>
  )
}
