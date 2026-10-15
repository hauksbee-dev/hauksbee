import { useMemo } from 'react'
import type { WebReport } from '../../types/report'
import { coverageFor, padNetsOf } from '../../lib/report-view'
import { vocabulary } from '../../lib/result-links'
import type { SelectedComponent } from '../SelectionCard'
import { ResultLinkContext, type ResultLinkHandlers } from '../report/ResultLinks'

/** The Checks page's reach into the board: a part or net chip on a check row
 *  goes to the Board view with that part or net selected (its card open), the
 *  same selection a click on the canvas makes. */
export function ChecksLinks({ report, onShow, selectNet, selectComponent, children }: {
  report: WebReport
  /** Switch to the Board view. */
  onShow: () => void
  selectNet: (net: string | null) => void
  selectComponent: (c: SelectedComponent | null) => void
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
  return <ResultLinkContext.Provider value={links}>{children}</ResultLinkContext.Provider>
}
