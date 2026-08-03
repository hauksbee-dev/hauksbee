import { type ReactNode } from 'react'
import { motion, useReducedMotion } from 'motion/react'
import { ARRIVE, INSTANT, staggerDelay } from './tokens'

// Content arriving for the first time, adapted from the entry half of
// interior.dev's `filter-grid`.
//
// Three constraints make this restrained rather than decorative:
//
//  - `initial` is a 4 px rise and an opacity fade. No scale, no bounce. A
//    report section that springs past its resting position is a report section
//    the reader's eye has to chase.
//  - The stagger is capped (see ./tokens). A findings list of thirty rows must
//    not take a second and a half to finish arriving.
//  - It runs ONCE, on mount. There is no `layout` prop and no re-trigger on
//    prop change, so a report that re-renders because a net was clicked does
//    not re-animate. The original ran the same entry on every filter change,
//    which is right for a filter and wrong for a report.

export function StaggerItem({ index = 0, children, className = '', style }: {
  index?: number
  children: ReactNode
  className?: string
  style?: React.CSSProperties
}) {
  const reduced = useReducedMotion()
  if (reduced) return <div className={className} style={style}>{children}</div>
  return (
    <motion.div
      className={className}
      style={style}
      initial={{ opacity: 0, y: 4 }}
      animate={{ opacity: 1, y: 0 }}
      transition={{ ...ARRIVE, delay: staggerDelay(index) }}
    >
      {children}
    </motion.div>
  )
}

/** One surface fading in as a whole, with no per-child stagger: for a panel
 *  that is a single thought rather than a list. */
export function ArriveOnce({ children, className = '', style, delay = 0 }: {
  children: ReactNode
  className?: string
  style?: React.CSSProperties
  delay?: number
}) {
  const reduced = useReducedMotion()
  return (
    <motion.div
      className={className}
      style={style}
      initial={reduced ? { opacity: 1 } : { opacity: 0, y: 4 }}
      animate={{ opacity: 1, y: 0 }}
      transition={reduced ? INSTANT : { ...ARRIVE, delay }}
    >
      {children}
    </motion.div>
  )
}
