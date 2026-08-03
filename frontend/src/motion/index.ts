// The app's motion layer: components vendored from interior.dev
// (https://github.com/ddoemonn/interior, MIT, a copy-the-source collection
// built on the `motion` package) and adapted to this codebase's theme tokens,
// vocabulary and standards.
//
// The house rules every file here obeys:
//  - 100-250 ms, restrained easings, no bounce past rest.
//  - `useReducedMotion()` takes the no-motion branch, not a shorter one.
//  - Nothing animates for its own sake. Every moving thing is the answer to a
//    question the user just asked with an input.
//  - An async animation never outlives its request: loading states are derived
//    from the request's own resolved flag, never from a timer.

export { CELL, SETTLE, CROSSFADE, ARRIVE, LEAVE, INSTANT, staggerDelay } from './tokens'
export { SkeletonSwap, SkeletonBar, useSkeletonSwap } from './SkeletonSwap'
export { ValueSettle, useValueSettle, leadingNumber } from './ValueSettle'
export { PressCard, usePressDepth, type PressOrigin } from './PressCard'
export { StaggerItem, ArriveOnce } from './Stagger'
export { useDropTarget, type DropState } from './DropField'
export { VerdictBadge } from './VerdictBadge'
export { EmptyState } from './EmptyState'
