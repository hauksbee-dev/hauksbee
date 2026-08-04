# The embeddable demo widget

The landing site's conversion element: a box that is already showing a real
board, already moving, and answers a click. It runs the actual Hauksbee
frontend, not a video and not a mock, against runs the real engine already made.

    demo/embed/         source
    demo/embed-dist/    build output (copy this whole directory)
    demo/sessions/      the recordings it replays

Build it: `demo/embed/build.sh`. Verify it: `bun demo/embed/test/embed-e2e.ts`
(a real browser over the built bundle; screenshots land in
`demo/embed-test-results/`).

---

## 1. Integration

### Hosting the assets

Copy `demo/embed-dist/` into the site's `public/` under one directory, keeping
its shape:

    public/hauksbee-embed/
      hauksbee-embed.js       the host module (this is the only file you import)
      iframe.html             the widget document
      iframe.js
      chunks/                 the widget itself, and the 3D viewer, both lazy
      favicon.svg
      sessions/               the recordings (see "payload" below)

Two hosting requirements, both one-liners:

1. **Serve `sessions/*.jsonl` and `sessions/*.kicad_pcb` as text.** Any
   `text/plain` is fine. They must not 404 and must not be rewritten by an SPA
   fallback.
2. **Put a `favicon.svg` at the site root.** The app's sidebar wordmark asks for
   `/favicon.svg` (root-absolute, inside the app's own markup). A copy ships in
   `embed-dist/favicon.svg`; the site needs one at `/favicon.svg` or that single
   request 404s. Nothing else in the widget is root-absolute.

### The one-line shape

```html
<div data-hauksbee-demo data-board="watchy"></div>
<script type="module" src="/hauksbee-embed/hauksbee-embed.js"></script>
```

That mounts the widget, sets the box to the compact height, and animates the box
between the two heights when the widget asks. `data-*` attributes: `data-board`,
`data-state` (`compact` | `expanded`), `data-inline` (`true` to skip the iframe),
`data-assets` (asset base, if the script is served from elsewhere).

### The JS shape (what the site should use)

```js
import { createHauksbeeDemo } from '/hauksbee-embed/hauksbee-embed.js'

const box = document.querySelector('#demo')          // the host owns this box
const demo = createHauksbeeDemo({
  container: box,
  board: 'watchy',        // 'watchy' | 'boot_gate' | 'blinky' | 'button_pullup'
  state: 'compact',       // opening state
  lazy: true,             // default: load nothing until the box nears the viewport
  rootMargin: '400px',    // how early "nears" is
  // inline: true,        // mount into this document instead of an iframe
  // assetBase: '/hauksbee-embed/',   // defaults to where this module was served
})

demo.on('ready',           p => {})   // the compact surface is interactive
demo.on('engaged',         p => {})   // first meaningful interaction
demo.on('idle',            p => {})   // 30s without one
demo.on('requestExpand',   p => { box.style.height = demo.suggestedHeight.expanded + 'px' })
demo.on('requestCollapse', p => { box.style.height = demo.suggestedHeight.compact + 'px' })
demo.on('error',           p => {})   // a recording failed to load

demo.expand()             // commands
demo.collapse()
demo.reset()
demo.loadBoard('boot_gate')
demo.destroy()
```

`createHauksbeeDemo` returns synchronously and queues commands issued before the
widget has mounted. `on()` returns an unsubscribe function.

### iframe or inline

`createHauksbeeDemo` **iframes by default**, and that is the recommendation. The
widget carries the app's whole stylesheet, which includes a CSS reset and sets
`html, body { height: 100%; overflow: hidden; background: <app canvas> }`; the
app also writes `data-theme` on the document element for its light/dark toggle.
In an iframe none of that reaches the site. With `inline: true` all of it lands
in the host document, and only a page built for that should ask for it.

The iframe can also be used directly, with no host module at all:

```html
<iframe src="/hauksbee-embed/iframe.html?board=watchy&assets=/hauksbee-embed/"
        style="width:100%;height:340px;border:0" title="Hauksbee demo"></iframe>
```

Query parameters: `board`, `state`, `assets`.

---

## 2. The contract

### Events (widget to host)

| event | when | payload |
| --- | --- | --- |
| `ready` | the compact surface is rendered and interactive | `board`, `state`, `boards[{id,title}]`, `suggested_height`, `engine_version` |
| `engaged` | the FIRST meaningful interaction: a net or part clicked on the map, a spec chip, a board switch, any click inside the app, or an expand | `how` (`net:+3V3`, `part:U4`, `preset:...`, `board:...`, `app`, `button`), `board`, `state` |
| `idle` | 30 s with no pointer, wheel or key inside the widget | `for_ms`, `board`, `state` |
| `requestExpand` | the visitor asked for the full surface | `board`, `how`, `suggested_height` |
| `requestCollapse` | the visitor asked to shrink it back | `board`, `suggested_height` |
| `error` | a recording could not be loaded | `message`, `phase`, `board` |

`engaged` fires **once** per widget lifetime (until `reset()`), so a host can use
it as the "this visitor is interested" signal without debouncing.

`frameReady` also arrives from the iframe shape before `ready`, for a parent that
attached its listener late. Ignore it unless you need it.

### Commands (host to widget)

`expand()`, `collapse()`, `reset()`, `loadBoard(id)`. Over postMessage the same
commands are `{ target: 'hauksbee-embed', command, args }`; events come back as
`{ source: 'hauksbee-embed', type, payload }`. Messages are posted to `*` and
commands are accepted from the parent whatever its origin: nothing sensitive
crosses this boundary, and there is no state in the widget worth protecting.

### Sizing

The widget **fills its container** in both states and never resizes it. The host
owns the box; the widget says which height suits the state:

    demo.suggestedHeight  ->  { compact: 340, expanded: 760 }

Animate `height` on the container (the harness uses
`height 420ms cubic-bezier(0.2, 0, 0, 1)`). Below about 900 px wide the app's own
sidebar collapses to icons, which is its real responsive behaviour; below about
480 px the expanded surface is cramped, so prefer keeping the widget compact on
phones and letting the expand be a tap that grows the section.

---

## 3. What is inside, and what is honest about it

The expanded state is `frontend/src/App.tsx`, the same component the installed
engine serves. The board handed to it goes in through the app's own file input,
so the report, the 2D map, the net and part selection, the checks builder and the
live surface all take the code path a dropped board takes. What is different is
the transport: instead of a local engine, `fetch` is answered from
`demo/sessions/cache/<board>.json`, recorded by a real `hauksbee serve` against
the real board (`demo/capture/record-embed-cache.ts`).

Rules the widget keeps, and the site copy should not contradict:

- **Every engine answer is a recording.** Reports, check verdicts, frames, faults
  and UART are what the engine said on that board on that day, with the commit
  recorded next to them. Nothing is synthesised or interpolated.
- **The caption is always on screen**, in both states:
  *"A recorded run of the real engine on this board. Your boards run locally."*
  followed by the recording date and engine version in the expanded state.
- **An interaction with no recording behind it is not offered.** The environment
  page, saved sessions, the upload/drop zone, datasheet extraction and part-model
  writing are hidden (`demo/embed/embed.css` lists each with its reason). What
  remains cannot dead-end.
- **An unrecorded spec is refused in words.** The checks panel is a real editor,
  so a visitor can compose a spec that was never run. They get the app's own
  error surface saying which assertion has no recording and that installing
  hauksbee runs it for real. Never a spinner, never a plausible verdict.
- **A verdict assembled from per-rule recordings says so.** Each rule was
  recorded on its own as well as in its preset, so any subset the visitor builds
  answers from real per-rule runs; when that happens the caption adds
  *"That verdict was assembled row by row from recorded per-rule runs of this
  spec."*
- **Two things are bookkeeping, not recordings**, because the app needs a
  coherent server to talk to: which board this page has loaded, and whether a
  live session is "running". The live surface is a recording and labels itself
  `replay` on its own transport bar, with the app's own "RECORDED RUN" card
  explaining that inputs cannot be changed.

---

## 4. The boards

Four, all cached, all offered in the widget's own switcher. `loadBoard(id)` takes
the id.

| id | title | what it is | firmware staged | recorded specs |
| --- | --- | --- | --- | --- |
| `watchy` | Watchy v1.5 | a fabricated ESP32-S3 e-paper smartwatch: 82 parts, 84 nets, two rails, a charger, a boost converter; 50 DRC findings | no (the S3 flash image is 4.2 MB; its co-sim is the recorded session) | the power-up ladder (green), the haptic line asked too early (red) |
| `boot_gate` | Boot gate + firmware | an ATmega328P driving a MOSFET gate with no pull resistor; whether that is a bug depends on the firmware | yes (`boot_gate.hex`) | gate check with firmware (green), the same check with none (red) |
| `blinky` | Blinky | ATmega328P, LED on D13, divider into ADC0; the firmware blinks at 5 Hz and prints a banner | yes (`demo.hex`) | the bench sanity pass (green), a check asking for a string the firmware never prints (red) |
| `button_pullup` | Button + pull-up | three nets, one resistor: the smallest thing the engine can be asked about | n/a | the rail and the pull-up (green), as if the button were held (red) |

`watchy` is the default and the one the compact state is written for ("This is
the real report for a real smartwatch. Click a net."). `boot_gate` carries the
two-sided story worth telling in copy: the same check, green with the firmware
staged and red with it unstaged, because only running the firmware can tell a
floating gate from an intended one.

---

## 5. Payload

Measured on the built bundle (gzip, as a CDN serves it):

| what | when it loads | raw | gzip |
| --- | --- | --- | --- |
| `hauksbee-embed.js` | with the page (or deferred; it is 3 kB) | 2.9 kB | 1.4 kB |
| `iframe.html` + `iframe.js` | when the box nears the viewport | 1.6 kB | 0.9 kB |
| the widget (React + the app + the cached transport, CSS inlined) | with the iframe | 672 kB | **199 kB** |
| the 3D viewer (three.js) | only if a visitor switches the map to 3D | 686 kB | 175 kB |
| per board: cache + board + session | when that board is opened | see below | |

Per-board recorded assets:

| board | cache | board file | session | total gzip |
| --- | --- | --- | --- | --- |
| watchy | 14 kB gz | 132 kB gz | 132 kB gz | ~278 kB |
| boot_gate | 4 kB gz | 1 kB gz | 14 kB gz | ~19 kB |
| blinky | 4 kB gz | 2 kB gz | 11 kB gz | ~17 kB |
| button_pullup | 3 kB gz | 1 kB gz | 9 kB gz | ~13 kB |

So the embed payload before any board is **~200 kB gz**, against a ~600 kB
budget, and the default board adds ~278 kB gz of recording. Nothing loads until
the container nears the viewport (`lazy` defaults on); the 3D chunk loads only if
someone asks for 3D.

**About 3D, which is retired.** The map's 2D/3D control is the app's own, and 3D
works: the model is generated from the board already in memory, so it needs no
extra recording and no extra request, and the three.js chunk only arrives on the
click. It is nevertheless hidden in the widget. The only frame-rate measurement
anyone has is from the verification harness, which renders WebGL in software and
manages about 2 fps, and a widget whose whole job is to feel alive cannot be
shipped on the hope that a visitor's machine does better. Measure it on real
hardware (a mid-range laptop, then a phone) and, if it is smooth, delete one rule
from `demo/embed/embed.css` (it is commented, and the e2e pass asserts the control
stays absent, so the rule cannot go stale unnoticed). Until then the chunk ships
but is never fetched: 175 kB gz of the deploy that no visitor downloads, kept so
re-enabling is a one-line change rather than a rebuild of the contract.

Measured cold in the verification pass: **compact interactive in 230-530 ms** from
navigation, on localhost, including bundle parse.

---

## 6. Re-recording

The widget is only as current as its recordings.

    scripts/capture-demo-sessions.sh              # everything, current engine
    scripts/capture-demo-sessions.sh --cache      # just the interaction cache
    scripts/capture-demo-sessions.sh watchy-display_init   # one scenario + its board
    demo/embed/build.sh                           # rebuild the widget
    bun demo/embed/test/embed-e2e.ts              # verify it in a browser

Every recording carries the engine version and commit that produced it, and the
widget shows them in the caption. A cache whose board no longer parses, or a
check that no longer runs, is dropped from the cache rather than shipped stale,
and the widget then does not offer that interaction.
