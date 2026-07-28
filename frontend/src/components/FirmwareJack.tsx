import { CheckIcon, PlusIcon } from './Icons'

// The firmware input, offered twice: once under the empty drop card, and again
// beneath a finished report so firmware can be added or swapped without
// starting the board over. Both jacks drive the same hidden #firmware-file
// input; only the copy differs, so only the copy is a prop.

/** Where the jack is being rendered, which is all that changes about it. */
export type FirmwareJackPlacement = 'intake' | 'report'

interface FirmwareJackProps {
  /** The staged firmware, if any. Drives the icon, colour, and copy. */
  firmware: File | null
  placement: FirmwareJackPlacement
  /** Called with a file dropped onto the jack. */
  onFile: (f: File) => void
  /** True while an upload is being analyzed: the jack refuses drops and
   *  clicks (one upload at a time) and dims to say so. */
  locked?: boolean
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

function StagedCopy({ name, placement }: { name: string; placement: FirmwareJackPlacement }) {
  return placement === 'intake' ? (
    <>Firmware: <strong>{name}</strong>, click to change</>
  ) : (
    <>Firmware: <strong>{name}</strong>, click to swap and re-run the co-sim</>
  )
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

export function FirmwareJack({ firmware, placement, onFile, locked = false }: FirmwareJackProps) {
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
      data-active={firmware ? 'true' : 'false'}
    >
      <span style={{ color: firmware ? 'var(--live)' : 'var(--silk-faint)', display: 'inline-flex', flexShrink: 0 }}>
        {firmware ? <CheckIcon size={15} /> : <PlusIcon size={15} />}
      </span>
      <span style={{ color: firmware ? 'var(--live)' : 'var(--silk-dim)' }}>
        {firmware
          ? <StagedCopy name={firmware.name} placement={placement} />
          : <EmptyCopy placement={placement} />}
      </span>
    </label>
  )
}
