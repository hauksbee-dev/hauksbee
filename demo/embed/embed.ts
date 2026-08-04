// hauksbee-embed.js: the host-page API.
//
//   import { createHauksbeeDemo } from '/hauksbee-embed/hauksbee-embed.js'
//
//   const demo = createHauksbeeDemo({ container: el })
//   demo.on('engaged', () => growTheSection())
//   demo.on('requestExpand', () => growTheSection())
//   demo.expand()
//
// By default this puts the widget in an iframe: the demo runs the real Hauksbee
// frontend, which brings its own reset and its own theme attribute, and a
// landing page should not have to survive that. `inline: true` mounts it
// straight into the host document instead (same API), for a host that wants one
// DOM and accepts the styling consequences.
//
// This module is deliberately tiny: the widget itself (React, the app, the
// cached transport) is a separate chunk that only loads when it is needed, so a
// page that never scrolls to the demo never pays for it.

import { SUGGESTED_HEIGHT } from './contract'
import type { EmbedEvent, EmbedEventName, EmbedState } from './contract'

export type { EmbedEvent, EmbedEventName, EmbedState }
export { SUGGESTED_HEIGHT }

export interface CreateOptions {
  /** Element the widget is placed in. It is sized by the host, in both states. */
  container: HTMLElement
  /** Where the widget's own assets live, e.g. "/hauksbee-embed/". Defaults to
   *  the directory this module was served from. */
  assetBase?: string
  /** Board to open on (a cache index id: "watchy", "boot_gate", ...). */
  board?: string
  /** Initial state. Defaults to 'compact'. */
  state?: EmbedState
  /** Mount into the host document instead of an iframe. */
  inline?: boolean
  /** Wait until the container is near the viewport before loading anything.
   *  Defaults to true: the widget is a below-the-fold element on a landing page
   *  and it should cost nothing until it is on its way into view. */
  lazy?: boolean
  /** How far ahead of the viewport to start loading. Default '400px'. */
  rootMargin?: string
}

export interface HauksbeeDemo {
  on: (type: EmbedEventName | 'frameReady', fn: (payload?: Record<string, unknown>) => void) => () => void
  expand: () => void
  collapse: () => void
  reset: () => void
  loadBoard: (id: string) => void
  /** Advisory heights, per state. */
  suggestedHeight: typeof SUGGESTED_HEIGHT
  destroy: () => void
}

const baseFromModule = () => new URL('./', import.meta.url).href

type Listeners = Map<string, Set<(payload?: Record<string, unknown>) => void>>

function emitter() {
  const listeners: Listeners = new Map()
  return {
    on(type: string, fn: (payload?: Record<string, unknown>) => void) {
      const set = listeners.get(type) ?? new Set()
      set.add(fn)
      listeners.set(type, set)
      return () => set.delete(fn)
    },
    fire(type: string, payload?: Record<string, unknown>) {
      for (const fn of listeners.get(type) ?? []) {
        try { fn(payload) } catch (e) { console.error('[hauksbee-embed] listener threw', e) }
      }
    },
  }
}

/** Resolve once the container is (nearly) in view, or immediately when lazy is
 *  off / IntersectionObserver is unavailable. */
function whenNearViewport(el: HTMLElement, lazy: boolean, rootMargin: string): Promise<void> {
  if (!lazy || typeof IntersectionObserver === 'undefined') return Promise.resolve()
  return new Promise(resolve => {
    const io = new IntersectionObserver(entries => {
      if (entries.some(e => e.isIntersecting)) { io.disconnect(); resolve() }
    }, { rootMargin })
    io.observe(el)
  })
}

export function createHauksbeeDemo(opts: CreateOptions): HauksbeeDemo {
  const bus = emitter()
  const assetBase = opts.assetBase ?? baseFromModule()
  const lazy = opts.lazy !== false
  const rootMargin = opts.rootMargin ?? '400px'
  let destroyed = false

  // Inline: the widget chunk is imported and mounted into the host document.
  if (opts.inline) {
    let mounted: { expand(): void; collapse(): void; reset(): void; loadBoard(id: string): void; destroy(): void } | null = null
    const queue: ((w: NonNullable<typeof mounted>) => void)[] = []
    const run = (fn: (w: NonNullable<typeof mounted>) => void) => {
      if (mounted) fn(mounted); else queue.push(fn)
    }
    void whenNearViewport(opts.container, lazy, rootMargin).then(async () => {
      if (destroyed) return
      const mod = await import('./widget')
      if (destroyed) return
      mounted = mod.mount({
        container: opts.container,
        assetBase,
        board: opts.board,
        state: opts.state,
        onEvent: (e: EmbedEvent) => bus.fire(e.type, e.payload),
      })
      while (queue.length > 0) queue.shift()?.(mounted)
    })
    return {
      on: bus.on,
      expand: () => run(w => w.expand()),
      collapse: () => run(w => w.collapse()),
      reset: () => run(w => w.reset()),
      loadBoard: id => run(w => w.loadBoard(id)),
      suggestedHeight: SUGGESTED_HEIGHT,
      destroy: () => { destroyed = true; mounted?.destroy() },
    }
  }

  // Iframed (the default).
  const frame = document.createElement('iframe')
  frame.title = 'Hauksbee demo'
  frame.setAttribute('loading', 'lazy')
  frame.setAttribute('allow', '')
  frame.style.cssText = 'display:block;width:100%;height:100%;border:0;background:transparent'
  const src = new URL('iframe.html', assetBase)
  src.searchParams.set('assets', new URL('.', assetBase).href)
  if (opts.board) src.searchParams.set('board', opts.board)
  if (opts.state) src.searchParams.set('state', opts.state)

  const send = (command: string, args?: unknown) => {
    frame.contentWindow?.postMessage({ target: 'hauksbee-embed', command, args }, '*')
  }

  const onMessage = (ev: MessageEvent) => {
    if (ev.source !== frame.contentWindow) return
    const data = ev.data as { source?: string; type?: string; payload?: Record<string, unknown> }
    if (!data || data.source !== 'hauksbee-embed' || !data.type) return
    bus.fire(data.type, data.payload)
  }
  window.addEventListener('message', onMessage)

  void whenNearViewport(opts.container, lazy, rootMargin).then(() => {
    if (destroyed) return
    frame.src = src.href
    opts.container.appendChild(frame)
  })

  return {
    on: bus.on,
    expand: () => send('expand'),
    collapse: () => send('collapse'),
    reset: () => send('reset'),
    loadBoard: id => send('loadBoard', id),
    suggestedHeight: SUGGESTED_HEIGHT,
    destroy: () => {
      destroyed = true
      window.removeEventListener('message', onMessage)
      frame.remove()
    },
  }
}

// A host that would rather not write JS: one script tag with data attributes.
//   <div data-hauksbee-demo data-board="watchy"></div>
//   <script type="module" src="/hauksbee-embed/hauksbee-embed.js"></script>
if (typeof document !== 'undefined') {
  const auto = () => {
    for (const el of document.querySelectorAll<HTMLElement>('[data-hauksbee-demo]')) {
      if (el.dataset.hauksbeeMounted === '1') continue
      el.dataset.hauksbeeMounted = '1'
      const demo = createHauksbeeDemo({
        container: el,
        board: el.dataset.board || undefined,
        state: el.dataset.state === 'expanded' ? 'expanded' : 'compact',
        inline: el.dataset.inline === 'true',
        assetBase: el.dataset.assets || undefined,
      })
      // The one host behaviour worth doing without being asked: grow the box
      // when the widget asks to expand, shrink it back when it asks to collapse.
      el.style.height ||= `${SUGGESTED_HEIGHT.compact}px`
      el.style.transition ||= 'height 420ms cubic-bezier(0.2, 0, 0, 1)'
      demo.on('requestExpand', () => { el.style.height = `${SUGGESTED_HEIGHT.expanded}px` })
      demo.on('requestCollapse', () => { el.style.height = `${SUGGESTED_HEIGHT.compact}px` })
    }
  }
  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', auto)
  else auto()
}
