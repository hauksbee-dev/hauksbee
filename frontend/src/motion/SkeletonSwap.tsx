import { useEffect, useRef, useState, type ReactNode } from 'react'
import { AnimatePresence, motion, useReducedMotion } from 'motion/react'
import { CROSSFADE, INSTANT } from './tokens'

// Skeleton-to-content, vendored from interior.dev's `skeleton-swap`. Two rules
// carry it, and both are about honesty rather than looks:
//
//  1. The skeleton does not appear for `delay` ms. A request that comes back in
//     80 ms should never flash a loading state; the flash reads as slower than
//     the wait it replaced.
//  2. Once shown, it stays for at least `minVisible` ms. A skeleton that
//     appears and vanishes inside one frame is a flicker, and a flicker reads
//     as a fault.
//
// The visible state is derived from `ready`, never from a timer that has to be
// cancelled, so it can never outlive its request. The caller supplies the
// skeleton whose shape matches the content it stands in for.

interface UseSkeletonSwapOptions {
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

interface SkeletonSwapProps {
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
