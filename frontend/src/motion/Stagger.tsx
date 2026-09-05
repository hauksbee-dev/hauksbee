import { type ReactNode } from 'react'
import { motion, useReducedMotion } from 'motion/react'
import { ARRIVE, INSTANT, staggerDelay } from './tokens'

// Content arriving for the first time, vendored from the entry half of
// interior.dev's `filter-grid`. Three constraints keep it restrained:
//
//  - `initial` is a 4 px rise and an opacity fade. No scale, no bounce: a
//    section that springs past its resting position is one the eye has to chase.
//  - The stagger is capped (see ./tokens): thirty rows must not take a second
//    and a half to finish arriving.
//  - It runs ONCE, on mount, so a report that re-renders because a net was
//    clicked does not re-animate.

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
