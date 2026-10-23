// The parts and nets a board result names, as live links to the board: hover
// lights the thing up on the canvas, click selects it (the same selection a
// click on the canvas makes, so its card opens) and frames it.
//
// The handlers come through a context so the verdict rows (VerdictConditions,
// PartlyJudged, the finding rows) stay plain components: rendered without a
// provider (the unit tests, the export) a chip is still a labelled button that
// simply has nowhere to go.

import { createContext, useContext, useState } from 'react'
import { explainFor, labelFor } from '../../lib/labels'
import { linkify } from '../../lib/result-links'
import { RAW_ID, isLabelledId } from '../../lib/plain-ids'
import type { ResultEntity, Vocabulary } from '../../lib/result-links'

export interface ResultLinkHandlers {
  vocab: Vocabulary | null
  /** Hovered (or focused) link, null when the pointer leaves. */
  onPoint?: (entity: ResultEntity | null) => void
  /** Clicked link: select and frame it on the board. `scroll: false` lights it
   *  without moving the page, for a link whose own surface is what the reader is
   *  looking at (the bench strip's run view). */
  onGo?: (entity: ResultEntity, opts?: { scroll?: boolean }) => void
  /** Open the existing model flow (draft from a datasheet, or write one) for
   *  a part. Absent where there is no flow to open (a restored session). */
  onGiveModel?: (ref: string) => void
  /** Parts whose orderable variant the board leaves open, and the existing
   *  picker that settles one with `[[override]] select`. */
  variantRefs?: ReadonlySet<string>
  onPickVariant?: (ref: string) => void
  /** The check the existing add-check path can start for a part or a net, if
   *  any, and the call that opens its editor. */
  checkOffer?: (entity: ResultEntity) => CheckOffer | null
  onAddCheck?: (offer: CheckOffer) => void
  /** Frame and light up on the board everything a finding names (and the spot
   *  it was found at), without selecting any one of them. `key` names the
   *  finding so its card can say it is the one on the map. */
  onShow?: (entities: ResultEntity[], at: { x: number; y: number; label: string } | null, key: string) => void
  /** The finding the board is showing now, if any. */
  shownKey?: string | null
  /** Open the Checks view. */
  onOpenChecks?: () => void
}

/** A check the add-check path can start: the kind, its target, and a label. */
export interface CheckOffer {
  kind: string
  net?: string
  ref?: string
  label: string
}

export const ResultLinkContext = createContext<ResultLinkHandlers>({ vocab: null })

const nameOf = (e: ResultEntity) => (e.kind === 'part' ? e.ref : e.net)

/** Punctuation that closes a clause: a chip it follows sits flush against it. */
const CLOSING_RUN = /^[,.;:)\]!?]+/

/** A part or a net as a chip: a small glyph for which it is, then its name.
 *  `tightBefore` / `tightAfter` say the chip sits against an opening bracket or
 *  closing punctuation in the sentence, so its padding on that side shrinks and
 *  the text reads "(from D13, driven by U1)", not "(from D13 , driven by U1 )". */
export function EntityChip({ entity, tightBefore = false, tightAfter = false }: { entity: ResultEntity; tightBefore?: boolean; tightAfter?: boolean }) {
  const { onPoint, onGo } = useContext(ResultLinkContext)
  const name = nameOf(entity)
  const part = entity.kind === 'part'
  return (
    <button
      type="button"
      data-testid="result-link"
      data-kind={entity.kind}
      data-name={name}
      data-tight={tightBefore || tightAfter ? [tightBefore && 'before', tightAfter && 'after'].filter(Boolean).join(' ') : undefined}
      title={onGo ? `${part ? 'Part' : 'Net'} ${name}: show it on the board` : undefined}
      onMouseEnter={() => onPoint?.(entity)}
      onMouseLeave={() => onPoint?.(null)}
      onFocus={() => onPoint?.(entity)}
      onBlur={() => onPoint?.(null)}
      onClick={() => onGo?.(entity)}
      className="hb-press result-chip inline-flex items-center gap-1 align-baseline rounded-md"
      style={{
        paddingLeft: tightBefore ? 3 : 6,
        paddingRight: tightAfter ? 3 : 6,
        marginLeft: tightBefore ? 0 : 1,
        marginRight: tightAfter ? 0 : 1,
        fontFamily: 'var(--font-mono)',
        fontSize: '0.9em',
        lineHeight: 1.45,
        cursor: onGo ? 'pointer' : 'default',
        border: '1px solid var(--hairline)',
        background: 'var(--surface-2)',
        color: 'var(--silk)',
      }}
    >
      <svg width="9" height="9" viewBox="0 0 10 10" aria-hidden="true" style={{ flexShrink: 0, color: 'var(--silk-faint)' }}>
        {part
          ? <rect x="1.5" y="1.5" width="7" height="7" rx="1.5" fill="none" stroke="currentColor" strokeWidth="1.6" />
          : <path d="M0.5 5h9" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" />}
      </svg>
      {name}
    </button>
  )
}

/** A sentence with the parts and nets it names turned into chips. Words the
 *  report does not list as a part or a net stay text. */
export function Linked({ text }: { text: string }) {
  const { vocab } = useContext(ResultLinkContext)
  if (!vocab) return <Plain text={text} />
  const segments = linkify(text, vocab)
  return (
    <>
      {segments.map((seg, i) => {
        const before = segments[i - 1]
        const after = segments[i + 1]
        const closing = typeof after === 'string' ? CLOSING_RUN.exec(after)?.[0] : undefined
        if (typeof seg === 'string') {
          // The closing mark after a chip was drawn with the chip (below).
          const drawn = typeof before === 'object' ? CLOSING_RUN.exec(seg)?.[0] : undefined
          return <Plain key={i} text={drawn ? seg.slice(drawn.length) : seg} />
        }
        const chip = (
          <EntityChip
            key={i}
            entity={seg}
            tightBefore={typeof before === 'string' && before.endsWith('(')}
            tightAfter={closing !== undefined}
          />
        )
        // A chip and the closing mark right after it are one unbreakable unit,
        // so a line never starts with a stray comma.
        return closing
          ? <span key={i} data-testid="chip-with-punct" style={{ whiteSpace: 'nowrap' }}>{chip}{closing}</span>
          : chip
      })}
    </>
  )
}

/** Text with any engine id that has words of its own shown as its label from
 *  the shared table (labels.ts), the id itself kept in the tooltip. The rule
 *  for which ids qualify (`isLabelledId`) is the one the report exports use. */
function Plain({ text }: { text: string }) {
  const out: React.ReactNode[] = []
  let last = 0
  for (const m of text.matchAll(RAW_ID)) {
    const at = m.index ?? 0
    if (!isLabelledId(m[0])) continue
    if (at > last) out.push(text.slice(last, at))
    out.push(
      <span key={at} data-testid="raw-id" title={explainFor(m[0]) ?? m[0]} style={{ textDecoration: 'underline dotted', textUnderlineOffset: 3 }}>
        {labelFor(m[0])}
      </span>,
    )
    last = at + m[0].length
  }
  if (last === 0) return <span>{text}</span>
  if (last < text.length) out.push(text.slice(last))
  return <span>{out}</span>
}

const SHOWN = 6

/** A list of chips, the first few shown and the rest one click away; the whole
 *  list is in the "more" button's tooltip. */
export function ChipList({ entities, testId, joined = false }: { entities: ResultEntity[]; testId?: string; joined?: boolean }) {
  const [all, setAll] = useState(false)
  const more = entities.length - SHOWN
  const shown = all ? entities : entities.slice(0, SHOWN)
  const hidden = more > 0 && !all
  // In a sentence the chips read as a list: "Q1, U4 and U5", or "Q1, U4 and 3 more".
  const between = (i: number) => {
    if (!joined || i === 0) return ''
    return i === shown.length - 1 && !hidden ? ' and ' : ', '
  }
  return (
    <span data-testid={testId}>
      {shown.map((e, i) => (
        <span key={`${e.kind}:${nameOf(e)}`}>{between(i)}<EntityChip entity={e} /></span>
      ))}      {more > 0 && !all && (
        <button
          type="button"
          data-testid="result-links-more"
          onClick={() => setAll(true)}
          title={entities.map(nameOf).join(', ')}
          className="hb-press text-[12px] ml-1 underline"
          style={{ color: 'var(--copper-hi)', background: 'none', border: 'none', padding: 0, cursor: 'pointer' }}
        >
          and {more} more
        </button>
      )}
    </span>
  )
}

/** A small action on a result row: what the reader can do about that row. */
export function RowAction({ testId, onClick, children, primary = false, title }: {
  testId: string
  onClick: () => void
  children: React.ReactNode
  primary?: boolean
  title?: string
}) {
  return (
    <button
      type="button"
      data-testid={testId}
      onClick={onClick}
      title={title}
      className={`${primary ? 'hb-chip' : 'hb-btn'} hb-press inline-flex items-center gap-1 px-2 text-[12px] whitespace-nowrap`}
      style={{ height: 26 }}
    >
      {children}
    </button>
  )
}

/** The "show on board" action every card shares. */
export function ShowOnBoard({ entities, at, id, label }: {
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

/** "Add as check" for the first part or net in `entities` the add-check path
 *  can start a check on; nothing when none can. */
export function AddCheck({ entities }: { entities: ResultEntity[] }) {
  const { checkOffer, onAddCheck } = useContext(ResultLinkContext)
  if (!checkOffer || !onAddCheck) return null
  const offer = entities.map(checkOffer).find(o => o !== null)
  if (!offer) return null
  return (
    <RowAction testId="result-add-check" onClick={() => onAddCheck(offer)} title={`Opens it in the board's check-this dock: ${offer.label}`}>
      Add a check
    </RowAction>
  )
}

export const partsOf = (refs: string[]): ResultEntity[] => refs.map(ref => ({ kind: 'part', ref }))
export const netsOf = (nets: string[]): ResultEntity[] => nets.map(net => ({ kind: 'net', net }))
