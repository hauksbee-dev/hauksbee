// The motion vocabulary, in one file, so nothing in the app invents a duration.
//
// Vendored from interior.dev (https://github.com/ddoemonn/interior, the
// copy-the-source component collection built on the `motion` package). Its
// components each declared these springs locally; here they are shared, because
// an instrument panel with three different crossfade curves reads as three
// different products.
//
// The rule this codebase applies on top of interior's defaults: motion is here
// to make a state change legible, never to decorate. Anything that moves is
// answering a question the user just asked ("did it take my file?", "is it
// still working?", "did that number change?"). Everything is in the
// 100-250 ms band, and every curve is asymmetric in the honest direction:
// things arrive quickly and leave faster.

/** Snap: a control acknowledging a press or a hover. */
export const CELL = { type: 'spring', stiffness: 520, damping: 34, mass: 0.45 } as const

/** Settle: a value or a badge coming to rest after it changed. */
export const SETTLE = { type: 'spring', stiffness: 380, damping: 30, mass: 0.6 } as const

/** Crossfade: one state of a surface replacing another (skeleton to content,
 *  idle face to pending face). */
export const CROSSFADE = { type: 'spring', stiffness: 300, damping: 34, mass: 0.7 } as const

/** Arrive: content entering for the first time. ~180 ms, decelerating. */
export const ARRIVE = { duration: 0.18, ease: [0.23, 1, 0.32, 1] } as const

/** Leave: anything on its way out. Deliberately faster than ARRIVE; a row
 *  being removed should not hold its slot open while the reader waits. */
export const LEAVE = { duration: 0.12, ease: [0.4, 0, 1, 1] } as const

/** No motion at all, for the reduced-motion branch. Every component below
 *  takes this path rather than shortening a duration: a 60 ms bounce is still
 *  a bounce. */
export const INSTANT = { duration: 0 } as const

/** Per-item delay for a staggered arrival, and the cap on how far the stagger
 *  is allowed to run. A twenty-row list that staggers all the way is a list
 *  that takes two seconds to become readable, so the delay stops accumulating
 *  after `STAGGER_CAP`. */
export const STAGGER_STEP = 0.035
export const STAGGER_CAP = 0.21

/** The delay of the nth item in a staggered group. */
export function staggerDelay(index: number): number {
  return Math.min(index * STAGGER_STEP, STAGGER_CAP)
}
