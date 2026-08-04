import { useCallback, useEffect, useRef, useState } from 'react'
import { AnimatePresence, motion, useReducedMotion } from 'motion/react'
import type { WebReport } from '../types/report'
import type { SpecSnapshot } from '../hooks/useSessions'
import { DownloadIcon } from './Icons'
import { ARRIVE, LEAVE } from '../motion'
import { buildReportHtml, downloadText, reportJson } from '../lib/report-export'
import { specStemFor, workflowYaml } from '../lib/ci-workflow'
import { APP_VERSION } from '../lib/version'

// One Export menu, under the report's verdict, holding every file this report
// can become.
//
// It is a menu rather than a row of buttons because the four files are one
// decision ("I want to take this away") with four answers, and each item has to
// say what the file IS, not just its extension: "a web page you can send
// someone" and "the JSON the API returned" are different enough that a reader
// should not have to open one to find out which they wanted.
//
// It sits on the report rather than in the app header for a measured reason: a
// third glyph button up there left the board name 16px of ellipsis on a 320px
// phone, which is the exact defect the header's existing comments are about. On
// the report it is beside the thing it exports and keeps its own word. The
// Checks pane keeps its Download buttons where the spec is being composed.

export function ExportMenu({
  report, boardLabel, firmwareName, analyzedAt, engineVersion, spec, checks,
  sessionName, restored,
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
}) {
  const [open, setOpen] = useState(false)
  const [done, setDone] = useState<string | null>(null)
  const wrap = useRef<HTMLDivElement>(null)
  const reduced = useReducedMotion()

  // Close on an outside click or Escape. A menu that only closes by re-clicking
  // its own trigger is a menu that sits over the report while you try to read
  // the thing you just exported.
  useEffect(() => {
    if (!open) return
    const onDown = (e: MouseEvent) => {
      if (!wrap.current?.contains(e.target as Node)) setOpen(false)
    }
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setOpen(false)
    }
    document.addEventListener('mousedown', onDown)
    document.addEventListener('keydown', onKey)
    return () => {
      document.removeEventListener('mousedown', onDown)
      document.removeEventListener('keydown', onKey)
    }
  }, [open])

  const stem = specStemFor(report.file_name)

  const emit = useCallback((label: string, fileName: string, body: string, mime: string) => {
    downloadText(fileName, body, mime)
    setOpen(false)
    // The browser's own download shelf is the real confirmation, but it is
    // browser chrome the app cannot see, and on a headless or kiosk profile
    // there is none. The line under the trigger says what was written.
    setDone(`${label} written as ${fileName}`)
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
    run: () => void
  }[] = [
    {
      id: 'export-html',
      label: 'Report as a web page',
      what: `${stem}-report.html; styled, standalone, opens with no server`,
      run: () => emit('The report', `${stem}-report.html`, buildReportHtml({
        report, boardLabel, firmwareName, analyzedAt, engineVersion,
        appVersion: APP_VERSION, spec, checks, sessionName, restored,
      }), 'text/html;charset=utf-8'),
    },
    {
      id: 'export-json',
      label: 'Findings as JSON',
      what: `${stem}-report.json; exactly what /api/analyze returned`,
      run: () => emit('The findings', `${stem}-report.json`, reportJson(report), 'application/json'),
    },
  ]
  if (spec) {
    items.push({
      id: 'export-toml',
      label: 'The checks spec',
      what: `${spec.fileName}; the file hauksbee-ci runs`,
      run: () => emit('The spec', spec.fileName, spec.toml, 'text/plain;charset=utf-8'),
    })
    items.push({
      id: 'export-workflow',
      label: 'The CI workflow',
      what: 'hauksbee-ci.yml; runs that spec on every push',
      run: () => emit('The workflow', 'hauksbee-ci.yml', workflowYaml(stem), 'text/yaml;charset=utf-8'),
    })
  }

  return (
    // `relative` and full-width: the panel is left-aligned under the trigger and
    // capped at the report column's width, so on a phone it narrows with the
    // column instead of reaching past it.
    <div ref={wrap} className="relative" data-testid="export">
      <button
        type="button"
        data-testid="export-open"
        onClick={() => setOpen(o => !o)}
        aria-haspopup="menu"
        aria-expanded={open}
        {/* No `title`: its words are on screen, so a tooltip repeating them only
            adds a description identical to the name and the button announces
            itself twice. */}
        aria-label="Export this report"
        className="hb-btn hb-press inline-flex items-center justify-center gap-2 px-3 text-[12px] whitespace-nowrap"
        style={{ height: 30 }}
      >
        <DownloadIcon size={13} />
        Export this report
      </button>

      <AnimatePresence>
        {open && (
          <motion.div
            data-testid="export-menu"
            role="menu"
            initial={reduced ? { opacity: 1 } : { opacity: 0, y: -4 }}
            animate={{ opacity: 1, y: 0 }}
            exit={reduced ? { opacity: 0 } : { opacity: 0, y: -4, transition: LEAVE }}
            transition={reduced ? { duration: 0 } : ARRIVE}
            className="hb-card absolute z-30 overflow-hidden"
            // Hung under the trigger, capped at the report column's width.
            style={{
              top: 'calc(100% + 6px)', left: 0, width: 300, maxWidth: '100%',
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
                className="hb-press block w-full text-left px-3 py-2 cursor-pointer"
                style={{ background: 'none', border: 'none' }}
                onMouseEnter={e => { (e.currentTarget as HTMLElement).style.background = 'var(--copper-tint)' }}
                onMouseLeave={e => { (e.currentTarget as HTMLElement).style.background = 'none' }}
              >
                <div className="text-[13px]" style={{ color: 'var(--silk)' }}>{item.label}</div>
                {/* The file name and what it is for. `anywhere` because a board
                    called `rev_c_panelised_v2.kicad_pcb` makes a long stem and
                    there is no space in it to break at. */}
                <div className="text-[11px]" style={{ color: 'var(--silk-faint)', overflowWrap: 'anywhere' }}>
                  {item.what}
                </div>
              </button>
            ))}
            {!spec && (
              <div
                className="px-3 py-2 text-[11px] leading-relaxed"
                style={{ borderTop: '1px solid var(--hairline)', color: 'var(--silk-faint)' }}
              >
                Compose checks on the Checks view and the spec and its CI workflow appear here too.
              </div>
            )}
          </motion.div>
        )}
      </AnimatePresence>

      {done && (
        <div
          data-testid="export-done"
          role="status"
          aria-live="polite"
          className="absolute z-20 px-2.5 py-1.5 rounded-lg text-[11px]"
          style={{
            top: 'calc(100% + 6px)', left: 0, width: 'max-content', maxWidth: '100%',
            background: 'var(--ok-bg)', border: '1px solid var(--ok-border)', color: 'var(--ok)',
          }}
        >
          {done}
        </div>
      )}
    </div>
  )
}
