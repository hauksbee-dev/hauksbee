// Bridge between the CSS theme tokens (index.css :root / [data-theme='light'])
// and canvas/WebGL code, which cannot use var() in fillStyle or material
// colors. Reads are cached per theme so per-frame draw loops pay a Map lookup,
// not a getComputedStyle round-trip.

export type ThemeName = 'dark' | 'light'

export function currentTheme(): ThemeName {
  return document.documentElement.dataset.theme === 'light' ? 'light' : 'dark'
}

export function isLightTheme(): boolean {
  return currentTheme() === 'light'
}

const cache = new Map<string, string>()
let cacheTheme: ThemeName | null = null

/** Resolved value of a CSS custom property (e.g. '--scope-face') for the
 *  current theme. */
export function cssToken(name: string): string {
  const t = currentTheme()
  if (t !== cacheTheme) {
    cache.clear()
    cacheTheme = t
  }
  let v = cache.get(name)
  if (v === undefined) {
    v = getComputedStyle(document.documentElement).getPropertyValue(name).trim()
    cache.set(name, v)
  }
  return v
}

/** Fires cb whenever the data-theme attribute flips. Returns an unsubscribe.
 *  Canvas surfaces that cache rendered pixels (the board's static blit, the
 *  report dot map) use this to invalidate on toggle; React surfaces re-render
 *  through App state and do not need it. */
export function onThemeChange(cb: () => void): () => void {
  const mo = new MutationObserver(() => cb())
  mo.observe(document.documentElement, { attributes: true, attributeFilter: ['data-theme'] })
  return () => mo.disconnect()
}
