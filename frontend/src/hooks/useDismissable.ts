import { useEffect, useRef } from 'react'
import type { Dispatch, RefObject, SetStateAction } from 'react'

/**
 * Close an open overlay on an outside click or on Escape.
 *
 * Every dismissible surface owes the user both ways out: a menu that only
 * closes by re-clicking its own trigger sits over the work while you try to
 * read the thing it covers. `onEscape`, when given, REPLACES the plain close
 * for the Escape key, which is how the layers panel also stops propagation and
 * hands focus back to its trigger. It is read through a ref so a fresh closure
 * each render does not re-subscribe the listeners.
 */
export function useDismissable(
  open: boolean,
  wrap: RefObject<HTMLElement | null>,
  setOpen: Dispatch<SetStateAction<boolean>>,
  onEscape?: (e: KeyboardEvent) => void,
) {
  const escape = useRef(onEscape)
  escape.current = onEscape

  useEffect(() => {
    if (!open) return
    const onDown = (e: MouseEvent) => {
      if (!wrap.current?.contains(e.target as Node)) setOpen(false)
    }
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== 'Escape') return
      if (escape.current) escape.current(e)
      else setOpen(false)
    }
    document.addEventListener('mousedown', onDown)
    document.addEventListener('keydown', onKey)
    return () => {
      document.removeEventListener('mousedown', onDown)
      document.removeEventListener('keydown', onKey)
    }
  }, [open, wrap, setOpen])
}
