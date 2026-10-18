import { useEffect, useRef, useState } from 'react'
import { DownloadIcon } from '../Icons'

// Everything that takes the checks off this page, in ONE menu beside Run: the
// spec file, the terminal command, the GitHub CI setup, and, once a run has
// landed, the run itself in the CLI's own files (served byte for byte) and as
// Markdown. Nothing here competes with Run for the eye.

/** The three files `hauksbee-ci run --json --junit --emit-manifest` wrote for
 *  this run, served byte for byte by the export route. */
const RUN_FILES: [file: string, label: string, what: string][] = [
  ['report.json', 'Run as JSON', 'what hauksbee-ci run --json prints'],
  ['results.xml', 'JUnit XML', 'for a CI test report'],
  ['manifest.json', 'Run manifest', 'inputs and versions, to reproduce it'],
]

const itemStyle: React.CSSProperties = {
  color: 'var(--silk)', textDecoration: 'none', background: 'none', border: 'none',
  width: '100%', textAlign: 'left', cursor: 'pointer', font: 'inherit',
}

function Item({ testId, label, what, onClick, href, download }: {
  testId: string
  label: string
  what: string
  onClick?: () => void
  href?: string
  download?: string
}) {
  const body = <>
    <div className="text-[12px] font-semibold">{label}</div>
    <div className="text-[11px]" style={{ color: 'var(--silk-faint)', overflowWrap: 'anywhere' }}>{what}</div>
  </>
  return href ? (
    <a role="menuitem" data-testid={testId} href={href} download={download} onClick={onClick} className="block px-3 py-1.5 hb-press" style={itemStyle}>
      {body}
    </a>
  ) : (
    <button type="button" role="menuitem" data-testid={testId} onClick={onClick} className="block px-3 py-1.5 hb-press" style={itemStyle}>
      {body}
    </button>
  )
}

export function ChecksExport({ specFileName, onDownloadSpec, cliCommand, onSetUpCi, ciOpen, exportId, onMarkdown }: {
  specFileName: string
  onDownloadSpec: () => void
  /** The terminal command that runs this spec with no time limit. */
  cliCommand: string
  onSetUpCi: () => void
  ciOpen: boolean
  /** The landed run's export id; its files are offered only once there is one. */
  exportId?: string | null
  /** Download the run as Markdown; absent before a run. */
  onMarkdown?: () => void
}) {
  const [open, setOpen] = useState(false)
  const [copied, setCopied] = useState(false)
  const wrap = useRef<HTMLDivElement>(null)
  useEffect(() => {
    if (!open) return
    const away = (e: MouseEvent) => { if (!wrap.current?.contains(e.target as Node)) setOpen(false) }
    const esc = (e: KeyboardEvent) => { if (e.key === 'Escape') setOpen(false) }
    document.addEventListener('mousedown', away)
    document.addEventListener('keydown', esc)
    return () => { document.removeEventListener('mousedown', away); document.removeEventListener('keydown', esc) }
  }, [open])
  const done = () => setOpen(false)
  const copy = () => {
    void navigator.clipboard?.writeText(cliCommand).then(() => {
      setCopied(true)
      window.setTimeout(() => setCopied(false), 1600)
    }).catch(() => {})
    done()
  }
  return (
    <div ref={wrap} className="relative" data-testid="run-export">
      <button
        type="button"
        data-testid="checks-export"
        aria-haspopup="menu"
        aria-expanded={open}
        onClick={() => setOpen(o => !o)}
        className="hb-btn hb-press inline-flex items-center gap-1.5 px-3 py-1.5 text-[13px] whitespace-nowrap"
      >
        <DownloadIcon size={13} /> {copied ? 'Copied' : 'Export'}
      </button>
      {open && (
        <div
          role="menu"
          className="absolute right-0 z-30 mt-1 rounded-lg py-1"
          style={{ width: 'min(290px, 86vw)', background: 'var(--surface)', border: '1px solid var(--hairline)', boxShadow: 'var(--shadow-pop)' }}
        >
          <div className="px-3 pt-1 pb-0.5 text-[10px] font-bold tracking-widest uppercase" style={{ color: 'var(--silk-faint)' }}>This spec</div>
          <Item testId="export-spec" label={`Download ${specFileName}`} what="the spec file, with board paths for a ci/ folder" onClick={() => { onDownloadSpec(); done() }} />
          <Item testId="export-cli" label="Copy the terminal command" what={cliCommand} onClick={copy} />
          <Item testId="export-ci" label={ciOpen ? 'Hide GitHub CI setup' : 'Set up GitHub CI'} what="run these checks on every push" onClick={() => { onSetUpCi(); done() }} />
          {exportId && (
            <>
              <div className="mt-1 px-3 pt-1.5 pb-0.5 text-[10px] font-bold tracking-widest uppercase" style={{ color: 'var(--silk-faint)', borderTop: '1px solid var(--hairline)' }}>This run</div>
              {onMarkdown && <Item testId="run-export-markdown" label="Summary as Markdown" what="the verdict and every check, for a ticket or review" onClick={() => { onMarkdown(); done() }} />}
              {RUN_FILES.map(([file, label, what]) => (
                <Item
                  key={file}
                  testId={`run-export-${file}`}
                  label={label}
                  what={what}
                  href={`/api/ci/export/${encodeURIComponent(exportId)}/${file}`}
                  download={`hauksbee-checks-${file}`}
                  onClick={done}
                />
              ))}
            </>
          )}
        </div>
      )}
    </div>
  )
}
