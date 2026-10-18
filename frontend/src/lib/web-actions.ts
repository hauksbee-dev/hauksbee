import type { ActionArgs, ActionId, BoardRequest, WebAction } from '../types/report'

// The one place that says what each engine `ActionId` does in this app. The
// engine sends a closed set of ids with the words for the button and the
// arguments (`next_actions`, `sections[].actions`, a check's prerequisites); a
// surface that wants those buttons describes what it can do as an `ActionHost`
// and asks `runnableActions` for the ones it can run. An id the host cannot run
// is dropped, never shown as a button that does nothing.
//
// Not mapped, because the app has nothing for them to open:
//  - `add_project_rules`: the upload carries no project-rules (.kicad_pro) part.
//  - `set_part_number`: the board's part numbers are edited in the layout tool;
//    the model flow's part-number field names a saved model, not the board part.
//  - `mark_not_fitted`: only the Checks view's open-parts sheet marks a part
//    not fitted, and it has its own button for it.
//  - `add_scenario`, `add_ac_sweep`, and `add_supply` without a net: they run
//    only where a checks catalogue lives (`CATALOG_BLOCK`), not from a report.

/** What a surface can do. Leave out whatever it cannot: an action that needs a
 *  missing capability is not offered. */
export interface ActionHost {
  /** Open the model flow for a part, on the draft-from-datasheet tab when asked. */
  giveModel?: (ref: string, tab?: 'draft') => void
  /** Open the evidence slots (firmware, bill of materials, model library). */
  openEvidence?: () => void
  openChecks?: () => void
  /** Queue something for the Checks view to compose. */
  queue?: (request: BoardRequest) => void
  /** Frame and light up parts, a net or a point on the board. */
  show?: (target: { refs: string[]; net?: string; at?: { x: number; y: number }; label: string }) => void
  /** Open the Environment page, where the simulators are installed. */
  openEnvironment?: () => void
}

/** An action this host can run, ready to be a button. */
export interface RunnableAction {
  /** Stable across renders and unique per action id and arguments. */
  key: string
  id: ActionId
  label: string
  testId: string
  run: () => void
}

const argsKey = (a: ActionArgs | undefined) => JSON.stringify(a ?? {})

/** The same step, however many sections asked for it. */
export const sameAction = (a: WebAction, b: WebAction) => a.id === b.id && argsKey(a.args) === argsKey(b.args)

/** The function that runs one action here, or null when this host cannot. */
function handler(action: WebAction, host: ActionHost): (() => void) | null {
  const { refs = [], net, check, volts, x, y } = action.args ?? {}
  const first = refs[0]
  switch (action.id) {
    case 'model_parts':
      return first && host.giveModel ? () => host.giveModel!(first) : null
    case 'extract_unmodelled':
      return first && host.giveModel ? () => host.giveModel!(first, 'draft') : null
    case 'add_bom':
    case 'add_firmware':
    case 'choose_models_folder':
      return host.openEvidence ?? null
    case 'add_supply':
      return net && host.queue && host.openChecks
        ? () => { host.queue!({ type: 'supply', net, ...(volts !== undefined ? { volts } : {}) }); host.openChecks!() }
        : null
    case 'open_check':
      return check && host.queue && host.openChecks
        ? () => {
            host.queue!({ type: 'check', kind: check, ...(net ? { net } : {}), ...(first ? { ref: first } : {}) })
            host.openChecks!()
          }
        : null
    case 'start_checks':
      return host.openChecks ?? null
    case 'show_on_board': {
      const at = x !== undefined && y !== undefined ? { x, y } : undefined
      return host.show && (refs.length > 0 || net || at)
        ? () => host.show!({ refs, net, at, label: refs.join(', ') || net || action.label })
        : null
    }
    case 'install_backend':
      return host.openEnvironment ?? null
    case 'add_project_rules':
    case 'set_part_number':
    case 'mark_not_fitted':
    case 'add_scenario':
    case 'add_ac_sweep':
      return null
  }
}

/** The buttons this host can offer for `actions`, in the engine's order, once
 *  each. */
export function runnableActions(actions: readonly WebAction[] | undefined, host: ActionHost): RunnableAction[] {
  const out: RunnableAction[] = []
  const seen: WebAction[] = []
  for (const action of actions ?? []) {
    if (seen.some(s => sameAction(s, action))) continue
    seen.push(action)
    const run = handler(action, host)
    if (!run) continue
    const ref = action.args?.refs?.[0]
    out.push({
      key: `${action.id}:${argsKey(action.args)}`,
      id: action.id,
      label: action.label,
      // The model button for one part keeps the id the verdict card has always had.
      testId: action.id === 'model_parts' && ref ? `result-give-model-${ref}` : `action-${action.id}`,
      run,
    })
  }
  return out
}

/** Actions in `list` that are not already in `shown` (a second surface does not
 *  repeat a button the first one has). */
export const withoutShown = (list: readonly WebAction[] | undefined, shown: readonly { key: string }[]): WebAction[] =>
  (list ?? []).filter(a => !shown.some(s => s.key === `${a.id}:${argsKey(a.args)}`))

/** Where a checks catalogue finds the minimal block that does what an action
 *  says: the catalogue kind whose example holds it. Used where a check is
 *  blocked on a prerequisite (`check-catalog.ts`); the report never inserts
 *  these. */
export const CATALOG_BLOCK: Partial<Record<ActionId, string>> = {
  add_scenario: 'rail_window',
  add_supply: 'protection_trip',
  add_ac_sweep: 'ac_gain',
}
