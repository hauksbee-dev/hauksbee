import type { FixFirst, WebHeadsUp } from '../../types/report'
import type { FindingGroup, SectionView } from '../../lib/report-view'

// A report section and the cards in it. One card shape serves every kind of
// note a section can carry (a finding, a collapsed group of similar ones, a
// heads-up, a top-level note): they differ only in what goes above the
// why/what-to-do pair.

/** Pan-the-map callback for findings that carry board coordinates. */
export type LocateFn = (x: number, y: number, label: string) => void

const LEVEL_ACCENT: Record<string, string> = { serious: 'var(--err)', warning: 'var(--warn)', note: 'var(--note-accent)' }
const LEVEL_TEXT: Record<string, string> = { serious: 'var(--err-strong)', warning: 'var(--warn-strong)', note: 'var(--note)' }

/** The card every finding-shaped note sits in: a left accent bar in the
 *  level's colour, a small-caps tag, the body, then the why/what-to-do pair
 *  (each half only when the engine supplied it). */
function NoteCard({ level, tag, accent, tagColor, testId, onClick, why, fix, labelColor = 'var(--silk-dim)', children }: {
  level?: string
  tag: React.ReactNode
  accent?: string
  tagColor?: string
  testId?: string
  onClick?: () => void
  why?: string
  fix?: string
  labelColor?: string
  children: React.ReactNode
}) {
  const gloss = (label: string, text?: string) => text && (
    <div className="text-sm my-0.5"><b style={{ color: labelColor, fontWeight: 600 }}>{label}:</b> {text}</div>
  )
  return (
    <div
      data-testid={testId}
      onClick={onClick}
      className="rounded-lg px-4 py-3 mb-2"
      style={{
        border: '1px solid var(--hairline)',
        borderLeft: `4px solid ${accent ?? LEVEL_ACCENT[level ?? ''] ?? 'var(--note-accent)'}`,
        background: 'var(--surface)',
        cursor: onClick ? 'pointer' : undefined,
      }}
    >
      <span className="text-[10px] font-bold tracking-widest uppercase" style={{ color: tagColor ?? LEVEL_TEXT[level ?? ''] ?? 'var(--note)' }}>
        {tag}
      </span>
      {children}
      {gloss('Why it matters', why)}
      {gloss('What to do', fix)}
    </div>
  )
}

/** Named for the finding it locates: fifty buttons reading "show on board" is
 *  one name fifty times over in the accessibility tree. */
function LocateButton({ what, x, y, onLocate, stopPropagation = false }: {
  what: string
  x?: number
  y?: number
  onLocate?: LocateFn
  stopPropagation?: boolean
}) {
  if (!onLocate || x === undefined || y === undefined) return null
  return (
    <button
      type="button"
      data-testid="finding-locate"
      onClick={e => { if (stopPropagation) e.stopPropagation(); onLocate(x, y, what) }}
      aria-label={`Show on board: ${what}`}
      className="hb-press ml-2 cursor-pointer text-[11px]"
      style={{
        background: 'none', border: 'none', padding: 0, color: 'var(--copper-hi)',
        textDecoration: 'underline', textDecorationColor: 'var(--copper-deep)',
      }}
    >
      show on board
    </button>
  )
}

/** One group of findings: a single card when the group has one item, else a
 *  collapsed card whose items live inside an expandable list so a long run
 *  (128 clearance warnings, say) never walls the page yet hides nothing. */
export function FindingCard({ group: g, onLocate }: { group: FindingGroup; onLocate?: LocateFn }) {
  const n = g.items.length
  if (n === 1) {
    const [it] = g.items
    const locatable = onLocate !== undefined && it.x !== undefined && it.y !== undefined
    return (
      <NoteCard
        testId="finding-card"
        level={g.level}
        tag={g.level}
        why={g.why}
        fix={g.fix}
        onClick={locatable ? () => onLocate!(it.x!, it.y!, it.what) : undefined}
      >
        <LocateButton what={it.what} x={it.x} y={it.y} onLocate={onLocate} stopPropagation />
        <div className="font-semibold text-sm mt-1 mb-1.5">{it.what}</div>
      </NoteCard>
    )
  }
  return (
    <NoteCard testId="grouped-finding" level={g.level} tag={`${g.level} · ${n} similar`} why={g.why} fix={g.fix}>
      <div className="font-semibold text-sm mt-1 mb-1.5">{n} similar findings, same cause, listed once below.</div>
      <details className="mb-1.5">
        <summary className="text-sm cursor-pointer" style={{ color: 'var(--silk-dim)' }}>Show all {n}</summary>
        <ul className="mt-1.5 pl-4 text-sm" style={{ color: 'var(--silk)', listStyleType: 'disc' }}>
          {g.items.map((it, i) => (
            <li key={i} className="my-0.5">
              {it.what}
              <LocateButton what={it.what} x={it.x} y={it.y} onLocate={onLocate} />
            </li>
          ))}
        </ul>
      </details>
    </NoteCard>
  )
}

/** The few causes to deal with before anything else, in the engine's order (the
 *  CLI's FIX THESE FIRST block prints the same list). Each one carries its own
 *  remedy and, when it has a spot, the way to see it. */
export function FixFirstCard({ fixFirst, onLocate }: { fixFirst: FixFirst; onLocate?: LocateFn }) {
  return (
    <section
      data-testid="fix-first"
      className="mt-3 rounded-xl px-4 py-3"
      style={{ border: '1px solid var(--err-border)', borderLeft: '4px solid var(--err)', background: 'var(--surface)' }}
    >
      <h2 className="text-[11px] font-bold tracking-widest uppercase mb-1" style={{ color: 'var(--err-strong)' }}>
        Fix these first
      </h2>
      <ol className="pl-5 text-sm" style={{ listStyleType: 'decimal' }}>
        {fixFirst.items.map((it, i) => (
          <li key={i} data-testid="fix-first-item" className="mt-2">
            <span className="font-semibold">{it.what}</span>
            {it.similar > 0 && <span style={{ color: 'var(--silk-dim)' }}> (+{it.similar} more like this)</span>}
            <LocateButton what={it.what} x={it.x} y={it.y} onLocate={onLocate} />
            <div className="my-0.5"><b style={{ color: 'var(--silk-dim)', fontWeight: 600 }}>What to do:</b> {it.fix}</div>
          </li>
        ))}
      </ol>
      {fixFirst.more > 0 && (
        <div className="mt-2 text-[12px]" style={{ color: 'var(--silk-dim)' }}>
          …and {fixFirst.more} more serious {fixFirst.more === 1 ? 'cause' : 'causes'} in the findings below.
        </div>
      )}
    </section>
  )
}

/** A layout with placed pads and no copper: say what ran and what waits, and
 *  keep the copper findings (counted) one click away instead of leading. */
export function PreLayoutStrip({ notice, copper, onLocate }: { notice: string; copper: SectionView | null; onLocate?: LocateFn }) {
  const held = copper?.groups.reduce((n, g) => n + g.items.length, 0) ?? 0
  return (
    <div
      data-testid="prelayout-strip"
      className="mt-3 rounded-lg px-4 py-2.5 text-[13px] leading-relaxed"
      style={{ border: '1px solid var(--hairline)', borderLeft: '4px solid var(--note-accent)', background: 'var(--surface)', color: 'var(--silk-dim)' }}
    >
      {notice}
      {copper && held > 0 && (
        <details className="mt-1.5" data-testid="prelayout-copper">
          <summary className="cursor-pointer text-[12px]">Copper findings held back ({held})</summary>
          {copper.groups.map((g, i) => <FindingCard key={i} group={g} onLocate={onLocate} />)}
        </details>
      )}
    </div>
  )
}

function HeadsUpCard({ note: h }: { note: WebHeadsUp }) {
  return (
    <NoteCard accent="var(--copper)" tagColor="var(--copper)" tag="Heads up" why={h.why} fix={h.fix} labelColor="var(--copper-hi)">
      <div className="text-sm mt-0.5" style={{ color: 'var(--silk)' }}>{h.what}</div>
    </NoteCard>
  )
}

/** A quiet slate note: the engine's top-level remarks and the co-sim's
 *  "nothing went wrong" line. */
export function NoteBlock({ children, tag = 'Note' }: { children: React.ReactNode; tag?: string }) {
  return (
    <NoteCard accent="var(--note-accent)" tagColor="var(--note)" tag={tag}>
      <div className="text-sm mt-0.5" style={{ color: 'var(--silk)' }}>{children}</div>
    </NoteCard>
  )
}

/** A report section's heading: small caps, with the verdict line under it. */
export function SectionHeading({ title, verdict, color = 'var(--silk-faint)' }: { title: string; verdict?: string; color?: string }) {
  return (
    <>
      <h2 className="text-[11px] font-bold tracking-widest uppercase mb-1" style={{ color }}>{title}</h2>
      {verdict && <div className="text-sm mb-2" style={{ color: 'var(--silk-dim)' }}>{verdict}</div>}
    </>
  )
}

export function SectionBlock({ section: s, onLocate }: { section: SectionView; onLocate?: LocateFn }) {
  return (
    <section className="mt-7">
      <SectionHeading title={s.title} verdict={s.verdict} />
      {s.groups.map((g, i) => <FindingCard key={i} group={g} onLocate={onLocate} />)}
      {s.headsUp.map((h, i) => <HeadsUpCard key={i} note={h} />)}
    </section>
  )
}
