import { useCallback, useEffect, useRef, useState, type ReactNode } from 'react'
import { AnimatePresence, motion, useReducedMotion } from 'motion/react'

// The app's motion layer, in one file, so nothing else invents a duration.
// Adapted from interior.dev (MIT). The rule: motion makes a state change
// legible, never decorates. Everything is in the 100-250 ms band, every curve
// is asymmetric in the honest direction (arrive quickly, leave faster), and
// reduced motion takes the no-motion branch rather than a shorter bounce.

// ── Tokens ───────────────────────────────────────────────────────────────────

/** Snap: a control acknowledging a press or a hover. */
export const CELL = { type: 'spring', stiffness: 520, damping: 34, mass: 0.45 } as const
/** Settle: a value or a badge coming to rest after it changed. */
export const SETTLE = { type: 'spring', stiffness: 380, damping: 30, mass: 0.6 } as const
/** Arrive: content entering for the first time. ~180 ms, decelerating. */
export const ARRIVE = { duration: 0.18, ease: [0.23, 1, 0.32, 1] } as const
/** Leave: anything on its way out. Faster than ARRIVE; a row being removed
 *  must not hold its slot open while the reader waits. */
export const LEAVE = { duration: 0.12, ease: [0.4, 0, 1, 1] } as const
/** No motion at all, for the reduced-motion branch. */
export const INSTANT = { duration: 0 } as const

/** Per-item delay of a staggered arrival, capped: a twenty-row list that
 *  staggers all the way takes two seconds to become readable. */
const staggerDelay = (index: number) => Math.min(index * 0.035, 0.21)

interface Boxed {
  children: ReactNode
  className?: string
  style?: React.CSSProperties
}

// ── Arrival ──────────────────────────────────────────────────────────────────

/** Content arriving for the first time: a 4 px rise and a fade (no scale, no
 *  bounce), staggered by `index`, once on mount so a report that re-renders
 *  because a net was clicked does not re-animate. */
export function StaggerItem({ index = 0, children, className = '', style }: Boxed & { index?: number }) {
  const reduced = useReducedMotion()
  if (reduced) return <div className={className} style={style}>{children}</div>
  return (
    <motion.div className={className} style={style} initial={{ opacity: 0, y: 4 }} animate={{ opacity: 1, y: 0 }} transition={{ ...ARRIVE, delay: staggerDelay(index) }}>
      {children}
    </motion.div>
  )
}

/** One surface fading in as a whole, with no per-child stagger: for a panel
 *  that is a single thought rather than a list. */
export function ArriveOnce({ children, className = '', style }: Boxed) {
  const reduced = useReducedMotion()
  return (
    <motion.div
      className={className}
      style={style}
      initial={reduced ? { opacity: 1 } : { opacity: 0, y: 4 }}
      animate={{ opacity: 1, y: 0 }}
      transition={reduced ? INSTANT : ARRIVE}
    >
      {children}
    </motion.div>
  )
}

/** A designed empty state: a sentence that says what is missing and one
 *  action that fixes it. One fade-in on arrival; an empty state that animates
 *  repeatedly draws attention to the absence of content. */
export function EmptyState({ title, body, action, testId }: {
  /** What is not here, as a statement. Not "No data". */
  title: string
  /** Why it is not here, or what would put something here. */
  body: ReactNode
  /** The one action. Optional only when the absence has no remedy from here. */
  action?: ReactNode
  testId?: string
}) {
  return (
    <ArriveOnce className="rounded-xl px-5 py-6 text-center" style={{ border: '1px dashed var(--hairline)', background: 'var(--surface)' }}>
      <div data-testid={testId}>
        <div className="text-[13px] font-semibold" style={{ color: 'var(--silk)' }}>{title}</div>
        <div className="mt-1.5 text-[12px] leading-relaxed mx-auto" style={{ color: 'var(--silk-dim)', maxWidth: '26rem' }}>{body}</div>
        {action && <div className="mt-3.5 flex justify-center">{action}</div>}
      </div>
    </ArriveOnce>
  )
}

// ── Values that change ───────────────────────────────────────────────────────

/** The first number in a measured detail line ("3.284 V at 40 ms"), so the
 *  roll direction can be right without the caller parsing it. */
function leadingNumber(s: string): number | null {
  const m = /-?\d+(\.\d+)?([eE][-+]?\d+)?/.exec(s)
  const n = m ? Number(m[0]) : NaN
  return Number.isFinite(n) ? n : null
}

/** A number (or short measured string) that changed. The vertical roll is the
 *  whole point: a value that went up enters from below, one that went down
 *  from above; that direction is the only information the animation carries.
 *  No colour: a measurement going up is neither good nor bad news. */
export function ValueSettle({ children, value, className = '', style }: Partial<Boxed> & { value: string }) {
  const reduced = useReducedMotion()
  const [state, setState] = useState<{ direction: 'up' | 'down' | null; changeId: number }>({ direction: null, changeId: 0 })
  const previous = useRef(value)
  useEffect(() => {
    if (previous.current === value) return
    const before = leadingNumber(previous.current), after = leadingNumber(value)
    previous.current = value
    const delta = before != null && after != null ? after - before : 0
    setState(prev => ({ direction: delta === 0 ? null : delta > 0 ? 'up' : 'down', changeId: prev.changeId + 1 }))
  }, [value])

  // Reduced motion gets the new text immediately: the roll exists for
  // legibility, and a reader who asked for no motion says it costs more.
  if (reduced) return <span className={className} style={style}>{children ?? value}</span>
  const { direction, changeId } = state
  return (
    <span className={`relative inline-grid overflow-hidden align-bottom ${className}`} style={style}>
      <AnimatePresence initial={false} mode="popLayout">
        <motion.span
          key={changeId}
          className="col-start-1 row-start-1"
          initial={{ opacity: 0, y: direction === 'down' ? '-0.7em' : '0.7em' }}
          animate={{ opacity: 1, y: '0em' }}
          exit={{ opacity: 0, y: direction === 'down' ? '0.6em' : '-0.6em', transition: LEAVE }}
          transition={direction ? SETTLE : INSTANT}
        >
          {children ?? value}
        </motion.span>
      </AnimatePresence>
    </span>
  )
}

/** A verdict chip whose word changes (PASS becoming FAIL after a re-run). Both
 *  faces share one grid cell, so the row never reflows. Colour is not
 *  animated: it comes from the caller's style, so the chip is the right colour
 *  on its first painted frame. */
export function VerdictBadge({ label, className = '', style, title, ...rest }: {
  label: string
  className?: string
  style?: React.CSSProperties
  title?: string
  'data-testid'?: string
}) {
  const reduced = useReducedMotion()
  return (
    <span className={className} style={{ ...style, display: 'inline-grid', placeItems: 'center' }} title={title} data-testid={rest['data-testid']}>
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

// ── Loading ──────────────────────────────────────────────────────────────────

/** Skeleton visibility, derived from `ready` and never from a timer that has
 *  to be cancelled. Two honesty rules: no skeleton for the first `delay` ms
 *  (an 80 ms answer must not flash a loading state), and once shown it stays
 *  `minVisible` ms (a one-frame skeleton reads as a fault). */
export function useSkeletonSwap({ ready, delay = 120, minVisible = 360 }: {
  ready: boolean
  delay?: number
  minVisible?: number
}): { showSkeleton: boolean } {
  const [visible, setVisible] = useState(false)
  const shownAt = useRef(0)
  useEffect(() => {
    if (!ready) {
      if (visible) return
      const t = window.setTimeout(() => { shownAt.current = performance.now(); setVisible(true) }, delay)
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
export function SkeletonBar({ width = '100%', height = 10, className = '' }: { width?: number | string; height?: number; className?: string }) {
  return <div className={`skeleton-bar ${className}`} style={{ width, height, borderRadius: Math.min(6, height / 2 + 1) }} />
}

// ── Pressing ─────────────────────────────────────────────────────────────────

export interface PressOrigin {
  /** Press point within the element, in -1..1 from its centre. */
  x: number
  y: number
  /** Press point in viewport pixels, for an origin-aware view transition. */
  clientX: number
  clientY: number
}

/** Press-and-hover feedback for a card-shaped button. The fiddly part is
 *  knowing when the press ENDED: `:active` lies when the pointer leaves the
 *  element while held, when the window loses focus mid-press, and for a
 *  keyboard Space-hold. The pointer tracking below covers all three. The face
 *  translates 1 px; the press ORIGIN is kept so the view that opens from a
 *  card can animate out of it. */
export function PressCard({ children, onPress, disabled = false, className = '', style, ...rest }: Boxed & {
  onPress: (origin: PressOrigin | null) => void
  disabled?: boolean
  'data-testid'?: string
}) {
  const reduced = useReducedMotion()
  const [pressed, setPressed] = useState(false)
  const [tracking, setTracking] = useState(false)
  const [hovered, setHovered] = useState(false)
  const [origin, setOrigin] = useState<PressOrigin | null>(null)
  const node = useRef<HTMLButtonElement | null>(null)
  const pointer = useRef<number | null>(null)

  const stop = useCallback(() => {
    pointer.current = null
    setTracking(false)
    setPressed(false)
  }, [])

  useEffect(() => {
    if (!tracking) return
    const move = (e: PointerEvent) => {
      if (e.pointerId !== pointer.current) return
      const r = node.current?.getBoundingClientRect()
      setPressed(!!r && e.clientX >= r.left && e.clientX <= r.right && e.clientY >= r.top && e.clientY <= r.bottom)
    }
    const lift = (e: PointerEvent) => { if (e.pointerId === pointer.current) stop() }
    const hidden = () => { if (document.hidden) stop() }
    window.addEventListener('pointermove', move)
    window.addEventListener('pointerup', lift)
    window.addEventListener('pointercancel', lift)
    window.addEventListener('blur', stop)
    document.addEventListener('visibilitychange', hidden)
    return () => {
      window.removeEventListener('pointermove', move)
      window.removeEventListener('pointerup', lift)
      window.removeEventListener('pointercancel', lift)
      window.removeEventListener('blur', stop)
      document.removeEventListener('visibilitychange', hidden)
    }
  }, [tracking, stop])

  useEffect(() => { if (disabled) stop() }, [disabled, stop])

  const key = (e: React.KeyboardEvent) => e.key === ' ' || e.key === 'Enter'
  return (
    <motion.button
      ref={node}
      type="button"
      disabled={disabled}
      data-testid={rest['data-testid']}
      onClick={() => onPress(origin)}
      onPointerEnter={() => setHovered(true)}
      onPointerLeave={() => setHovered(false)}
      onPointerDown={e => {
        if (disabled || (e.pointerType === 'mouse' && e.button !== 0)) return
        const r = e.currentTarget.getBoundingClientRect()
        const clamp = (v: number) => Math.max(-1, Math.min(1, v))
        setOrigin({
          x: clamp(((e.clientX - r.left) / r.width) * 2 - 1),
          y: clamp(((e.clientY - r.top) / r.height) * 2 - 1),
          clientX: e.clientX,
          clientY: e.clientY,
        })
        pointer.current = e.pointerId
        setTracking(true)
        setPressed(true)
      }}
      onKeyDown={e => { if (!disabled && !e.repeat && key(e)) setPressed(true) }}
      onKeyUp={e => { if (key(e) || e.key === 'Escape') setPressed(false) }}
      onBlur={stop}
      initial={false}
      animate={reduced ? {} : { y: pressed ? 1 : hovered ? -1 : 0 }}
      transition={CELL}
      style={{ touchAction: 'manipulation', ...style }}
      className={className}
    >
      {children}
    </motion.button>
  )
}

// ── Dropping ─────────────────────────────────────────────────────────────────

export type DropState =
  /** Nothing being dragged over this target. */
  | 'idle'
  /** Files are over the target. Whether they are readable is the drop's answer. */
  | 'over'
  /** Something is over the target that is definitely not a file. */
  | 'reject'

/** Does this drag carry files? The browser withholds file NAMES during a drag,
 *  so "accepted" is unknowable and a green tick would be a guess; whether it
 *  carries files at all is knowable, and dragging selected text onto the
 *  board zone gets a refusal before the user lets go. */
function carriesFiles(dt: DataTransfer | null): boolean {
  return !!dt && (Array.from(dt.types).includes('Files') || Array.from(dt.items ?? []).some(i => i.kind === 'file'))
}

/** Drag-over feedback for a drop target. `dragenter`/`dragleave` fire for
 *  every descendant the pointer crosses, so a depth counter is what stops the
 *  flicker: only the leave that balances the outermost enter counts. */
export function useDropTarget(onFiles: (files: FileList) => void): {
  state: DropState
  bind: {
    onDragEnter: (e: React.DragEvent) => void
    onDragOver: (e: React.DragEvent) => void
    onDragLeave: (e: React.DragEvent) => void
    onDrop: (e: React.DragEvent) => void
  }
} {
  const [state, setState] = useState<DropState>('idle')
  const depth = useRef(0)
  return {
    state,
    bind: {
      onDragEnter: e => {
        e.preventDefault()
        depth.current += 1
        setState(carriesFiles(e.dataTransfer) ? 'over' : 'reject')
      },
      onDragOver: e => {
        e.preventDefault()
        // Say this is a copy, so the cursor shows the copy badge rather than
        // the browser's default "no" for a target it has not been told about.
        const files = carriesFiles(e.dataTransfer)
        if (e.dataTransfer) e.dataTransfer.dropEffect = files ? 'copy' : 'none'
        // Also the keep-alive: a dragenter missed through a child that stopped
        // propagation still resolves.
        if (depth.current === 0) depth.current = 1
        setState(files ? 'over' : 'reject')
      },
      onDragLeave: e => {
        e.preventDefault()
        depth.current = Math.max(0, depth.current - 1)
        if (depth.current === 0) setState('idle')
      },
      onDrop: e => {
        e.preventDefault()
        depth.current = 0
        setState('idle')
        const files = e.dataTransfer?.files
        if (files && files.length > 0) onFiles(files)
      },
    },
  }
}
