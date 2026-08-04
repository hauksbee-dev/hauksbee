// The widget bundle: mounts the embeddable demo into a container and speaks the
// embed contract. Loaded two ways, and it does not care which:
//
//   - inside demo/embed-dist/iframe.html, where the bridge below turns the
//     contract into postMessage (the isolated shape, and the recommended one);
//   - directly by a host page via mountInline() from hauksbee-embed.js, where
//     the contract is the returned object's methods and events.
//
// Everything the widget needs is in this bundle plus the recorded assets under
// its asset base. There is no engine, no API and no third party.

import { createRoot } from 'react-dom/client'
import type { Root } from 'react-dom/client'
import { EmbedWidget } from './EmbedWidget'
import type { WidgetHandle } from './EmbedWidget'
import { SUGGESTED_HEIGHT } from './contract'
import type { EmbedEvent, EmbedEventName, EmbedState } from './contract'
import css from './styles.css?inline'

export type { EmbedEvent, EmbedEventName, EmbedState }
export { SUGGESTED_HEIGHT }

/** The version the app reports. A build-time literal from the frontend's
 *  package.json (see demo/embed/vite.config.ts). */
declare const __APP_VERSION__: string

export interface MountOptions {
  /** Where the widget renders. */
  container: HTMLElement
  /** Where the recorded assets live, e.g. "/hauksbee-embed/". Defaults to the
   *  directory this module was loaded from. */
  assetBase?: string
  /** Board to open on. Defaults to the first in the cache index. */
  board?: string
  /** Initial visual state. */
  state?: EmbedState
  /** Event sink. */
  onEvent?: (e: EmbedEvent) => void
}

export interface MountedWidget {
  expand: () => void
  collapse: () => void
  reset: () => void
  loadBoard: (id: string) => void
  destroy: () => void
}

const defaultAssetBase = () => new URL('./', import.meta.url).pathname

/** Inject the app's stylesheet once per document. The widget owns a scoped root
 *  (`.hb-embed-root`) and the app's own CSS is what makes the app look like the
 *  app; a host that wants none of it in its document should use the iframe. */
function injectCss(doc: Document) {
  if (doc.querySelector('style[data-hauksbee-embed]')) return
  const style = doc.createElement('style')
  style.setAttribute('data-hauksbee-embed', '')
  style.textContent = css
  doc.head.appendChild(style)
}

export function mount(opts: MountOptions): MountedWidget {
  const { container } = opts
  injectCss(container.ownerDocument)
  container.classList.add('hb-embed-host')

  let handle: WidgetHandle | null = null
  const pending: (() => void)[] = []
  const withHandle = (fn: (h: WidgetHandle) => void) => {
    if (handle) fn(handle)
    else pending.push(() => { if (handle) fn(handle) })
  }

  let root: Root | null = createRoot(container)
  root.render(
    <EmbedWidget
      assetBase={opts.assetBase ?? defaultAssetBase()}
      initialBoardId={opts.board}
      initialState={opts.state ?? 'compact'}
      version={typeof __APP_VERSION__ === 'string' ? __APP_VERSION__ : '0.0.0'}
      onEvent={e => opts.onEvent?.(e)}
      handleRef={h => {
        handle = h
        while (pending.length > 0) pending.shift()?.()
      }}
    />,
  )

  return {
    expand: () => withHandle(h => h.setState('expanded')),
    collapse: () => withHandle(h => h.setState('compact')),
    reset: () => withHandle(h => h.reset()),
    loadBoard: id => withHandle(h => h.loadBoard(id)),
    destroy: () => {
      root?.unmount()
      root = null
      container.classList.remove('hb-embed-host')
    },
  }
}

// ── The iframe bridge ────────────────────────────────────────────────────────
//
// Wire protocol, both directions (documented in demo/EMBED.md):
//   widget -> parent   { source: 'hauksbee-embed', type, payload }
//   parent -> widget   { target: 'hauksbee-embed', command, args }
//
// Nothing sensitive crosses it, so messages go to '*' and commands are accepted
// from the embedding parent whatever its origin: a landing page and its widget
// are frequently on different hosts, and locking this down would only be
// security theatre over a public demo. Commands are validated by name.

const COMMANDS = ['expand', 'collapse', 'reset', 'loadBoard'] as const
type Command = typeof COMMANDS[number]

export function bridge(container: HTMLElement) {
  const post = (type: string, payload?: unknown) => {
    try {
      window.parent?.postMessage({ source: 'hauksbee-embed', type, payload }, '*')
    } catch { /* a parent that refuses messages is not an error here */ }
  }

  const params = new URLSearchParams(location.search)
  const widget = mount({
    container,
    assetBase: params.get('assets') ?? defaultAssetBase(),
    board: params.get('board') ?? undefined,
    state: params.get('state') === 'expanded' ? 'expanded' : 'compact',
    onEvent: e => post(e.type, e.payload),
  })

  window.addEventListener('message', ev => {
    const data = ev.data as { target?: string; command?: string; args?: unknown }
    if (!data || data.target !== 'hauksbee-embed') return
    const cmd = data.command as Command | undefined
    if (!cmd || !COMMANDS.includes(cmd)) return
    switch (cmd) {
      case 'expand': widget.expand(); break
      case 'collapse': widget.collapse(); break
      case 'reset': widget.reset(); break
      case 'loadBoard': {
        const id = typeof data.args === 'string'
          ? data.args
          : (data.args as { id?: string } | undefined)?.id
        if (id) widget.loadBoard(id)
        break
      }
    }
  })

  // The parent may have been listening before this frame's JS ran; the `ready`
  // event the widget emits covers the normal case, and this covers a parent
  // that attached late and asks.
  post('frameReady', { suggested_height: SUGGESTED_HEIGHT })
  return widget
}

// Auto-boot when loaded by iframe.html.
if (typeof document !== 'undefined') {
  const el = document.getElementById('hauksbee-embed')
  if (el) bridge(el)
}
