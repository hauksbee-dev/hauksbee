import { createContext, useContext, useMemo } from 'react'
import type { WebReport } from '../../types/report'
import { coverageFor, padNetsOf } from '../../lib/report-view'
import { vocabulary } from '../../lib/result-links'
import type { SelectedComponent } from '../SelectionCard'
import { ResultLinkContext, type ResultLinkHandlers } from '../report/ResultLinks'

/** Switch to the Board view, when the Checks page is mounted beside one. */
export const ShowBoardContext = createContext<(() => void) | null>(null)

/** Light a set of nets together on the Board view and go there (the unrouted
 *  nets of a not-yet-routed board), when the Checks page sits beside one. */
export const ShowNetsContext = createContext<((nets: string[], label: string) => void) | null>(null)

/** Words that point at the board ("click a trace on the board") as a button
 *  that goes there, or as plain words where there is no board to go to. */
export function ToBoard({ children }: { children: React.ReactNode }) {
  const show = useContext(ShowBoardContext)
  if (!show) return <>{children}</>
  return (
    <button
      type="button"
      data-testid="to-board"
      onClick={show}
      className="hb-press underline underline-offset-2 text-left"
      style={{ color: 'var(--copper)', background: 'none', border: 'none', padding: 0, font: 'inherit' }}
    >
      {children}
    </button>
  )
}

/** The Checks page's reach into the board: a part or net chip on a check row
 *  goes to the Board view with that part or net selected (its card open), the
 *  same selection a click on the canvas makes. */
export function ChecksLinks({ report, onShow, selectNet, selectComponent, showNets, children }: {
  report: WebReport
  /** Switch to the Board view. */
  onShow: () => void
  selectNet: (net: string | null) => void
  selectComponent: (c: SelectedComponent | null) => void
  /** Light these nets on the board, labelled, and switch to it. */
  showNets?: (nets: string[], label: string) => void
  children: React.ReactNode
}) {
  const links = useMemo<ResultLinkHandlers>(() => ({
    vocab: vocabulary(report),
    onGo: entity => {
      if (entity.kind === 'part') {
        const row = coverageFor(report.model_coverage, entity.ref)
        const padNets = row ? padNetsOf(row) : []
        selectNet(null)
        selectComponent({
          ref: entity.ref,
          value: row?.value ?? report.components?.find(c => c.reference === entity.ref)?.value ?? '',
          lib_id: row?.lib_id ?? '',
          padNet: padNets[0] ?? null,
          padNets,
        })
      } else {
        selectComponent(null)
        selectNet(entity.net)
      }
      onShow()
    },
  }), [report, onShow, selectNet, selectComponent])
  return (
    <ShowBoardContext.Provider value={onShow}>
      <ShowNetsContext.Provider value={showNets ?? null}>
        <ResultLinkContext.Provider value={links}>{children}</ResultLinkContext.Provider>
      </ShowNetsContext.Provider>
    </ShowBoardContext.Provider>
  )
}
