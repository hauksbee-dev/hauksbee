import type { RunnableAction } from '../../lib/web-actions'
import { RowAction } from './ResultLinks'

/** Buttons for the engine's typed actions, already filtered to the ones this
 *  surface can run (`runnableActions`). Nothing renders for an empty list.
 *
 *  `step` is the size of the verdict card's Next step buttons; `row` is the
 *  smaller button on a finding card. Only the first button can be the primary
 *  one, and only when `primary` says the surface has no other.
 *  Render it inside the flex row that holds the other buttons. */
export function ActionButtons({ actions, size, primary = false }: {
  actions: readonly RunnableAction[]
  size: 'step' | 'row'
  primary?: boolean
}) {
  if (actions.length === 0) return null
  return (
    <>
      {actions.map((a, i) => size === 'row' ? (
        <RowAction key={a.key} testId={a.testId} onClick={a.run}>{a.label}</RowAction>
      ) : (
        <button
          key={a.key} type="button" data-testid={a.testId} data-action={a.id} onClick={a.run}
          className={`${primary && i === 0 ? 'hb-btn-primary' : 'hb-btn'} hb-press px-3 py-1.5 text-[12px]`}
        >
          {a.label}
        </button>
      ))}
    </>
  )
}
