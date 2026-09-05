import { AnimatePresence, motion, useReducedMotion } from 'motion/react'
import { INSTANT, LEAVE, SETTLE } from './tokens'

// A verdict chip whose text changes (PASS becoming FAIL after a re-run),
// vendored from the icon-swap half of interior.dev's `inline-validation`.
//
// Both faces occupy the same grid cell, so the row never reflows when the
// verdict flips. A verdict arriving for the first time scales up from 0.9
// ("this is new"); one REPLACING another only crossfades ("this changed").
//
// Colour is not animated: it comes from the caller's own style, so the chip is
// the right colour on its first painted frame. A badge that fades from grey
// into red spends 200 ms telling the reader nothing.

interface VerdictBadgeProps {
  /** The word shown. A change to this drives the swap. */
  label: string
  className?: string
  style?: React.CSSProperties
  title?: string
  'data-testid'?: string
}

export function VerdictBadge({ label, className = '', style, title, ...rest }: VerdictBadgeProps) {
  const reduced = useReducedMotion()

  return (
    <span
      className={className}
      style={{ ...style, display: 'inline-grid', placeItems: 'center' }}
      title={title}
      data-testid={rest['data-testid']}
    >
      <AnimatePresence initial mode="popLayout">
        <motion.span
          key={label}
          className="col-start-1 row-start-1"
          initial={reduced ? { opacity: 1 } : { opacity: 0, scale: 0.9 }}
          animate={{ opacity: 1, scale: 1 }}
          exit={reduced ? { opacity: 0, transition: INSTANT } : { opacity: 0, scale: 0.94, transition: LEAVE }}
          transition={reduced ? INSTANT : SETTLE}
        >
          {label}
        </motion.span>
      </AnimatePresence>
    </span>
  )
}
