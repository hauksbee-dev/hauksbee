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
  // Only where the parent does NOT clip or scroll: if the parent has
  // overflow-x:auto the child is allowed to be wider (that is the table
  // wrapper case, and the wrapper itself is covered by rule 1).
  const ESCAPE_CANDIDATES = 'button, a, img, svg, input, select, textarea, code, [role="button"]'
  for (const el of all) {
    if (!el.matches(ESCAPE_CANDIDATES)) continue
    if (!shown(el)) continue
    const parent = el.parentElement
    if (!parent || parent === document.body) continue
    const ps = style(parent)
    // The parent clips or scrolls: overflowing it is its business, not a defect.
    if (ps.overflowX !== 'visible' || ps.overflowY !== 'visible') continue
    // Inline parents are line boxes, whose rect is the union of their lines;
    // comparing a child's box to that measures nothing.
    if (ps.display === 'inline') continue
    const cs = style(el)
    // Taken out of flow on purpose (a popover, a sticky bar, an overlay).
    if (cs.position === 'absolute' || cs.position === 'fixed' || cs.position === 'sticky') continue
    const pr = rect(parent)
    if (pr.width <= 0) continue
    const cr = rect(el)
    const right = cr.right - pr.right
    const left = pr.left - cr.left
    const worstX = Math.max(right, left)
    if (worstX > tol.escapeX) {
      add('escapes-container', 'error', el,
        `child ${Math.round(cr.width)}x${Math.round(cr.height)} at x=[${Math.round(cr.left)},${Math.round(cr.right)}]`
        + ` escapes parent x=[${Math.round(pr.left)},${Math.round(pr.right)}] by ${Math.round(worstX)}px`
        + ` (${right > left ? 'right' : 'left'})`)
      continue
    }
    // Vertical spill only counts out of something that draws a box: a card,
    // a bordered row, a filled chip. Out of a bare layout div it is invisible.
    const draws = ps.borderTopWidth !== '0px' || ps.borderBottomWidth !== '0px'
      || (ps.backgroundColor !== 'rgba(0, 0, 0, 0)' && ps.backgroundColor !== 'transparent')
    const below = cr.bottom - pr.bottom
    if (draws && below > tol.escapeY) {
      add('escapes-container', 'error', el,
        `child bottom=${Math.round(cr.bottom)} escapes drawn parent bottom=${Math.round(pr.bottom)}`
        + ` by ${Math.round(below)}px`)
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

  // ── 4. Images: loaded, and not stretched ─────────────────────────────────
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

  // ── 5. Fixed/sticky chrome must not sit on top of a control ───────────────
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
    add('overlay-covers-control', 'error', el,
      `centre (${cx},${cy}) belongs to ${path(top)} under ${style(overlay).position} ${path(overlay)}`)
  }

  return out
}
