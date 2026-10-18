import type { InstallState } from '../hooks/useDeps'
import type { DepInfo } from '../types/report'
import { BusyLine, Callout, LogWell } from './ui'

// The install button and its live progress, shared by the Environment page and
// the Settings backend rows (one flow, one look).

/** Beside the description where the row is wide enough, on its own line below
 *  it when it is not. Held to one line at every width, the button and its cost
 *  caption ran 5px off a 320px viewport and the caption took the click. */
export function InstallButton({ dep, install, onInstall, variant = 'primary', cost }: {
  dep: DepInfo
  install: InstallState
  onInstall: (id: string) => void
  /** The page's one main action is `primary`; every other install is `secondary`. */
  variant?: 'primary' | 'secondary'
  /** The size/cost caption; defaults to the server's wording. */
  cost?: string
}) {
  const busyId = install.phase === 'running' ? install.id : null
  return (
    <div className="flex flex-col items-end gap-1 w-full sm:w-auto min-w-0" style={{ flexShrink: 0 }}>
      <button
        type="button"
        data-testid={`dep-install-${dep.id}`}
        disabled={busyId !== null}
        onClick={() => onInstall(dep.id)}
        className={`${variant === 'primary' ? 'hb-btn-primary' : 'hb-btn'} hb-press px-3.5 py-1.5 text-[12px] font-semibold disabled:opacity-40 disabled:cursor-not-allowed`}
      >
        {busyId === dep.id ? 'Installing ...' : 'Install'}
      </button>
      {(cost ?? dep.cost) && (
        <span className="text-[10px] text-right max-w-full" style={{ color: 'var(--silk-faint)', maxWidth: '14rem' }}>
          {cost ?? dep.cost}
        </span>
      )}
    </div>
  )
}

/** Live install progress and the final state, under the row it belongs to. */
export function InstallProgress({ id, install }: { id: string; install: InstallState }) {
  if (install.phase === 'idle' || install.id !== id) return null
  return (
    <div className="mt-2.5">
      <LogWell lines={install.log} testId="dep-log" />
      {install.phase === 'running' && (
        <BusyLine className="mt-1.5">Installing; this can take a few minutes on a slow connection.</BusyLine>
      )}
      {install.phase === 'ended' && (
        <Callout
          tone={install.ok ? 'ok' : 'err'}
          testId="dep-install-result"
          live
          className="mt-1.5 px-3 py-2 text-[12px] whitespace-pre-wrap"
        >
          {install.ok ? 'Installed and verified. The status is refreshed from a real re-probe.' : install.message}
        </Callout>
      )}
    </div>
  )
}
