import { useEffect, useRef, useState, type ReactNode } from 'react'
import { AnimatePresence, motion, useReducedMotion } from 'motion/react'
import { CROSSFADE, INSTANT } from './tokens'

// Skeleton-to-content, adapted from interior.dev's `skeleton-swap`.
//
// Two rules from the original are the whole point of it, and both are about
// honesty rather than looks:
//
//  1. The skeleton does not appear for `delay` ms. A request that comes back in
//     80 ms should never flash a loading state; the flash reads as slower than
//     the wait it replaced.
//  2. Once shown, it stays for at least `minVisible` ms. A skeleton that
//     appears and vanishes inside one frame is a flicker, and a flicker is
//     read as a fault.
//
// Point 1 is also why this never outlives its request: the visible state is
// derived from `ready`, not from a timer that has to be cancelled. When the
// request resolves (or fails), `ready` flips and the skeleton is on its way out
// in the same tick.
//
// Adapted for this codebase: theme tokens instead of Tailwind stone/white
// scales, the shared spring vocabulary from ./tokens, and no fixed-height
// shell. The original reserved `lines * lineHeight` and scrolled inside it,
// which suits a docs demo; here the caller supplies the skeleton whose shape
// matches the report it is standing in for, and the surface grows to it.

export interface UseSkeletonSwapOptions {
  /** The request has resolved (either way). */
  ready: boolean
  /** Wait this long before showing a skeleton at all. */
  delay?: number
  /** Once shown, keep it at least this long. */
  minVisible?: number
}

/** The visibility decision on its own, for callers that render their own
 *  loading surface (the drop zone's busy card, for instance). */
export function useSkeletonSwap({
  ready,
  delay = 120,
  minVisible = 360,
}: UseSkeletonSwapOptions): { showSkeleton: boolean } {
  const [visible, setVisible] = useState(false)
  const shownAt = useRef(0)

  useEffect(() => {
    if (!ready) {
      if (visible) return
      const t = window.setTimeout(() => {
        shownAt.current = performance.now()
        setVisible(true)
      }, delay)
      return () => window.clearTimeout(t)
    }
    if (!visible) return
    const rest = Math.max(0, minVisible - (performance.now() - shownAt.current))
    const t = window.setTimeout(() => setVisible(false), rest)
    return () => window.clearTimeout(t)
  }, [ready, visible, delay, minVisible])

  return { showSkeleton: visible }
}

/** A bar of the right shape for one line of text. Widths are irregular on
 *  purpose: an even stack of identical bars reads as a table, not as prose. */
export function SkeletonBar({ width = '100%', height = 10, className = '' }: {
  width?: number | string
  height?: number
  className?: string
}) {
  return (
    <div
      className={`skeleton-bar ${className}`}
      style={{ width, height, borderRadius: Math.min(6, height / 2 + 1) }}
    />
  )
}

export interface SkeletonSwapProps {
  ready: boolean
  children: ReactNode
  /** The stand-in. Required: a generic three-bar block where the reader knows
   *  the report's shape is a worse answer than the report's own shape. */
  skeleton: ReactNode
  /** Announced to assistive tech, and used for the resolved status message. */
  label?: string
  delay?: number
  minVisible?: number
  className?: string
}

export function SkeletonSwap({
  ready,
  children,
  skeleton,
  label,
  delay = 120,
  minVisible = 360,
  className = '',
}: SkeletonSwapProps) {
  const { showSkeleton } = useSkeletonSwap({ ready, delay, minVisible })
  const reduced = useReducedMotion()

  return (
    <div className={`relative grid ${className}`} aria-busy={!ready} aria-label={label}>
      <motion.div
        className="col-start-1 row-start-1 min-w-0"
        initial={false}
        animate={reduced
          ? { opacity: showSkeleton ? 0 : 1 }
          : { opacity: showSkeleton ? 0 : 1, filter: showSkeleton ? 'blur(3px)' : 'blur(0px)' }}
        transition={reduced ? INSTANT : CROSSFADE}
        style={{ pointerEvents: showSkeleton ? 'none' : undefined }}
      >
        {children}
      </motion.div>

      <AnimatePresence initial={false}>
        {showSkeleton && (
          <motion.div
            key="skeleton"
            aria-hidden
            className="pointer-events-none col-start-1 row-start-1 w-full self-start"
            initial={reduced ? { opacity: 1 } : { opacity: 0 }}
            animate={{ opacity: 1 }}
            exit={reduced ? { opacity: 0 } : { opacity: 0, filter: 'blur(3px)' }}
            transition={reduced ? INSTANT : CROSSFADE}
          >
            {skeleton}
          </motion.div>
        )}
      </AnimatePresence>

      {label && (
        <span role="status" className="sr-only">{ready ? `${label} ready` : ''}</span>
      )}
    </div>
  )
}
