// Bringing something into view inside the report. `Element.scrollIntoView`
// scrolls EVERY scrollable ancestor, including the app shell (`h-screen
// overflow-hidden`), which then sits a few pixels off with its header clipped
// and nothing the user can scroll back. This scrolls only the nearest
// scrolling ancestor.

const scrollsY = (el: HTMLElement) =>
  /(auto|scroll|overlay)/.test(getComputedStyle(el).overflowY) && el.scrollHeight > el.clientHeight

/** The nearest ancestor that actually scrolls vertically, or null. */
function scroller(el: HTMLElement): HTMLElement | null {
  for (let p = el.parentElement; p; p = p.parentElement) if (scrollsY(p)) return p
  return null
}

/** Centre `el` in its scrolling ancestor. A short hop eases; a long one (a
 *  report is tens of thousands of pixels on a phone) lands at once, because a
 *  smooth scroll that long is slower than the user's attention and can be
 *  caught mid-flight over unrelated content. */
export function centerInScroller(el: HTMLElement | null): void {
  if (!el) return
  const box = scroller(el)
  if (!box) return
  const from = box.getBoundingClientRect()
  const to = el.getBoundingClientRect()
  const delta = to.top + to.height / 2 - (from.top + from.height / 2)
  const top = Math.max(0, Math.min(box.scrollHeight - box.clientHeight, box.scrollTop + delta))
  box.scrollTo({ top, behavior: Math.abs(delta) > box.clientHeight * 1.5 ? 'auto' : 'smooth' })
}
