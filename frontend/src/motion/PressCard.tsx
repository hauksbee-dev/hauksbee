import { useCallback, useEffect, useRef, useState, type ReactNode } from 'react'
import { motion, useReducedMotion } from 'motion/react'
import { CELL } from './tokens'

// Press-and-hover feedback for a card-shaped button, adapted from
// interior.dev's `press-depth`.
//
// The hook is vendored nearly whole, because the fiddly part is not the
// animation, it is knowing when the press ENDED. A `:active` pseudo-class lies
// in three situations that all happen constantly: the pointer leaves the
// element while still held (the press is cancelled, but `:active` sticks until
// release), the window loses focus mid-press, and a keyboard Space-hold never
// gets a pointer event at all. Each leaves a control looking pressed when it is
// not. The pointer capture below tracks all three.
//
// What was cut: the original renders a physical slab with a coloured underside
// that the face sinks into, and a perspective tilt toward the press point. On a
// sample card in a tool panel that is a toy. The face translates 1 px, and the
// origin is kept because the VIEW that opens from a card uses it (a report
// arriving from the card that was clicked, rather than from nowhere).

export interface PressOrigin {
  /** Press point within the element, in -1..1 from its centre. */
  x: number
  y: number
  /** Press point in viewport pixels, for an origin-aware view transition. */
  clientX: number
  clientY: number
}

export function usePressDepth({ disabled = false }: { disabled?: boolean } = {}): {
  pressed: boolean
  origin: PressOrigin | null
  ref: (node: HTMLElement | null) => void
  bind: {
    onPointerDown: (e: React.PointerEvent) => void
    onKeyDown: (e: React.KeyboardEvent) => void
    onKeyUp: (e: React.KeyboardEvent) => void
    onBlur: () => void
  }
} {
  const [pressed, setPressed] = useState(false)
  const [tracking, setTracking] = useState(false)
  const [origin, setOrigin] = useState<PressOrigin | null>(null)

  const node = useRef<HTMLElement | null>(null)
  const pointer = useRef<number | null>(null)

  const stop = useCallback(() => {
    pointer.current = null
    setTracking(false)
    setPressed(false)
  }, [])

  useEffect(() => {
    if (!tracking) return
    const inside = (e: PointerEvent) => {
      const el = node.current
      if (!el) return false
      const r = el.getBoundingClientRect()
      return e.clientX >= r.left && e.clientX <= r.right
        && e.clientY >= r.top && e.clientY <= r.bottom
    }
    const move = (e: PointerEvent) => {
      if (e.pointerId !== pointer.current) return
      setPressed(inside(e))
    }
    const lift = (e: PointerEvent) => {
      if (e.pointerId !== pointer.current) return
      stop()
    }
    const bail = () => stop()
    const hidden = () => { if (document.hidden) stop() }

    window.addEventListener('pointermove', move)
    window.addEventListener('pointerup', lift)
    window.addEventListener('pointercancel', lift)
    window.addEventListener('blur', bail)
    document.addEventListener('visibilitychange', hidden)
    return () => {
      window.removeEventListener('pointermove', move)
      window.removeEventListener('pointerup', lift)
      window.removeEventListener('pointercancel', lift)
      window.removeEventListener('blur', bail)
      document.removeEventListener('visibilitychange', hidden)
    }
  }, [tracking, stop])

  useEffect(() => { if (disabled) stop() }, [disabled, stop])

  const ref = useCallback((next: HTMLElement | null) => { node.current = next }, [])

  const bind = {
    onPointerDown: (e: React.PointerEvent) => {
      if (disabled) return
      if (e.pointerType === 'mouse' && e.button !== 0) return
      const r = e.currentTarget.getBoundingClientRect()
      setOrigin({
        x: Math.max(-1, Math.min(1, ((e.clientX - r.left) / r.width) * 2 - 1)),
        y: Math.max(-1, Math.min(1, ((e.clientY - r.top) / r.height) * 2 - 1)),
        clientX: e.clientX,
        clientY: e.clientY,
      })
      pointer.current = e.pointerId
      setTracking(true)
      setPressed(true)
    },
    onKeyDown: (e: React.KeyboardEvent) => {
      if (disabled || e.repeat) return
      if (e.key === ' ' || e.key === 'Enter') setPressed(true)
    },
    onKeyUp: (e: React.KeyboardEvent) => {
      if (e.key === ' ' || e.key === 'Enter' || e.key === 'Escape') setPressed(false)
    },
    onBlur: () => stop(),
  }

  return { pressed, origin, ref, bind }
}

export interface PressCardProps {
  children: ReactNode
  /** Receives where the press landed, so the surface that opens from this card
   *  can animate out of it rather than out of the page centre. */
  onPress: (origin: PressOrigin | null) => void
  disabled?: boolean
  className?: string
  style?: React.CSSProperties
  'data-testid'?: string
}

export function PressCard({
  children, onPress, disabled = false, className = '', style, ...rest
}: PressCardProps) {
  const reduced = useReducedMotion()
  const { pressed, origin, ref, bind } = usePressDepth({ disabled })
  const [hovered, setHovered] = useState(false)

  return (
    <motion.button
      ref={ref}
      type="button"
      disabled={disabled}
      data-testid={rest['data-testid']}
      onClick={() => onPress(origin)}
      onPointerEnter={() => setHovered(true)}
      onPointerLeave={() => setHovered(false)}
      initial={false}
      animate={reduced ? {} : { y: pressed ? 1 : hovered ? -1 : 0 }}
      transition={CELL}
      style={{ touchAction: 'manipulation', ...style }}
      className={className}
      {...bind}
    >
      {children}
    </motion.button>
  )
}
