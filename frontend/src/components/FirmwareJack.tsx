import { useEffect, useState } from 'react'
import { CheckIcon, PlusIcon, CloseIcon, ChevronRightIcon } from './Icons'
import { readFirmwareInfo, formatBytes, type FirmwareInfo } from '../lib/firmware-info'

// The firmware input, offered twice: once under the empty drop card, and again
// beneath a finished report so firmware can be added or swapped without
// starting the board over.
//
// Empty, it is a drop target: one <label> over the hidden #firmware-file input,
// the same gesture as the board card. Filled, it stops being a drop target and
// becomes a SLOT: it names what is staged, offers Replace and Remove as two
// separate deliberate controls, and opens an inspection panel describing the
// bytes. Replace used to be the same undifferentiated click as everything else
// and Remove did not exist at all, so a staged firmware was a one-way door.

/** Where the jack is being rendered, which is all that changes about the copy. */
export type FirmwareJackPlacement = 'intake' | 'report'

interface FirmwareJackProps {
  /** The staged firmware, if any. */
  firmware: File | null
  placement: FirmwareJackPlacement
  /** Called with a file dropped onto (or chosen for) the jack. */
  onFile: (f: File) => void
  /** Unstage the firmware and re-run the board without it. Absent means the
   *  caller cannot support removal, and no Remove control is offered. */
  onClear?: () => void
  /** True while an upload is being analyzed: the jack refuses drops and
   *  clicks (one upload at a time) and dims to say so. */
  locked?: boolean
  /** Whether the finished report actually co-simulated this firmware
   *  (`cosim.ran`). The report placement's subtitle used to claim
   *  "co-simulated in this report" unconditionally, which sat under a
   *  "Co-sim not available" banner whenever the board's MCU needs an external
   *  emulator. Undefined where the answer is not known yet. */
  cosimRan?: boolean
}

const TEST_IDS: Record<FirmwareJackPlacement, string> = {
  intake: 'firmware-zone',
  report: 'report-firmware-jack',
}

const LABELS: Record<FirmwareJackPlacement, string> = {
  intake: 'Add a firmware file to co-simulate on the board',
  report: 'Add or swap firmware and re-run the co-sim',
}

// Keyboard activation for the label-wrapped file input (U3 a11y): a styled
// <label> is clickable by mouse and screen-reader but is not keyboard-focusable
// on its own, so it gets role="button" + tabIndex and triggers the hidden input
// on Enter/Space, the same activation the label already does on click.
function activateFirmwareInput(e: React.KeyboardEvent) {
  if (e.key === 'Enter' || e.key === ' ') {
    e.preventDefault()
    document.getElementById('firmware-file')?.click()
  }
}

function pickFirmwareFile() {
  document.getElementById('firmware-file')?.click()
}

function EmptyCopy({ placement }: { placement: FirmwareJackPlacement }) {
  return placement === 'intake' ? (
    <>
      Add firmware (<code>.elf</code> / <code>.hex</code>, or a zipped PlatformIO
      project, e.g. <code>.pio/build/&lt;env&gt;/firmware.elf</code>) to co-simulate
      it on the board&rsquo;s MCU
    </>
  ) : (
    <>
      Add firmware (<code>.elf</code> / <code>.hex</code> / project <code>.zip</code>):
      the report re-runs with a co-sim of it on this board
    </>
  )
}

/** What the uploaded bytes are. Read from the file itself; see firmware-info.ts
 *  for why none of this can come from the engine. */
function FirmwareDetail({ file }: { file: File }) {
  const [info, setInfo] = useState<FirmwareInfo | null>(null)

  useEffect(() => {
    let live = true
    setInfo(null)
    void readFirmwareInfo(file).then(i => { if (live) setInfo(i) })
    return () => { live = false }
  }, [file])

  if (!info) {
    return (
      <div className="text-[11px] px-3 pb-3" style={{ color: 'var(--silk-faint)' }}>
        reading the image…
      </div>
    )
  }

  const row = (label: string, value: React.ReactNode) => (
    <div className="flex items-baseline gap-2 text-[11px]">
      <span className="shrink-0" style={{ color: 'var(--silk-faint)', minWidth: 74 }}>{label}</span>
      <span style={{ color: 'var(--silk-dim)', fontFamily: 'var(--font-mono)', wordBreak: 'break-all' }}>
        {value}
      </span>
    </div>
  )

  return (
    <div
      data-testid="firmware-detail"
      className="flex flex-col gap-1 px-3 pb-3"
      style={{ borderTop: '1px solid var(--rule)', paddingTop: 10 }}
    >
      {row('size', formatBytes(info.size))}
      {info.sha256Short && row('sha256', `${info.sha256Short}…`)}
      {row('format', info.arch
        ? `${info.format} · ${info.arch}`
        : info.format === 'ELF' && info.machineCode !== undefined
          // Named by number when the engine has no name for it: an image the
          // loader would refuse is still worth identifying precisely.
          ? `${info.format} · e_machine 0x${info.machineCode.toString(16)} (not a target this build knows)`
          : info.format)}
      {info.elfClass && row('ELF', `${info.elfClass}-bit, ${info.endian}-endian`)}
      {info.entry && row('entry', info.entry)}
      {info.sections && info.sections.length > 0 && (
        <div className="flex items-baseline gap-2 text-[11px]">
          <span className="shrink-0" style={{ color: 'var(--silk-faint)', minWidth: 74 }}>sections</span>
          <span style={{ color: 'var(--silk-dim)', fontFamily: 'var(--font-mono)' }}>
            {info.sections.slice(0, 6).map(s => (
              <span key={s.name} className="mr-2 inline-block">
                {s.name} {formatBytes(s.size)}
                {s.noBits && <span style={{ color: 'var(--silk-faint)' }}> (RAM)</span>}
              </span>
            ))}
          </span>
        </div>
      )}
      {info.parseNote && (
        <div className="text-[11px]" style={{ color: 'var(--warn-strong)' }}>{info.parseNote}</div>
      )}
    </div>
  )
}

export function FirmwareJack({ firmware, placement, onFile, onClear, locked = false, cosimRan }: FirmwareJackProps) {
  const [open, setOpen] = useState(false)

  // Empty: a drop target, the same gesture the board card takes.
  if (!firmware) {
    return (
      <label
        data-testid={TEST_IDS[placement]}
        htmlFor="firmware-file"
        role="button"
        tabIndex={locked ? -1 : 0}
        aria-label={LABELS[placement]}
        aria-disabled={locked}
        onKeyDown={locked ? undefined : activateFirmwareInput}
        onClick={e => { if (locked) e.preventDefault() }}
        onDragEnter={e => e.preventDefault()}
        onDragOver={e => e.preventDefault()}
        onDrop={e => {
          e.preventDefault()
          if (locked) return
          const f = e.dataTransfer.files[0]
          if (f) onFile(f)
        }}
        className={`fw-row flex items-center gap-2.5 px-4 py-3 text-[13px] ${
          locked ? 'cursor-default' : 'cursor-pointer'
        } ${placement === 'intake' ? 'mt-3' : 'flex-1'}`}
        style={locked ? { opacity: 0.5 } : undefined}
        data-active="false"
      >
        <span style={{ color: 'var(--silk-faint)', display: 'inline-flex', flexShrink: 0 }}>
          <PlusIcon size={15} />
        </span>
        <span style={{ color: 'var(--silk-dim)' }}>
          <EmptyCopy placement={placement} />
        </span>
      </label>
    )
  }

  // Staged: a slot. Not a label, because it now contains its own buttons, and a
  // button inside a label fires the label too.
  return (
    <div
      data-testid={TEST_IDS[placement]}
      className={`fw-row fw-slot ${placement === 'intake' ? 'mt-3' : 'flex-1'}`}
      style={locked ? { opacity: 0.5 } : undefined}
      data-active="true"
      onDragEnter={e => e.preventDefault()}
      onDragOver={e => e.preventDefault()}
      onDrop={e => {
        e.preventDefault()
        if (locked) return
        const f = e.dataTransfer.files[0]
        if (f) onFile(f)
      }}
    >
      <div className="flex items-center gap-2.5 px-4 py-3">
        <span style={{ color: 'var(--ok)', display: 'inline-flex', flexShrink: 0 }}>
          <CheckIcon size={15} />
        </span>
        <div className="min-w-0 flex-1">
          <div
            className="text-[13px] font-semibold truncate"
            style={{ color: 'var(--silk)', fontFamily: 'var(--font-mono)' }}
            title={firmware.name}
          >
            {firmware.name}
          </div>
          <div className="text-[11px]" style={{ color: 'var(--silk-faint)' }}>
            {placement === 'intake'
              ? 'co-simulated on the board’s MCU'
              : cosimRan === false
                ? 'staged; this report did not co-simulate it (see the co-sim section)'
                : 'co-simulated in this report'}
          </div>
        </div>

        <div className="flex items-center gap-1.5 shrink-0">
          <button
            type="button"
            data-testid="firmware-inspect"
            onClick={() => setOpen(v => !v)}
            aria-expanded={open}
            className="hb-btn hb-press px-2.5 py-1.5 text-[12px]"
          >
            <span
              className="inline-flex mr-1"
              style={{ transform: open ? 'rotate(90deg)' : undefined, transition: 'transform 120ms' }}
            >
              <ChevronRightIcon size={11} />
            </span>
            Inspect
          </button>
          <button
            type="button"
            data-testid="firmware-replace"
            onClick={pickFirmwareFile}
            disabled={locked}
            className="hb-btn hb-press px-2.5 py-1.5 text-[12px]"
          >
            Replace
          </button>
          {onClear && (
            <button
              type="button"
              data-testid="firmware-remove"
              onClick={onClear}
              disabled={locked}
              aria-label={`Remove the firmware ${firmware.name} and re-run the board without it`}
              title="Remove the firmware and re-run the board without it"
              className="hb-btn hb-press px-2 py-1.5 text-[12px]"
            >
              <CloseIcon size={12} />
            </button>
          )}
        </div>
      </div>

      {open && <FirmwareDetail file={firmware} />}
    </div>
  )
}
