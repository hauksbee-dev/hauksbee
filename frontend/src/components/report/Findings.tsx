import { useContext } from 'react'
import type { FixFirst, WebHeadsUp } from '../../types/report'
import type { FindingEntry, FindingGroup, SectionStatus, SectionView } from '../../lib/report-view'
import { LEVEL_WORD, plural } from '../../lib/report-view'
import { entitiesIn } from '../../lib/result-links'
import type { ResultEntity } from '../../lib/result-links'
import { AddCheck, ChipList, Linked, partsOf, ResultLinkContext, RowAction } from './ResultLinks'

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

// ── The findings list beside the board ──────────────────────────────────────
//
// Every finding as a card that reaches the board: "Show on board" frames and
// lights up every part and net it names (and marks the spot it was found at,
// when it has one), the part it is about opens in the inspector or in the
// model flow, and a check can be started from it. One severity scale
// (Serious / Warning / Note); categories with nothing in them are one quiet
// line at the end, never a card each.

const ENTRY_INK: Record<string, string> = { serious: 'var(--err)', warning: 'var(--warn)', note: 'var(--note-accent)' }
const ENTRY_TAG_INK: Record<string, string> = { serious: 'var(--err-strong)', warning: 'var(--warn-strong)', note: 'var(--note)' }

function LevelTag({ level }: { level: FindingEntry['level'] }) {
  return (
    <span
      data-testid="finding-level"
      className="inline-block px-1.5 rounded text-[10px] font-bold uppercase tracking-wider"
      style={{ color: ENTRY_TAG_INK[level], border: `1px solid ${ENTRY_INK[level]}`, lineHeight: '16px' }}
    >
      {LEVEL_WORD[level]}
    </span>
  )
}

/** The "show on board" action every card shares. */
function ShowOnBoard({ entities, at, id, label }: {
  entities: ResultEntity[]
  at: { x: number; y: number; label: string } | null
  id: string
  label: string
}) {
  const { onShow, shownKey } = useContext(ResultLinkContext)
  if (!onShow || (entities.length === 0 && !at)) return null
  const on = shownKey === id
  return (
    <button
      type="button"
      data-testid="finding-show"
      aria-pressed={on}
      aria-label={`Show on board: ${label}`}
      onClick={() => onShow(entities, at, id)}
      className={`${on ? 'hb-chip' : 'hb-btn'} hb-press inline-flex items-center gap-1 px-2 text-[12px] whitespace-nowrap`}
      style={{ height: 26 }}
    >
      <svg width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.2" aria-hidden="true">
        <circle cx="12" cy="12" r="7" /><path d="M12 2v4M12 18v4M2 12h4M18 12h4" strokeLinecap="round" />
      </svg>
      {on ? 'On the board' : 'Show on board'}
    </button>
  )
}

/** The part a finding is about, as the action that opens it: the model flow
 *  for a part with no model, else the part itself in the inspector. */
function PartAction({ parts, openRefs }: { parts: string[]; openRefs: ReadonlySet<string> }) {
  const { onGo, onGiveModel } = useContext(ResultLinkContext)
  const unmodelled = parts.find(ref => openRefs.has(ref))
  if (unmodelled && onGiveModel) {
    return (
      <RowAction testId={`finding-give-model-${unmodelled}`} onClick={() => onGiveModel(unmodelled)}>
        Give {unmodelled} a model
      </RowAction>
    )
  }
  const first = parts[0]
  if (!first || !onGo) return null
  return (
    <RowAction testId="finding-open-part" onClick={() => onGo({ kind: 'part', ref: first })} title={`Open ${first} and its model in the inspector`}>
      Open {first}
    </RowAction>
  )
}

function FindingItem({ entry: e, openRefs }: { entry: FindingEntry; openRefs: ReadonlySet<string> }) {
  const { vocab, shownKey, onOpenChecks } = useContext(ResultLinkContext)
  const named = vocab ? entitiesIn(e.text, vocab) : []
  const parts = named.flatMap(n => (n.kind === 'part' ? [n.ref] : []))
  const located = e.items.find(it => it.x !== undefined && it.y !== undefined)
  const at = located ? { x: located.x!, y: located.y!, label: e.title } : null
  const n = e.items.length
  const on = shownKey === e.key
  const more = [e.why && 'Why it matters', n > 1 && `All ${n} places`].filter(Boolean).join(' · ')
  return (
    <li
      data-testid={n > 1 ? 'grouped-finding' : 'finding-card'}
      data-level={e.level}
      className="rounded-lg px-3 py-2.5"
      style={{
        border: `1px solid ${on ? 'var(--copper)' : 'var(--hairline)'}`,
        borderLeft: `3px solid ${ENTRY_INK[e.level]}`,
        background: 'var(--surface)',
        overflowWrap: 'anywhere',
      }}
    >
      <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
        <LevelTag level={e.level} />
        <span className="text-[11px]" style={{ color: 'var(--silk-faint)' }}>
          {e.section}{n > 1 ? ` · found in ${n} places` : ''}
        </span>
      </div>
      <div data-testid="finding-title" className="mt-1 text-[13px] font-semibold leading-snug" style={{ color: 'var(--silk)' }}>
        <Linked text={e.title} />
      </div>
      {e.detail && (
        <div className="mt-0.5 text-[12px] leading-relaxed" style={{ color: 'var(--silk-dim)' }}><Linked text={e.detail} /></div>
      )}
      {e.data && (
        <div data-testid="finding-data" className="mt-1 text-[11px]" style={{ color: 'var(--silk-dim)', fontFamily: 'var(--font-mono)' }}>
          {e.data}
        </div>
      )}
      {e.fix && (
        <div data-testid="finding-fix" className="mt-1 text-[12px] leading-relaxed" style={{ color: 'var(--silk)' }}>
          <b style={{ color: 'var(--silk-dim)', fontWeight: 600 }}>Fix: </b><Linked text={e.fix} />
        </div>
      )}
      {more && (
        <details className="mt-1 text-[12px]" style={{ color: 'var(--silk-dim)' }}>
          <summary className="cursor-pointer" style={{ color: 'var(--silk-faint)' }}>{more}</summary>
          {e.why && <div className="mt-1 leading-relaxed">{e.why}</div>}
          {n > 1 && (
            <ul className="mt-1.5 pl-4 leading-relaxed" style={{ listStyleType: 'disc', color: 'var(--silk)' }}>
              {e.items.map((it, i) => <li key={i} className="my-0.5"><Linked text={it.what} /></li>)}
            </ul>
          )}
        </details>
      )}
      <div className="mt-2 flex flex-wrap items-center gap-1.5">
        <ShowOnBoard entities={named} at={at} id={e.key} label={e.title} />
        <PartAction parts={parts} openRefs={openRefs} />
        {e.wantsCheck && onOpenChecks
          ? <RowAction testId="finding-open-checks" onClick={onOpenChecks}>Say what it should do</RowAction>
          : <AddCheck entities={named} />}
      </div>
    </li>
  )
}

/** Parts left out of the circuit: a warning card of its own, with the active
 *  parts on the board and the way to model or list them. */
export function OpenPartsCard({ count, active, onList }: { count: number; active: string[]; onList?: () => void }) {
  const { onGiveModel } = useContext(ResultLinkContext)
  return (
    <li
      data-testid="open-parts-banner"
      data-open-count={count}
      className="rounded-lg px-3 py-2.5"
      style={{ border: '1px solid var(--hairline)', borderLeft: `3px solid ${ENTRY_INK.warning}`, background: 'var(--surface)' }}
    >
      <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
        <LevelTag level="warning" />
        <span className="text-[11px]" style={{ color: 'var(--silk-faint)' }}>Models</span>
      </div>
      <div className="mt-1 text-[13px] font-semibold leading-snug" style={{ color: 'var(--silk)' }}>
        {plural(count, 'part')} with no usable model
      </div>
      {active.length > 0 && (
        <div className="mt-0.5 text-[12px] leading-relaxed" style={{ color: 'var(--silk-dim)' }}>
          <ChipList entities={partsOf(active)} />{' '}
          {active.length === 1 ? 'is an active part' : 'are active parts'}, so analog, AC and thermal results on their nets are not trustworthy.
        </div>
      )}
      <div className="mt-2 flex flex-wrap items-center gap-1.5">
        <ShowOnBoard entities={partsOf(active)} at={null} id="open-parts" label="the active parts with no model" />
        {onGiveModel && active.slice(0, 2).map(ref => (
          <RowAction key={ref} testId={`finding-give-model-${ref}`} onClick={() => onGiveModel(ref)}>Give {ref} a model</RowAction>
        ))}
        {onList && (
          <RowAction testId="show-guessed-parts" onClick={onList}>List all {count}</RowAction>
        )}
      </div>
    </li>
  )
}

/** Sections that could not judge everything because parts have no model, as
 *  one row naming those parts. Their model buttons are on the open-parts card
 *  or the Next step, so this row only says what is not vouched for. */
function InconclusiveRow({ sections }: { sections: SectionStatus['inconclusive'] }) {
  const { vocab } = useContext(ResultLinkContext)
  if (sections.length === 0) return null
  // The parts each section names in its "(Q1, U4)", in order, once.
  const parts = [...new Set(sections.flatMap(s =>
    vocab ? entitiesIn(s.verdict.split(/, so /)[0], vocab).flatMap(e => (e.kind === 'part' ? [e.ref] : [])) : []))]
  return (
    <div
      data-testid="sections-inconclusive"
      className="rounded-lg px-3 py-2.5 text-[12px] leading-relaxed"
      style={{ border: '1px solid var(--warn-border)', background: 'var(--warn-bg)', color: 'var(--silk)' }}
    >
      <b style={{ fontWeight: 600, color: 'var(--warn-strong)' }}>Not fully checked: </b>
      {sections.map(s => s.title).join(', ')}.
      {parts.length > 0 && (
        <> Without a model for <ChipList entities={partsOf(parts)} />, a clean result there is not a clean bill.</>
      )}
    </div>
  )
}

/** The sections that found nothing, as one quiet line. Each name carries the
 *  section's own words as its tooltip (a default-rules qualifier, say). */
function ClearLine({ sections }: { sections: SectionStatus['clear'] }) {
  if (sections.length === 0) return null
  return (
    <div data-testid="sections-clear" className="px-1 text-[12px] leading-relaxed" style={{ color: 'var(--silk-dim)' }}>
      Also checked, nothing found:{' '}
      {sections.map((s, i) => (
        <span key={s.title}>
          {i > 0 && ', '}
          <span title={s.verdict} style={{ textDecoration: 'underline dotted', textUnderlineOffset: 3 }}>{s.title}</span>
        </span>
      ))}
      .
    </div>
  )
}

export function FindingsList({ entries, status, openParts, openRefs, preLayout, onListParts, onLocate }: {
  entries: FindingEntry[]
  status: SectionStatus
  /** Parts left out of the circuit: how many, and the active ones among them. */
  openParts: { count: number; active: string[] } | null
  /** Parts with no model, for "Give X a model" rather than "Open X". */
  openRefs: ReadonlySet<string>
  /** An unrouted board's note on what waits for copper, with the held-back findings. */
  preLayout: { notice: string; copper: SectionView | null } | null
  onListParts?: () => void
  onLocate?: LocateFn
}) {
  const shown = entries.length + (openParts ? 1 : 0)
  // The cards below, counted by level, so the count is the list's own.
  // Every place counts, so a card "found in 10 places" adds ten, as the
  // engine's own "12 issues found" does.
  const count = (level: FindingEntry['level']) =>
    entries.filter(e => e.level === level).reduce((n, e) => n + e.items.length, 0) + (level === 'warning' && openParts ? 1 : 0)
  const tally = [
    count('serious') > 0 && `${count('serious')} serious`,
    count('warning') > 0 && plural(count('warning'), 'warning'),
    count('note') > 0 && plural(count('note'), 'note'),
  ].filter(Boolean).join(' · ')
  return (
    <section data-testid="findings" aria-label="Findings" className="flex flex-col gap-2">
      <h2 className="flex items-baseline justify-between gap-2 px-1 text-[11px] font-bold tracking-widest uppercase" style={{ color: 'var(--silk-faint)' }}>
        <span>Findings</span>
        <span className="tnum font-normal normal-case tracking-normal" data-testid="findings-count">
          {shown === 0 ? 'none' : tally}
        </span>
      </h2>
      {shown > 0 && (
        <ul className="list-none p-0 m-0 flex flex-col gap-2" data-testid="result-findings">
          {openParts && <OpenPartsCard count={openParts.count} active={openParts.active} onList={onListParts} />}
          {entries.map(e => <FindingItem key={e.key} entry={e} openRefs={openRefs} />)}
        </ul>
      )}
      <InconclusiveRow sections={status.inconclusive} />
      {preLayout && <PreLayoutStrip notice={preLayout.notice} copper={preLayout.copper} onLocate={onLocate} />}
      <ClearLine sections={status.clear} />
    </section>
  )
}
