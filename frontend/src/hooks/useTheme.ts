import { useCallback, useEffect, useState } from 'react'

// Theme preference: dark (the default identity) or light, persisted per
// browser. The tokens live in index.css under :root / [data-theme='light'];
// this hook only flips the attribute and remembers the choice.

export type Theme = 'dark' | 'light'

const STORAGE_KEY = 'hauksbee.theme'

function initialTheme(): Theme {
  try {
    const saved = localStorage.getItem(STORAGE_KEY)
    if (saved === 'light' || saved === 'dark') return saved
  } catch { /* storage blocked: fall through to the default */ }
  return 'dark'
}

export function useTheme(): { theme: Theme; toggleTheme: () => void } {
  const [theme, setTheme] = useState<Theme>(initialTheme)

  useEffect(() => {
    document.documentElement.dataset.theme = theme
    try { localStorage.setItem(STORAGE_KEY, theme) } catch { /* non-fatal */ }
  }, [theme])

  const toggleTheme = useCallback(() => {
    setTheme(t => (t === 'dark' ? 'light' : 'dark'))
  }, [])

  return { theme, toggleTheme }
}
