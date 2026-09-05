import { useCallback, useEffect, useRef, useState } from 'react'
import { CheckIcon } from './Icons'
import { ArriveOnce } from '../motion'

// The small pieces every surface in the app repeats: a copy button, a titled
// status box, and the log well that streaming endpoints write into.

type Tone = 'ok' | 'warn' | 'err' | 'note' | 'quiet'

/** Background / border / text for a tone, as the theme tokens define it. */
function toneStyle(tone: Tone): React.CSSProperties {
  switch (tone) {
    case 'ok': return { background: 'var(--ok-bg)', border: '1px solid var(--ok-border)', color: 'var(--ok)' }
    case 'warn': return { background: 'var(--warn-bg)', border: '1px solid var(--warn-border)', color: 'var(--silk)' }
    case 'err': return { background: 'var(--err-bg)', border: '1px solid var(--err-border)', color: 'var(--err-strong)' }
    case 'note': return { background: 'var(--surface)', border: '1px solid var(--hairline)', color: 'var(--silk)' }
    case 'quiet': return { background: 'var(--surface-2)', border: '1px solid var(--hairline)', color: 'var(--silk-dim)' }
  }
}

/** The colour a tone's small-caps title takes. */
function toneTitleColor(tone: Tone): string {
  switch (tone) {
    case 'ok': return 'var(--ok)'
    case 'warn': return 'var(--warn-strong)'
    case 'err': return 'var(--err)'
    case 'note': return 'var(--note)'
    case 'quiet': return 'var(--silk-faint)'
  }
}

/**
 * A titled status box: small-caps label over a body, in one of the status
 * tones. `accent` gives it the note/heads-up left bar instead of a tinted fill.
 */
export function Callout({ tone, title, testId, className = '', accent = false, live, style, children }: {
  tone: Tone
  /** Small-caps line above the body. Omitted, the body stands alone. */
  title?: React.ReactNode
  testId?: string
  className?: string
  /** Quiet card with a coloured left bar (a note), not a tinted panel. */
  accent?: boolean
  /** aria-live region, for a box that appears in response to an action. */
  live?: boolean
  style?: React.CSSProperties
  children: React.ReactNode
}) {
  const base = accent
    ? {
        border: '1px solid var(--hairline)',
        borderLeft: `4px solid ${tone === 'note' ? 'var(--note-accent)' : toneTitleColor(tone)}`,
        background: 'var(--surface)',
        color: 'var(--silk)',
      }
    : toneStyle(tone)
  return (
    <div
      data-testid={testId}
      aria-live={live ? 'polite' : undefined}
      className={`rounded-lg px-4 py-3 text-[13px] leading-relaxed ${className}`}
      style={{ ...base, ...style }}
    >
      {title !== undefined && (
        <span
          className="text-[10px] font-bold tracking-widest uppercase block mb-1"
          style={{ color: toneTitleColor(tone) }}
        >
          {title}
        </span>
      )}
      {children}
    </div>
  )
}

/**
 * Copy to the clipboard, saying so for a moment afterwards.
 *
 * The textarea fallback is for insecure contexts and older browsers, where
 * `navigator.clipboard` is simply absent.
 */
export function CopyButton({ text, label = 'Copy', testId, small = false }: {
  text: string
  label?: string
  testId?: string
  /** The tighter form used inline beside a terminal command. */
  small?: boolean
}) {
  const [copied, setCopied] = useState(false)
  const copy = useCallback(async () => {
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
    setCopied(true)
    setTimeout(() => setCopied(false), 1500)
  }, [text])
  const size = small ? 'px-1.5 py-0.5 text-[10px]' : 'px-2 py-0.5 text-[11px]'
  return (
    <button
      type="button"
      data-testid={testId}
      onClick={() => void copy()}
      className={`hb-press ml-2 rounded font-semibold cursor-pointer ${size}`}
      style={{
        background: copied ? 'var(--ok-bg)' : 'var(--copper-tint)',
        border: `1px solid ${copied ? 'var(--ok-border)' : 'var(--copper-deep)'}`,
        color: copied ? 'var(--ok)' : 'var(--copper-hi)',
        whiteSpace: 'nowrap',
      }}
    >
      {copied
        ? <span className="inline-flex items-center gap-1"><CheckIcon size={small ? 10 : 11} /> Copied</span>
        : label}
    </button>
  )
}

/** A terminal command with its copy button. */
export function TerminalCommand({ command, testId }: { command: string; testId?: string }) {
  return (
    <span className="inline-flex items-center flex-wrap">
      <span className="mr-1">Terminal:</span>
      <code
        className="px-1.5 py-0.5 rounded"
        style={{ background: 'var(--code-bg)', color: 'var(--silk-dim)', border: '1px solid var(--hairline)' }}
      >
        {command}
      </code>
      <CopyButton text={command} testId={testId} small />
    </span>
  )
}

/**
 * The scrollback a streaming endpoint writes into, pinned to its newest line.
 * Renders nothing while the log is empty, so a caller can hand it every log it
 * has without guarding first.
 */
export function LogWell({ lines, testId, maxHeight = 180 }: {
  lines: readonly string[]
  testId?: string
  maxHeight?: number
}) {
  const ref = useRef<HTMLPreElement>(null)
  useEffect(() => {
    const el = ref.current
    if (el) el.scrollTop = el.scrollHeight
  }, [lines])
  if (lines.length === 0) return null
  return (
    <pre
      ref={ref}
      data-testid={testId}
      className="rounded-lg px-3 py-2 text-[11px] overflow-x-auto overflow-y-auto whitespace-pre-wrap"
      style={{
        maxHeight,
        background: 'var(--instrument)',
        border: '1px solid var(--hairline)',
        color: 'var(--silk-dim)',
        fontFamily: 'var(--font-mono)',
      }}
    >
      {lines.join('\n')}
    </pre>
  )
}

/**
 * The two things an upload can say back: something the app did on the user's
 * behalf (a firmware project zip routed to the other slot — not an error, the
 * drop worked, it just did not do the literal thing), and a refusal.
 *
 * Both surfaces that take a board render them identically, so they live here.
 */
export function UploadBanners({ notice, error, onDismiss, className = 'mt-6' }: {
  notice: string | null
  error: string | null
  onDismiss: () => void
  className?: string
}) {
  return (
    <>
      {notice && (
        <ArriveOnce
          className={`${className} rounded-lg px-4 py-3 text-[13px] leading-relaxed`}
          style={{ background: 'var(--warn-bg)', border: '1px solid var(--warn-border)', color: 'var(--silk)' }}
        >
          <div data-testid="upload-notice" aria-live="polite">
            {notice}
            <button
              type="button"
              onClick={onDismiss}
              className="hb-press ml-2 text-[12px] cursor-pointer"
              style={{ background: 'none', border: 'none', color: 'var(--copper)' }}
            >
              Got it
            </button>
          </div>
        </ArriveOnce>
      )}
      {error && (
        <ArriveOnce
          className={`${className} rounded-lg px-4 py-3 text-sm text-center`}
          style={{ background: 'var(--err-bg)', border: '1px solid var(--err-border)', color: 'var(--err-strong)' }}
        >
          <div data-testid="upload-error" aria-live="polite">{error}</div>
        </ArriveOnce>
      )}
    </>
  )
}

/** The app's switch, as a picture: a pill with a knob, copper when on. Purely
 *  presentational, so a row that is itself the control (a whole clickable line)
 *  can show one without nesting a second button inside itself. */
export function SwitchTrack({ on, size = 36 }: { on: boolean; size?: 26 | 36 }) {
  const big = size === 36
  const [h, knob, inset] = big ? [20, 14, 2] : [15, 10, 1.5]
  return (
    <span
      aria-hidden
      className="relative rounded-full shrink-0"
      style={{
        display: 'inline-block',
        width: size, height: h,
        background: on ? 'var(--copper-tint-strong)' : 'var(--surface-3)',
        border: `1px solid ${on ? 'var(--copper-deep)' : 'var(--hairline)'}`,
        transition: 'background-color 0.15s, border-color 0.15s',
      }}
    >
      <span
        className="absolute rounded-full"
        style={{
          top: inset, width: knob, height: knob,
          background: on ? 'var(--copper)' : 'var(--silk-faint)',
          left: on ? size - knob - inset - 2 : inset,
          transition: 'left 0.15s cubic-bezier(0.2,0,0,1), background-color 0.15s',
        }}
      />
    </span>
  )
}

/** The switch as its own control. */
export function Switch({ on, onToggle, label, size = 36 }: {
  on: boolean
  onToggle: () => void
  label: string
  size?: 26 | 36
}) {
  return (
    <button
      type="button"
      onClick={onToggle}
      role="switch"
      aria-checked={on}
      aria-label={label}
      className="hb-press cursor-pointer"
      style={{ background: 'none', border: 'none', padding: 0, display: 'inline-flex' }}
    >
      <SwitchTrack on={on} size={size} />
    </button>
  )
}

/** A spinner with a line of text beside it, announced to a screen reader. */
export function BusyLine({ children, className = '', color = 'var(--copper-hi)' }: {
  children: React.ReactNode
  className?: string
  color?: string
}) {
  return (
    <div
      className={`text-[12px] flex items-center gap-2 ${className}`}
      role="status"
      aria-live="polite"
      style={{ color }}
    >
      <span className="slot-spin" /> {children}
    </div>
  )
}
