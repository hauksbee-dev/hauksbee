// The embed contract: the names and numbers both sides agree on. Deliberately
// free of React and of the app, so the host-side module (hauksbee-embed.js) can
// import it without pulling the widget bundle onto a page that never scrolls to
// the demo.

export type EmbedState = 'compact' | 'expanded'

export type EmbedEventName =
  | 'ready' | 'engaged' | 'idle' | 'requestExpand' | 'requestCollapse' | 'error'

export interface EmbedEvent {
  type: EmbedEventName
  payload?: Record<string, unknown>
}

/** Height the host should give the widget in each state. Advisory: the widget
 *  fills whatever box it is given, and this says which box suits the state. */
export const SUGGESTED_HEIGHT: Record<EmbedState, number> = { compact: 340, expanded: 760 }

/** The one line that is on screen in both states, unconditionally. */
export const HONESTY_LINE =
  'A recorded run of the real engine on this board. Your boards run locally.'
