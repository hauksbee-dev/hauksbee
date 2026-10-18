import { useEffect, useState } from 'react'

/** Whether a CSS media query matches right now, kept current as it changes.
 *  Used where the LAYOUT or the WORDING depends on the device (a phone's
 *  one-column transport, "tap" instead of "click" on a touch screen) and a
 *  class-only breakpoint cannot express it. False where matchMedia is absent
 *  (server render, a bare test DOM), so those fall back to the desktop shape. */
export function useMediaQuery(query: string): boolean {
  const read = () => typeof window !== 'undefined' && typeof window.matchMedia === 'function'
    && window.matchMedia(query).matches
  const [matches, setMatches] = useState(read)
  useEffect(() => {
    if (typeof window === 'undefined' || typeof window.matchMedia !== 'function') return
    const mq = window.matchMedia(query)
    const onChange = () => setMatches(mq.matches)
    onChange()
    mq.addEventListener('change', onChange)
    return () => mq.removeEventListener('change', onChange)
  }, [query])
  return matches
}

/** A touch screen as the primary pointer: no hover, no ctrl-scroll. */
export const COARSE_POINTER = '(pointer: coarse)'
/** The phone breakpoint the layout's `sm:` classes switch at. */
export const PHONE_WIDTH = '(max-width: 639px)'
