import { AnimatePresence, motion, useReducedMotion } from 'motion/react'
import { INSTANT, LEAVE, SETTLE } from './tokens'

// A verdict chip whose text changes (PASS becoming FAIL after a re-run),
// adapted from the icon-swap half of interior.dev's `inline-validation`.
//
// The original crossfades two fixed glyphs in a grid cell, which is exactly the
// right structure: both faces occupy the same cell, so the row never reflows
// when the verdict flips. Kept whole. What differs is the payload (a word, not
// an icon) and the entry: a verdict arriving for the first time after a run
// scales up from 0.9, which reads as "this is new", while a verdict REPLACING
// another only crossfades, which reads as "this changed". Those are different
// events and the animation says which one happened.
//
// Colour is not animated. It is set by the same style the static chip always
// used, so the chip is the correct colour on its first painted frame; a badge
// that fades from grey into red spends 200 ms telling the reader nothing.

export interface VerdictBadgeProps {
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
