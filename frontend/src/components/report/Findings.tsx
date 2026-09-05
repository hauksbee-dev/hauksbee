import type { WebFinding, WebHeadsUp, WebSection } from '../../types/report'
import { groupFindings } from '../../lib/findings'
import type { FindingGroup } from '../../lib/findings'

// A report section and the cards in it. One card shape serves the three kinds
// of note a section can carry (a single finding, a collapsed group of similar
// ones, a heads-up), because they differ only in what goes above the
// why/what-to-do pair.

/** Pan-the-map callback for findings that carry board coordinates. */
export type LocateFn = (x: number, y: number, label: string) => void

const LEVEL_ACCENT: Record<string, string> = {
  serious: 'var(--err)',
  warning: 'var(--warn)',
  note: 'var(--note-accent)',
}

const LEVEL_TEXT: Record<string, string> = {
  serious: 'var(--err-strong)',
  warning: 'var(--warn-strong)',
  note: 'var(--note)',
}

/** The card every finding-shaped note sits in: a left accent bar in the
 *  level's colour, a small-caps tag, and the body. */
function NoteCard({ level, tag, accent, tagColor, testId, onClick, children }: {
  level?: string
  tag: React.ReactNode
  accent?: string
  tagColor?: string
  testId?: string
  onClick?: () => void
  children: React.ReactNode
}) {
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
      <span
        className="text-[10px] font-bold tracking-widest uppercase"
        style={{ color: tagColor ?? LEVEL_TEXT[level ?? ''] ?? 'var(--note)' }}
      >
        {tag}
      </span>
      {children}
    </div>
  )
}

/** The why / what-to-do pair under a finding's headline. Each half renders
 *  only when the engine supplied it (self-contained notes carry just `what`). */
function Gloss({ why, fix, labelColor = 'var(--silk-dim)' }: {
  why?: string
  fix?: string
  labelColor?: string
}) {
  return (
    <>
      {why && (
        <div className="text-sm my-0.5">
          <b style={{ color: labelColor, fontWeight: 600 }}>Why it matters:</b> {why}
        </div>
      )}
      {fix && (
        <div className="text-sm my-0.5">
          <b style={{ color: labelColor, fontWeight: 600 }}>What to do:</b> {fix}
        </div>
      )}
    </>
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

export function FindingCard({ finding: f, onLocate }: { finding: WebFinding; onLocate?: LocateFn }) {
  const locatable = onLocate !== undefined && f.x !== undefined && f.y !== undefined
  return (
    <NoteCard
      testId="finding-card"
      level={f.level}
      tag={f.level}
      onClick={locatable ? () => onLocate!(f.x!, f.y!, f.what) : undefined}
    >
      <LocateButton what={f.what} x={f.x} y={f.y} onLocate={onLocate} stopPropagation />
      <div className="font-semibold text-sm mt-1 mb-1.5">{f.what}</div>
      <Gloss why={f.why} fix={f.fix} />
    </NoteCard>
  )
}

/** A collapsed group of same-shaped findings: the shared level and explanation
 *  are shown once, and the individual items live inside an expandable list so a
 *  long run (128 clearance warnings, say) never walls the page yet hides
 *  nothing. */
function GroupedFindingCard({ group: g, onLocate }: { group: FindingGroup; onLocate?: LocateFn }) {
  const n = g.items.length
  return (
    <NoteCard testId="grouped-finding" level={g.level} tag={`${g.level} · ${n} similar`}>
      <div className="font-semibold text-sm mt-1 mb-1.5">
        {n} similar findings, same cause, listed once below.
      </div>
      <details className="mb-1.5">
        <summary className="text-sm cursor-pointer" style={{ color: 'var(--silk-dim)' }}>
          Show all {n}
        </summary>
        <ul className="mt-1.5 pl-4 text-sm" style={{ color: 'var(--silk)', listStyleType: 'disc' }}>
          {g.items.map((it, i) => (
            <li key={i} className="my-0.5">
              {it.what}
              <LocateButton what={it.what} x={it.x} y={it.y} onLocate={onLocate} />
            </li>
          ))}
        </ul>
      </details>
      <Gloss why={g.why} fix={g.fix} />
    </NoteCard>
  )
}

function HeadsUpCard({ note: h }: { note: WebHeadsUp }) {
  return (
    <NoteCard accent="var(--copper)" tagColor="var(--copper)" tag="Heads up">
      <div className="text-sm mt-0.5" style={{ color: 'var(--silk)' }}>{h.what}</div>
      <Gloss why={h.why} fix={h.fix} labelColor="var(--copper-hi)" />
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

export function SectionBlock({ section: s, onLocate }: { section: WebSection; onLocate?: LocateFn }) {
  return (
    <section className="mt-7">
      <h2 className="text-[11px] font-bold tracking-widest uppercase mb-1" style={{ color: 'var(--silk-faint)' }}>
        {s.title}
      </h2>
      <div className="text-sm mb-2" style={{ color: 'var(--silk-dim)' }}>{s.verdict}</div>
      {groupFindings(s.findings).map((g, i) =>
        g.items.length === 1
          ? (
            <FindingCard
              key={i}
              finding={{ level: g.level, what: g.items[0].what, why: g.why, fix: g.fix, x: g.items[0].x, y: g.items[0].y }}
              onLocate={onLocate}
            />
          )
          : <GroupedFindingCard key={i} group={g} onLocate={onLocate} />,
      )}
      {(s.heads_up || []).map((h, i) => <HeadsUpCard key={i} note={h} />)}
    </section>
  )
}
