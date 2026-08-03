import { useEffect, useRef, useState, type ReactNode } from 'react'
import { AnimatePresence, motion, useReducedMotion } from 'motion/react'
import { INSTANT, LEAVE, SETTLE } from './tokens'

// A number (or a short measured string) that changed, adapted from
// interior.dev's `value-flash`.
//
// What was kept: the vertical roll, so a value that went up enters from below
// and a value that went down enters from above. That direction is the only part
// of the animation carrying information, and it is information the reader
// would otherwise have to remember.
//
// What was cut, and why: the original tints the whole cell green or red and
// pops a triangle glyph next to it. This is an instrument panel where green and
// red already mean PASS and FAIL, and a measured value going up is not good
// news or bad news; it is a measurement. Colouring it would be the UI having an
// opinion the physics does not support. So the value moves and does not change
// colour, and the arrow is gone.

export type SettleDirection = 'up' | 'down'

/** Which way the value went, and a key that changes once per change so
 *  AnimatePresence has something to swap on. */
export function useValueSettle(text: string, numeric: number | null): {
  direction: SettleDirection | null
  changeId: number
} {
  const [state, setState] = useState({ direction: null as SettleDirection | null, changeId: 0 })
  const previous = useRef({ text, numeric })

  useEffect(() => {
    const prior = previous.current
    if (prior.text === text) return
    previous.current = { text, numeric }
    const delta = numeric != null && prior.numeric != null ? numeric - prior.numeric : 0
    setState(prev => ({
      direction: delta === 0 ? null : delta > 0 ? 'up' : 'down',
      changeId: prev.changeId + 1,
    }))
  }, [text, numeric])

  return state
}

/** Pull the first number out of a measured detail line ("3.284 V at 40 ms"),
 *  so the roll direction can be right without the caller parsing it. Returns
 *  null when there is no leading number, in which case the value still swaps,
 *  just without a direction. */
export function leadingNumber(s: string): number | null {
  const m = /-?\d+(\.\d+)?([eE][-+]?\d+)?/.exec(s)
  if (!m) return null
  const n = Number(m[0])
  return Number.isFinite(n) ? n : null
}

export function ValueSettle({ children, value, className = '', style }: {
  /** How the value renders. Defaults to the value itself. */
  children?: ReactNode
  /** The text whose change drives the animation. */
  value: string
  className?: string
  style?: React.CSSProperties
}) {
  const reduced = useReducedMotion()
  const { direction, changeId } = useValueSettle(value, leadingNumber(value))

  // Reduced motion gets the new text, immediately, with no swap: the point of
  // the roll is legibility, and a reader who asked for no motion is telling us
  // it costs them more than it gives.
  if (reduced) {
    return <span className={className} style={style}>{children ?? value}</span>
  }

  return (
    <span className={`relative inline-grid overflow-hidden align-bottom ${className}`} style={style}>
      <AnimatePresence initial={false} mode="popLayout">
        <motion.span
          key={changeId}
          className="col-start-1 row-start-1"
          initial={{ opacity: 0, y: direction === 'down' ? '-0.7em' : '0.7em' }}
          animate={{ opacity: 1, y: '0em' }}
          exit={{
            opacity: 0,
            y: direction === 'down' ? '0.6em' : '-0.6em',
            transition: LEAVE,
          }}
          transition={direction ? SETTLE : INSTANT}
        >
          {children ?? value}
        </motion.span>
      </AnimatePresence>
    </span>
  )
}
