import { useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react'
import { createPortal } from 'react-dom'
import { AnimatePresence, motion, useReducedMotion } from 'motion/react'
import type { WebReport } from '../types/report'
import type { SpecSnapshot } from '../hooks/useSessions'
import { DownloadIcon } from './Icons'
import { ARRIVE, LEAVE } from '../motion'
import { buildReportHtml, downloadText, reportJson } from '../lib/report-export'
import { checkReportJson, reportMarkdown } from '../lib/result-export'
import { specStemFor, workflowExportAvailable, workflowYaml } from '../lib/ci-workflow'
import { APP_VERSION } from '../lib/version'

// One Export menu, on the report's verdict card, holding every file this
// report can become and the command that reproduces it.
//
// A menu rather than a row of buttons: the files are one decision ("I want to
// take this away") with several answers. Each label says what the file is in
// plain words; the only detail line is its file name.
//
// The panel is portalled to <body> and placed with fixed coordinates from the
// trigger's box. The verdict card it hangs from clips its own corners
// (overflow hidden), and a panel inside it was drawn under the next card, so
// clicking Export looked like it did nothing.

/** Wide enough for "Results (JSON, same as the CLI)" and a long file name. */
const MENU_WIDTH = 340
/** The window margin the panel keeps, matching the page gutter on a phone. */
const GUTTER = 16

async function copyText(text: string) {
  try {
    await navigator.clipboard.writeText(text)
  } catch {
    const ta = document.createElement('textarea')
    ta.value = text
    ta.style.position = 'fixed'
    ta.style.opacity = '0'
    document.body.appendChild(ta)
    ta.select()
    try { document.execCommand('copy') } catch { /* nothing more to try */ }
    document.body.removeChild(ta)
  }
}

export function ExportMenu({
  report, boardLabel, firmwareName, analyzedAt, engineVersion, spec, checks,
  sessionName, restored, command,
}: {
  report: WebReport
  boardLabel: string | null
  firmwareName: string | null
  analyzedAt: number | null
  engineVersion: string | null
  spec: SpecSnapshot | null
  checks: { passed: number; failed: number; invalid: number } | null
  sessionName: string | null
  restored: boolean
  /** The terminal command that reproduces this run ("Copy the CLI command"). */
  command?: string
}) {
  const [open, setOpen] = useState(false)
  const [done, setDone] = useState<string | null>(null)
  const [pos, setPos] = useState<{ top: number; left: number; width: number; maxHeight?: number } | null>(null)
  const wrap = useRef<HTMLDivElement>(null)
  const menu = useRef<HTMLDivElement>(null)
  const reduced = useReducedMotion()

  // Inside the window: under the trigger where it fits, else above it, else
  // pinned to the window's bottom gutter and scrolling. Left-aligned with the
  // trigger where it fits, else pulled back so its right edge keeps the gutter.
  const place = useCallback(() => {
    const box = wrap.current?.getBoundingClientRect()
    if (!box) return
    const vw = document.documentElement.clientWidth
    const vh = window.innerHeight
    const width = Math.min(MENU_WIDTH, vw - 2 * GUTTER)
    const left = Math.max(GUTTER, Math.min(box.left, vw - GUTTER - width))
    const height = menu.current?.scrollHeight ?? 0
    const below = box.bottom + 6
    const above = box.top - 6 - height
    if (!height || below + height <= vh - GUTTER) setPos({ top: below, left, width })
    else if (above >= GUTTER) setPos({ top: above, left, width })
    else {
      const top = Math.max(GUTTER, vh - GUTTER - height)
      setPos({ top, left, width, maxHeight: vh - GUTTER - top })
    }
  }, [])
  useLayoutEffect(() => {
    if (open) place()
    else setPos(null)
  }, [open, place])
  // Once the panel is drawn its height is known: place it again so it never
  // runs off the bottom of the window.
  const drawn = pos !== null
  useLayoutEffect(() => {
    if (open && drawn) place()
  }, [open, drawn, place])

  // Both ways out: a click outside the trigger AND the panel (the panel is no
  // longer inside the trigger's subtree), and Escape. A scroll or a resize
  // moves the trigger, so the panel follows it.
  useEffect(() => {
    if (!open) return
    const onDown = (e: MouseEvent) => {
      const t = e.target as Node
      if (!wrap.current?.contains(t) && !menu.current?.contains(t)) setOpen(false)
    }
    const onKey = (e: KeyboardEvent) => { if (e.key === 'Escape') setOpen(false) }
    document.addEventListener('mousedown', onDown)
    document.addEventListener('keydown', onKey)
    window.addEventListener('resize', place)
    window.addEventListener('scroll', place, true)
    return () => {
      document.removeEventListener('mousedown', onDown)
      document.removeEventListener('keydown', onKey)
      window.removeEventListener('resize', place)
      window.removeEventListener('scroll', place, true)
    }
  }, [open, place])

  const stem = specStemFor(report.file_name)

  const emit = useCallback((label: string, fileName: string, body: string, mime: string) => {
    downloadText(fileName, body, mime)
    setOpen(false)
    // The browser's own download shelf is the real confirmation, but it is
    // browser chrome the app cannot see, and on a headless or kiosk profile
    // there is none. A line under the trigger says what was written.
    setDone(`${label} saved as ${fileName}`)
  }, [])

  useEffect(() => {
    if (!done) return
    const t = setTimeout(() => setDone(null), 4000)
    return () => clearTimeout(t)
  }, [done])

  const items: {
    id: string
    label: string
    what: string
    mono?: boolean
    run: () => void
  }[] = [
    {
      id: 'export-html',
      label: 'Report (web page)',
      what: `${stem}-report.html · opens in any browser`,
      run: () => emit('The report', `${stem}-report.html`, buildReportHtml({
        report, boardLabel, firmwareName, analyzedAt, engineVersion,
        appVersion: APP_VERSION, spec, checks, sessionName, restored,
      }), 'text/html;charset=utf-8'),
    },
    {
      id: 'export-markdown',
      label: 'Report (Markdown)',
      what: `${stem}-result.md · for a pull request or an issue`,
      run: () => emit('The Markdown report', `${stem}-result.md`, reportMarkdown({
        report, boardLabel, firmwareName, engineVersion, analyzedAt,
      }), 'text/markdown;charset=utf-8'),
    },
    {
      id: 'export-check-json',
      label: 'Results (JSON, same as the CLI)',
      what: `${stem}-check.json`,
      run: () => emit('The results', `${stem}-check.json`, checkReportJson(report), 'application/json'),
    },
    {
      id: 'export-json',
      label: 'Full analysis (JSON)',
      what: `${stem}-report.json · everything on this page, as data`,
      run: () => emit('The full analysis', `${stem}-report.json`, reportJson(report), 'application/json'),
    },
  ]
  if (spec) {
    items.push({
      id: 'export-toml',
      label: 'Checks file (TOML)',
      what: spec.fileName,
      run: () => emit('The checks file', spec.fileName, spec.toml, 'text/plain;charset=utf-8'),
    })
    if (workflowExportAvailable) {
      items.push({
        id: 'export-workflow',
        label: 'GitHub CI workflow (YAML)',
        what: 'hauksbee-ci.yml · runs those checks on every push',
        run: () => emit('The workflow', 'hauksbee-ci.yml', workflowYaml(stem), 'text/yaml;charset=utf-8'),
      })
    }
  }
  if (command) {
    items.push({
      id: 'export-copy-cli',
      label: 'Copy the CLI command',
      what: command,
      mono: true,
      run: () => {
        setOpen(false)
        void copyText(command).then(() => setDone('CLI command copied'))
      },
    })
  }

  return (
    <div ref={wrap} className="relative inline-block max-w-full" data-testid="export">
      <button
        type="button"
        data-testid="export-open"
        onClick={() => setOpen(o => !o)}
        aria-haspopup="menu"
        aria-expanded={open}
        /* No `title`: its words are on screen, so a tooltip repeating them only
           adds a description identical to the name. */
        aria-label="Export this report"
        className="hb-btn hb-press inline-flex items-center justify-center gap-2 px-3 text-[12px] whitespace-nowrap"
        style={{ height: 30 }}
      >
        <DownloadIcon size={13} />
        Export
        <svg width="9" height="9" viewBox="0 0 10 10" aria-hidden="true">
          <path d="M2 3.5 5 6.5 8 3.5" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" />
        </svg>
      </button>

      {typeof document !== 'undefined' && createPortal(
        <AnimatePresence>
          {open && pos && (
            <motion.div
              ref={menu}
              data-testid="export-menu"
              role="menu"
              aria-label="Export this report"
              initial={reduced ? { opacity: 1 } : { opacity: 0, y: -4 }}
              animate={{ opacity: 1, y: 0 }}
              exit={reduced ? { opacity: 0 } : { opacity: 0, y: -4, transition: LEAVE }}
              transition={reduced ? { duration: 0 } : ARRIVE}
              className="hb-card overflow-hidden"
              style={{
                position: 'fixed', top: pos.top, left: pos.left, width: pos.width, zIndex: 70,
                maxHeight: pos.maxHeight, overflowY: pos.maxHeight ? 'auto' : undefined,
                boxShadow: 'var(--shadow-pop)',
              }}
            >
              {items.map(item => (
                <button
                  key={item.id}
                  type="button"
                  role="menuitem"
                  data-testid={item.id}
                  onClick={item.run}
                  className="hb-press block w-full text-left px-3.5 py-2 cursor-pointer"
                  style={{
                    background: 'none', border: 'none',
                    borderTop: item.mono ? '1px solid var(--hairline)' : 'none',
                  }}
                  onMouseEnter={e => { (e.currentTarget as HTMLElement).style.background = 'var(--copper-tint)' }}
                  onMouseLeave={e => { (e.currentTarget as HTMLElement).style.background = 'none' }}
                >
                  <div className="text-[13px]" style={{ color: 'var(--silk)' }}>{item.label}</div>
                  {/* `anywhere` because a board called
                      `rev_c_panelised_v2.kicad_pcb` makes a long stem with no
                      space in it to break at. */}
                  <div
                    className="text-[11px]"
                    style={{
                      color: 'var(--silk-faint)', overflowWrap: 'anywhere',
                      fontFamily: item.mono ? 'var(--font-mono)' : undefined,
                    }}
                  >
                    {item.what}
                  </div>
                </button>
              ))}
              {!spec && (
                <div
                  className="px-3.5 py-2 text-[11px] leading-relaxed"
                  style={{ borderTop: '1px solid var(--hairline)', color: 'var(--silk-faint)' }}
                >
                  Checks you set up on the Checks view can be exported here too.
                </div>
              )}
            </motion.div>
          )}
        </AnimatePresence>,
        document.body,
      )}

      {done && (
        <div
          data-testid="export-done"
          role="status"
          aria-live="polite"
          className="absolute left-0 z-20 px-2.5 py-1.5 rounded-lg text-[11px]"
          style={{
            top: 'calc(100% + 6px)', width: 'max-content', maxWidth: 'min(420px, 80vw)',
            background: 'var(--ok-bg)', border: '1px solid var(--ok-border)', color: 'var(--ok)',
            boxShadow: 'var(--shadow-pop)',
          }}
        >
          {done}
        </div>
      )}
    </div>
  )
}
