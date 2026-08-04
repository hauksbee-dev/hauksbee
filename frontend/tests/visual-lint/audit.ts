// The rules, as one function that runs inside the page.
//
// Everything here is measured from the real layout, never from the source: a
// class name says what was intended, `getBoundingClientRect` says what the
// user got. The function is serialized into the browser by Playwright, so it
// must be self-contained (no imports, no closure over module scope).

export type Finding = {
  /** Which rule fired. */
  rule: string
  /** `error` fails the lint; `info` is reported and does not. */
  severity: 'error' | 'info'
  /** A short DOM path, enough to find the element in the source. */
  selector: string
  /** The measured numbers behind the verdict. */
  detail: string
}

/** Tolerances, in CSS pixels. Sub-pixel layout rounding is not a defect. */
export const TOLERANCE = {
  /** Page-level horizontal scroll. */
  page: 1,
  /** A child's box crossing its parent's left/right edge. */
  escapeX: 2,
  /** A child's box crossing its parent's bottom edge. Looser: line-height and
   *  descenders routinely put a glyph box a pixel or two past a tight parent. */
  escapeY: 4,
  /** nowrap text whose content is wider than its box. */
  text: 1,
  /** A control's own label crossing its own border box. Looser than `text`
   *  because a centred single line in a tight fixed height sits a pixel or two
   *  past the box at some font sizes without being cut. */
  controlText: 3,
  /** An out-of-flow box crossing the window's edge. */
  viewport: 2,
  /** Rendered-vs-natural image aspect, as a fraction (0.02 = 2%). */
  aspect: 0.02,
}

export function auditPage(tol: typeof TOLERANCE): Finding[] {
  const out: Finding[] = []
  const style = (el: Element) => window.getComputedStyle(el)
  const add = (rule: string, severity: 'error' | 'info', el: Element | string, detail: string) => {
    out.push({ rule, severity, selector: typeof el === 'string' ? el : path(el), detail })
    // Tag the offending element so the runner can outline it and scroll it into
    // the screenshot. The app shell is `h-screen overflow-hidden` with its own
    // inner scrollers, so a full-page capture is just the viewport: without
    // this, a violation below the fold produced a screenshot of somewhere else.
    if (severity === 'error' && typeof el !== 'string') el.setAttribute('data-visual-lint', rule)
  }

  /** A short, readable DOM path: the last few ancestors, tagged with the
   *  identifying attribute each one carries. */
  function path(el: Element): string {
    const parts: string[] = []
    let node: Element | null = el
    for (let i = 0; node && i < 4 && node !== document.body; i++) {
      const testid = node.getAttribute('data-testid')
      const id = node.id
      let s = node.tagName.toLowerCase()
      if (testid) s += `[data-testid="${testid}"]`
      else if (id) s += `#${id}`
      else {
        const cls = (node.getAttribute('class') ?? '')
          .split(/\s+/).filter(Boolean).slice(0, 2).join('.')
        if (cls) s += `.${cls}`
      }
      parts.unshift(s)
      node = node.parentElement
    }
    return parts.join(' > ')
  }

  const rect = (el: Element) => el.getBoundingClientRect()
  const round = (n: number) => Math.round(n)

  /** Laid out AND painted. Both halves matter:
   *  - this app hides views rather than unmounting them, so a whole surface's
   *    worth of zero-size elements is always in the DOM;
   *  - the contents of a closed `<details>` keep their layout boxes (the UA
   *    hides them with content-visibility, not display:none), so a rect alone
   *    reports collapsed help text as visibly broken. `checkVisibility` is what
   *    knows the difference. */
  const shown = (el: Element) => {
    const r = rect(el)
    if (r.width <= 0 || r.height <= 0) return false
    if (!el.checkVisibility({ checkVisibilityCSS: true })) return false
    return style(el).display !== 'contents'
  }

  const vw = document.documentElement.clientWidth
  const vh = document.documentElement.clientHeight

  type Box = { left: number; right: number; top: number; bottom: number; width: number }

  /** The box a child is supposed to be inside.
   *
   *  For a block, that is its border box. For an INLINE element it is not: an
   *  inline box's bounding rect is the union of its line boxes, which on a
   *  wrapped inline spans the full column and starts at the first line's
   *  indent, so comparing a child against it measures nothing on either edge.
   *  The line boxes themselves are what the inline actually occupies, so the
   *  bound used here is the union of `getClientRects()` (per-line), which is the
   *  tightest true statement available about where an inline parent is. */
  function bounds(el: Element): Box {
    const s = style(el)
    const r = rect(el)
    if (s.display !== 'inline' && s.display !== 'ruby') {
      return { left: r.left, right: r.right, top: r.top, bottom: r.bottom, width: r.width }
    }
    const rs = Array.from(el.getClientRects()).filter(x => x.width > 0 || x.height > 0)
    if (rs.length === 0) {
      return { left: r.left, right: r.right, top: r.top, bottom: r.bottom, width: r.width }
    }
    const left = Math.min(...rs.map(x => x.left))
    const right = Math.max(...rs.map(x => x.right))
    const top = Math.min(...rs.map(x => x.top))
    const bottom = Math.max(...rs.map(x => x.bottom))
    return { left, right, top, bottom, width: right - left }
  }

  /** Does this box draw anything, so that spilling out of it is visible? A
   *  border, or a background that is not fully transparent. */
  const paints = (s: CSSStyleDeclaration) =>
    s.borderTopWidth !== '0px' || s.borderBottomWidth !== '0px'
    || s.borderLeftWidth !== '0px' || s.borderRightWidth !== '0px'
    || (s.backgroundColor !== 'rgba(0, 0, 0, 0)' && s.backgroundColor !== 'transparent')
    || s.backgroundImage !== 'none'

  const scrolls = (v: string) => v === 'auto' || v === 'scroll'
  const clips = (v: string) => v === 'hidden' || v === 'clip'

  // ── 1. No page-level horizontal scroll ────────────────────────────────────
  // A table or a code block may scroll inside its own overflow-x container.
  // The PAGE may not: that is the defect where the whole layout slides sideways
  // and the right-hand column is off screen with no way back.
  for (const el of [document.documentElement, document.body]) {
    if (el.scrollWidth > vw + tol.page) {
      add('page-h-overflow', 'error', el.tagName.toLowerCase(),
        `scrollWidth=${el.scrollWidth} viewport=${vw} excess=${el.scrollWidth - vw}px`)
    }
  }

  const all = Array.from(document.querySelectorAll<HTMLElement>('body *'))

  // ── 2. Nothing escapes the box that is supposed to hold it ────────────────
  //
  // Three things changed here after the first version of this file shipped with
  // them written down as known blind spots:
  //
  //  - an INLINE parent is no longer skipped. `bounds()` above measures its
  //    line boxes instead of its useless union rect.
  //  - the check no longer stops at the immediate parent. A button inside three
  //    bare layout divs inside a card was compared only against the innermost
  //    div, which has no edges of its own, so hanging 40px off the CARD passed.
  //    The walk carries on to the nearest ancestor that CLIPS (where the child
  //    is genuinely cut) or PAINTS (where the spill is visible), which is the
  //    ancestor a reader would call "the box it is in".
  //  - out-of-flow elements are no longer exempt. They are measured against the
  //    window, and an absolute one against its offset parent as well.
  const ESCAPE_CANDIDATES = 'button, a, img, svg, input, select, textarea, code, [role="button"]'
  for (const el of all) {
    if (!el.matches(ESCAPE_CANDIDATES)) continue
    if (!shown(el)) continue
    const parent = el.parentElement
    if (!parent || parent === document.body) continue
    const cs = style(el)
    const cr = rect(el)
    if (cs.position === 'fixed' || cs.position === 'sticky') {
      offWindow(el, cr, cs)
      continue
    }
    if (cs.position === 'absolute') {
      if (!offWindow(el, cr, cs)) offOffsetParent(el, cr)
      continue
    }
    escapeWalk(el, cr)
  }

  /** An out-of-flow box that has left the window. Nothing can scroll it back
   *  (that is what taking it out of flow means), so whatever is out there is
   *  unreachable rather than merely awkward. Returns true when it fired. */
  function offWindow(el: Element, cr: DOMRect, cs: CSSStyleDeclaration): boolean {
    // Parked off-screen on purpose: the screen-reader-only patterns, which are
    // either a 1px box or clipped to nothing.
    if (cr.width <= 4 || cr.height <= 4) return false
    if (cs.clipPath !== 'none' || (cs.clip !== 'auto' && cs.clip !== '')) return false
    const pastRight = cr.right - vw
    const pastLeft = -cr.left
    const worst = Math.max(pastRight, pastLeft)
    if (worst <= tol.viewport) return false
    add('offscreen', 'error', el,
      `${cs.position} ${round(cr.width)}x${round(cr.height)} at x=[${round(cr.left)},${round(cr.right)}]`
      + ` is ${round(worst)}px outside the ${vw}px window (${pastRight > pastLeft ? 'right' : 'left'});`
      + ' nothing can scroll it back into view')
    return true
  }

  /** An absolutely positioned box against its containing block.
   *
   *  Tuned, and here is the reasoning: an anchored overlay (a dropdown, a
   *  popover, a floating card) is routinely WIDER than the control it hangs off,
   *  and being wider than its offset parent is how it is supposed to work. What
   *  is never fine is being cut off, so this fires when the containing block
   *  CLIPS in the axis the child crosses. The other half of "not exempt any
   *  more" is `offWindow` above, which is what catches the anchored panel that
   *  went off the side of the phone. */
  function offOffsetParent(el: HTMLElement, cr: DOMRect) {
    const op = el.offsetParent
    if (!op || op === document.body || op === document.documentElement) return
    const ops = style(op)
    const ob = bounds(op)
    if (ob.width <= 0) return
    const outX = Math.max(cr.right - ob.right, ob.left - cr.left)
    const outY = Math.max(cr.bottom - ob.bottom, ob.top - cr.top)
    const cutX = outX > tol.escapeX && clips(ops.overflowX) && !scrolls(ops.overflowX)
    const cutY = outY > tol.escapeY && clips(ops.overflowY) && !scrolls(ops.overflowY)
    if (!cutX && !cutY) return
    add('escapes-offset-parent', 'error', el,
      `absolute ${round(cr.width)}x${round(cr.height)} escapes its clipping containing block`
      + ` ${path(op)} by ${round(cutX ? outX : outY)}px in ${cutX ? 'x' : 'y'}`
      + ` (overflow ${ops.overflowX}/${ops.overflowY}), so that much of it is cut off`)
  }

  /** Walk out from an in-flow child to the box that is meant to hold it: the
   *  nearest ancestor that CLIPS (where crossing it means being cut off) or
   *  PAINTS (where crossing it is visible). A bare layout div is neither, so the
   *  walk goes through it; a scroller ends the walk, because being wider than a
   *  scroller is how a scroller is used.
   *
   *  The first version of this rule reported against the immediate parent
   *  whatever it was, which was wrong in both directions: it named a bare div
   *  with no edges instead of the card the child actually hung off, and it fired
   *  on a wide child inside a bare div inside an `overflow-x:auto` wrapper, which
   *  is a table that scrolls, not a defect. A child wider than a bare column with
   *  nothing painting or clipping anywhere above it produces no finding here;
   *  what that costs the page is page-level horizontal scroll, which is rule 1's. */
  function escapeWalk(el: Element, cr: DOMRect) {
    let node: Element | null = el.parentElement
    for (let depth = 0; node && node !== document.body && depth < 12; depth++) {
      const ns = style(node)
      // No box of its own: whatever contains IT is the next question.
      if (ns.display === 'contents') { node = node.parentElement; continue }
      const nb = bounds(node)
      if (nb.width <= 0) { node = node.parentElement; continue }

      // The ancestor scrolls: overflowing it is its business, not a defect
      // (that is the table-wrapper case, and the wrapper itself is rule 1's).
      if (scrolls(ns.overflowX) || scrolls(ns.overflowY)) return

      const cut = clips(ns.overflowX) || clips(ns.overflowY)
      const draws = paints(ns)
      // A bare layout box: it has no edge anyone can see the child cross.
      if (!cut && !draws) { node = node.parentElement; continue }

      const where = `${cut ? 'clipping' : 'drawn'} ${depth === 0 ? 'parent' : 'ancestor'} ${path(node)}`
      const right = cr.right - nb.right
      const left = nb.left - cr.left
      const worstX = Math.max(right, left)
      if (worstX > tol.escapeX) {
        add(cut ? 'escapes-clipper' : 'escapes-container', 'error', el,
          `child ${round(cr.width)}x${round(cr.height)} at x=[${round(cr.left)},${round(cr.right)}]`
          + ` escapes ${where} x=[${round(nb.left)},${round(nb.right)}] by ${round(worstX)}px`
          + ` (${right > left ? 'right' : 'left'})${cut ? ', and is clipped there' : ''}`)
        return
      }
      const below = cr.bottom - nb.bottom
      if (below > tol.escapeY) {
        add(cut ? 'escapes-clipper' : 'escapes-container', 'error', el,
          `child bottom=${round(cr.bottom)} escapes ${where} bottom=${round(nb.bottom)}`
          + ` by ${round(below)}px${cut ? ', and is clipped there' : ''}`)
      }
      // This ancestor is the box a reader would name, and the child is inside
      // it. Anything further out is a different question.
      return
    }
  }

  // ── 3. Single-line text that does not fit its box ─────────────────────────
  for (const el of all) {
    const s = style(el)
    if (s.whiteSpace !== 'nowrap' && s.whiteSpace !== 'pre') continue
    if (s.display === 'inline') continue // no client box to measure against
    if (s.overflowX === 'auto' || s.overflowX === 'scroll') continue // scrolls on purpose
    if (!shown(el)) continue
    if (el.clientWidth <= 0) continue
    // Screen-reader-only text (the 1px clip pattern) is meant to be
    // unreadable, so "the content does not fit" is the point, not a defect.
    const r = rect(el)
    if (r.width <= 4 || r.height <= 4) continue
    const excess = el.scrollWidth - el.clientWidth
    if (excess <= tol.text) continue
    const text = (el.textContent ?? '').trim().slice(0, 60)
    if (!text) continue
    if (s.textOverflow === 'ellipsis') {
      // Deliberate truncation. Allowed, but it means someone is reading "hauksb…"
      // where a name was meant to be, so it is on the report.
      add('text-truncated', 'info', el,
        `nowrap+ellipsis, content ${el.scrollWidth}px in ${el.clientWidth}px (${excess}px hidden): "${text}"`)
    } else {
      add('text-clipped', 'error', el,
        `nowrap, content ${el.scrollWidth}px in ${el.clientWidth}px (${excess}px cut, no ellipsis): "${text}"`)
    }
  }

  // ── 4. A control's own label does not fit the control ─────────────────────
  //
  // Rule 3 catches a nowrap element wider than its own client box, which is the
  // horizontal case AND only when the nowrap sits on the measured element. It
  // never caught the commonest button defect: a fixed-height control whose label
  // wrapped to two lines, so half of it is outside the button. Nothing about
  // that shows up in `scrollWidth`, the label is not nowrap, and the label span
  // is inline so rule 3 skips it.
  //
  // Measured with a Range over the control's own content, against the control's
  // BORDER box (not its content box): text that eats its padding still reads as
  // being on the button, text past the border does not.
  const LABELLED_CONTROLS =
    'button, a[href], summary, [role="button"], input[type="button"], input[type="submit"], input[type="reset"]'
  for (const el of all) {
    if (!el.matches(LABELLED_CONTROLS)) continue
    if (!shown(el)) continue
    const s = style(el)
    if (s.display === 'inline') continue // no box of its own to be cut by
    if (scrolls(s.overflowX) || scrolls(s.overflowY)) continue // scrolls on purpose
    const label = (el.textContent ?? '').trim()
    if (!label) continue // an icon-only control has no label to clip
    let rects: DOMRect[] = []
    try {
      const range = document.createRange()
      range.selectNodeContents(el)
      rects = Array.from(range.getClientRects()).filter(r => r.width > 0.5 && r.height > 0.5)
      range.detach()
    } catch {
      continue
    }
    if (rects.length === 0) continue
    const r = rect(el)
    const top = Math.min(...rects.map(x => x.top))
    const bottom = Math.max(...rects.map(x => x.bottom))
    const left = Math.min(...rects.map(x => x.left))
    const right = Math.max(...rects.map(x => x.right))
    const outY = Math.max(r.top - top, bottom - r.bottom)
    const outX = Math.max(r.left - left, right - r.right)
    const worst = Math.max(outX, outY)
    if (worst <= tol.controlText) continue
    const cut = clips(s.overflowX) || clips(s.overflowY)
    add('control-label-overflows', 'error', el,
      `label "${label.slice(0, 40)}" needs ${round(right - left)}x${round(bottom - top)}`
      + ` in a ${round(r.width)}x${round(r.height)} control`
      + ` (${round(worst)}px outside it in ${outY >= outX ? 'y' : 'x'};`
      + ` ${cut ? `clipped by overflow:${clips(s.overflowY) ? s.overflowY : s.overflowX}` : 'spilling over what is next to it'})`)
  }

  // ── 5. Images: loaded, and not stretched ─────────────────────────────────
  for (const img of Array.from(document.images)) {
    if (!shown(img)) continue
    const ir = rect(img)
    // An image below the fold with loading="lazy" is UNLOADED, which is the
    // feature. Only what a viewer can currently see has to have arrived.
    const onScreen = ir.bottom > 0 && ir.top < vh && ir.right > 0 && ir.left < vw
    if (!onScreen) continue
    if (!img.complete || img.naturalWidth === 0) {
      add('image-broken', 'error', img, `naturalWidth=${img.naturalWidth} src=${img.currentSrc || img.src}`)
      continue
    }
    const s = style(img)
    // object-fit other than `fill` deliberately decouples the two aspects.
    if (s.objectFit !== 'fill') continue
    if (ir.width < 4 || ir.height < 4) continue
    const natural = img.naturalWidth / img.naturalHeight
    const rendered = ir.width / ir.height
    const off = Math.abs(rendered - natural) / natural
    if (off > tol.aspect) {
      add('image-distorted', 'error', img,
        `rendered ${ir.width.toFixed(1)}x${ir.height.toFixed(1)} (${rendered.toFixed(3)})`
        + ` vs natural ${img.naturalWidth}x${img.naturalHeight} (${natural.toFixed(3)}),`
        + ` off by ${(off * 100).toFixed(1)}%`)
    }
  }

  // ── 6. Fixed/sticky chrome must not sit on top of a control ───────────────
  // Measured with elementFromPoint, which answers the question the user asks
  // by clicking: what does this pixel belong to?
  const CONTROLS = 'button, a[href], input, select, textarea, summary, [role="button"]'
  for (const el of all) {
    if (!el.matches(CONTROLS)) continue
    if (!shown(el)) continue
    if ((el as HTMLButtonElement).disabled) continue
    const r = rect(el)
    // Only pixels actually on screen can be covered; anything scrolled out is
    // not this rule's business.
    const cx = Math.round(Math.min(Math.max(r.left + r.width / 2, 1), vw - 1))
    const cy = Math.round(Math.min(Math.max(r.top + r.height / 2, 1), vh - 1))
    if (r.bottom < 0 || r.top > vh || r.right < 0 || r.left > vw) continue
    if (cx < r.left - 1 || cx > r.right + 1 || cy < r.top - 1 || cy > r.bottom + 1) continue
    const top = document.elementFromPoint(cx, cy)
    if (!top || top === el || el.contains(top) || top.contains(el)) continue
    // Walk up from whatever owns the pixel looking for the fixed/sticky layer
    // that put it there. A shared ancestor means they are in the same layer and
    // the overlap is ordinary stacking, not chrome covering content.
    let node: Element | null = top
    let overlay: Element | null = null
    while (node && node !== document.body) {
      const p = style(node).position
      if (p === 'fixed' || p === 'sticky') { overlay = node; break }
      node = node.parentElement
    }
    if (!overlay || overlay.contains(el)) continue
    // A dialog, a menu or a listbox covering the page is not chrome sitting on
    // content: it is the thing the user just opened, and the next click anywhere
    // outside it closes it. This rule is about the OTHER case, the sticky
    // toolbar or the pinned pane that permanently owns the pixels of a button
    // nobody can reach. The discriminator is the overlay's own declared role,
    // which is also what tells a screen reader the same thing.
    const role = overlay.getAttribute('role')
    if (role && /^(dialog|alertdialog|menu|listbox|tooltip)$/.test(role)) continue
    if (overlay.getAttribute('aria-modal') === 'true') continue
    add('overlay-covers-control', 'error', el,
      `centre (${cx},${cy}) belongs to ${path(top)} under ${style(overlay).position} ${path(overlay)}`)
  }

  return out
}
