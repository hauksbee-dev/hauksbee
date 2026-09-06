// Minimal KiCad .kicad_pcb s-expression parser: only the geometry the 2D
// renderer and the hit-test need. Board-level graphics (gr_*) and footprint
// graphics (fp_*) are the SAME shapes, so both parse into one `Graphics` set;
// a footprint's are taken from its local frame into board mm on the way in,
// so nothing downstream has to know which it is looking at.
// All coordinates are KiCad mm.

// ── S-expression tokeniser ───────────────────────────────────────────────────

type SNode = string | SNode[]

function tokenise(src: string): SNode {
  let i = 0
  const ws = (c: string) => c === ' ' || c === '\t' || c === '\r' || c === '\n'
  const skipWS = () => { while (i < src.length && ws(src[i])) i++ }
  function readAtom(): string {
    let s = ''
    if (src[i] === '"') {
      i++
      while (i < src.length && src[i] !== '"') {
        if (src[i] === '\\') i++
        s += src[i++]
      }
      i++
      return s
    }
    while (i < src.length && src[i] !== '(' && src[i] !== ')' && !ws(src[i])) s += src[i++]
    return s
  }
  function readList(): SNode[] {
    i++
    const items: SNode[] = []
    for (;;) {
      skipWS()
      if (i >= src.length) break
      if (src[i] === ')') { i++; break }
      items.push(src[i] === '(' ? readList() : readAtom())
    }
    return items
  }
  skipWS()
  return src[i] === '(' ? readList() : readAtom()
}

const isList = (n: SNode): n is SNode[] => Array.isArray(n)
const head = (n: SNode[]): string => (isList(n[0]) ? '' : (n[0] as string))
const child = (parent: SNode[], tag: string): SNode[] | undefined =>
  parent.find((c): c is SNode[] => isList(c) && head(c) === tag)
const num = (n: SNode | undefined): number => (!n || isList(n) ? 0 : parseFloat(n) || 0)
const str = (n: SNode | undefined): string => (!n || isList(n) ? '' : n)
/** "(xy x y)", "(start x y)", "(at x y [angle])": the two numbers. */
const xy = (n: SNode[] | undefined): Point => (n ? { x: num(n[1]), y: num(n[2]) } : { x: 0, y: 0 })

/** Stroke width, from KiCad 6 `(stroke (width w))` or the older bare
 *  `(width w)`. Zero (or absent) falls back to `fallback`. */
function strokeWidth(c: SNode[], fallback: number): number {
  const stroke = child(c, 'stroke')
  return num((stroke ? child(stroke, 'width') : child(c, 'width'))?.[1]) || fallback
}

/** `(fill solid)` and `(fill (type solid))` both mean filled. */
function fillSolid(c: SNode[]): boolean {
  const fill = child(c, 'fill')
  return !!fill && (str(child(fill, 'type')?.[1]) === 'solid' || str(fill[1]) === 'solid')
}

/** A node's net id and the name it resolves to, ready to spread onto a track. */
function netOf(c: SNode[], nets: Map<string, string>): Net {
  const net = child(c, 'net') ? str(child(c, 'net')![1]) : undefined
  return { net, netName: net ? (nets.get(net) ?? net) : undefined }
}

// ── Geometry types ───────────────────────────────────────────────────────────

export interface Point { x: number; y: number }

export interface Net { net?: string; netName?: string }

interface Stroke { layer: string; width: number; fill?: boolean }
/** A straight stroke; also a rectangle when it is in `rects` (two corners). */
export interface Line extends Stroke { start: Point; end: Point }
/** A three-point arc: start, a point ON the arc, end. */
export interface Arc extends Line { mid: Point }
/** A circle by centre and a point on its edge, as KiCad writes it. */
export interface Circle extends Stroke { center: Point; end: Point }
export interface Poly extends Stroke { pts: Point[] }

/** Every drawable stroke on one owner (the board, or one footprint). */
export interface Graphics {
  lines: Line[]
  arcs: Arc[]
  circles: Circle[]
  rects: Line[]
  polys: Poly[]
}

const emptyGraphics = (): Graphics => ({ lines: [], arcs: [], circles: [], rects: [], polys: [] })

/** Every stroke of a graphics set, whatever its shape. */
export function allStrokes(g: Graphics): Stroke[] {
  return [...g.lines, ...g.arcs, ...g.circles, ...g.rects, ...g.polys]
}

/** The points a stroke spans, for bounds. */
export function strokePoints(g: Graphics, only?: (s: Stroke) => boolean): Point[] {
  const keep = only ?? (() => true)
  const pts: Point[] = []
  for (const l of [...g.lines, ...g.rects]) if (keep(l)) pts.push(l.start, l.end)
  for (const a of g.arcs) if (keep(a)) pts.push(a.start, a.mid, a.end)
  for (const c of g.circles) {
    if (!keep(c)) continue
    const r = Math.hypot(c.end.x - c.center.x, c.end.y - c.center.y)
    pts.push({ x: c.center.x - r, y: c.center.y - r }, { x: c.center.x + r, y: c.center.y + r })
  }
  for (const p of g.polys) if (keep(p)) pts.push(...p.pts)
  return pts
}

export interface Pad extends Net {
  number: string
  type: 'thru_hole' | 'smd' | 'connect' | 'np_thru_hole'
  shape: 'circle' | 'rect' | 'oval' | 'roundrect' | 'trapezoid' | 'custom'
  at: Point
  angle: number
  size: { w: number; h: number }
  drill?: { diameter: number; oval?: boolean; dx?: number; dy?: number }
}

export interface Footprint {
  ref: string
  value: string
  lib_id: string
  at: Point
  angle: number
  layer: string
  pads: Pad[]
  /** Silkscreen / fab / courtyard strokes, already in board mm. */
  graphics: Graphics
}

export type Segment = Line & Net
export type TrackArc = Arc & Net

export interface Via extends Net {
  at: Point
  size: number
  drill: number
  layers: string[]
}

export interface BoardBounds {
  minX: number; maxX: number; minY: number; maxY: number
  width: number; height: number; cx: number; cy: number
}

export interface ParsedBoard {
  footprints: Footprint[]
  segments: Segment[]
  arcs: TrackArc[]
  vias: Via[]
  /** Board-level graphics: the outline, drawings, board silkscreen. */
  graphics: Graphics
  /** net index → name */
  nets: Map<string, string>
  bounds: BoardBounds
}

// ── Strokes ──────────────────────────────────────────────────────────────────

type Transform = (p: Point) => Point
const identity: Transform = p => p

/** The footprint's placement as a local→board transform. */
function placement(at: Point, angleDeg: number): Transform {
  const rad = (angleDeg * Math.PI) / 180
  const cos = Math.cos(rad), sin = Math.sin(rad)
  return p => (angleDeg === 0
    ? { x: at.x + p.x, y: at.y + p.y }
    : { x: at.x + p.x * cos - p.y * sin, y: at.y + p.x * sin + p.y * cos })
}

/** A three-point arc from either form KiCad writes. KiCad 6 gives (start,
 *  mid, end). KiCad 5 gives a CENTRE, one endpoint and a swept angle:
 *  (start = centre, end = first endpoint, angle = degrees), and the sweep is
 *  positive in the file's own y-down frame. Measured rather than reasoned:
 *  converting all 14 Edge.Cuts arcs of the KiCad-5 Watchy this way lands
 *  every far endpoint exactly on a neighbouring segment's endpoint, gap
 *  0.0000 mm, where the opposite sign leaves 10 of 14 stranded by up to 4.1 mm. */
function arcPoints(c: SNode[]): { start: Point; mid: Point; end: Point } {
  const start = xy(child(c, 'start')), end = xy(child(c, 'end'))
  const midNode = child(c, 'mid'), angleNode = child(c, 'angle')
  if (!midNode && angleNode) {
    const r = Math.hypot(end.x - start.x, end.y - start.y)
    const t0 = Math.atan2(end.y - start.y, end.x - start.x)
    const sweep = (num(angleNode[1]) * Math.PI) / 180
    const at = (t: number) => ({ x: start.x + r * Math.cos(t), y: start.y + r * Math.sin(t) })
    return { start: end, mid: at(t0 + sweep / 2), end: at(t0 + sweep) }
  }
  return { start, mid: midNode ? xy(midNode) : { x: 0, y: 0 }, end }
}

/** Parse every `${prefix}line|arc|circle|rect|poly` child of `node` into a
 *  Graphics set. `fills` is read only for board-level graphics, which is the
 *  only place KiCad writes a filled outline the renderer honours. */
function parseGraphics(node: SNode[], prefix: 'gr_' | 'fp_', xf: Transform, fallbackWidth: number, fills: boolean): Graphics {
  const g = emptyGraphics()
  for (const c of node) {
    if (!isList(c) || !head(c).startsWith(prefix)) continue
    const base = { layer: str(child(c, 'layer')?.[1]), width: strokeWidth(c, fallbackWidth) }
    const fill = fills ? { fill: fillSolid(c) } : {}
    switch (head(c).slice(prefix.length)) {
      case 'line': g.lines.push({ ...base, start: xf(xy(child(c, 'start'))), end: xf(xy(child(c, 'end'))) }); break
      case 'rect': g.rects.push({ ...base, ...fill, start: xf(xy(child(c, 'start'))), end: xf(xy(child(c, 'end'))) }); break
      case 'arc': {
        const { start, mid, end } = arcPoints(c)
        g.arcs.push({ ...base, start: xf(start), mid: xf(mid), end: xf(end) })
        break
      }
      case 'circle': g.circles.push({ ...base, ...fill, center: xf(xy(child(c, 'center'))), end: xf(xy(child(c, 'end'))) }); break
      case 'poly':
        if (prefix === 'gr_') {
          const pts = (child(c, 'pts') ?? []).flatMap(p => (isList(p) && head(p) === 'xy' ? [xf(xy(p))] : []))
          g.polys.push({ ...base, ...fill, pts })
        }
        break
    }
  }
  return g
}

// ── Footprints and tracks ────────────────────────────────────────────────────

function parsePad(c: SNode[], nets: Map<string, string>, xf: Transform, fpAngle: number): Pad {
  const at = child(c, 'at')
  const sizeNode = child(c, 'size')
  const drillNode = child(c, 'drill')
  return {
    number: str(c[1]),
    type: str(c[2]) as Pad['type'],
    shape: str(c[3]) as Pad['shape'],
    at: xf(xy(at)),
    angle: fpAngle + num(at?.[3]),
    // Generated boards may omit (size ...): default to a visible 1 mm pad
    // rather than a 0x0 one the renderer would silently skip.
    size: sizeNode ? { w: num(sizeNode[1]), h: num(sizeNode[2]) } : { w: 1, h: 1 },
    drill: !drillNode
      ? undefined
      : str(drillNode[1]) === 'oval'
        ? { diameter: num(drillNode[2]), oval: true, dx: num(drillNode[3]), dy: num(drillNode[4]) }
        : { diameter: num(drillNode[1]) },
    ...netOf(c, nets),
  }
}

function parseFootprint(node: SNode[], nets: Map<string, string>): Footprint {
  const atNode = child(node, 'at')
  const at = xy(atNode), angle = num(atNode?.[3])
  const xf = placement(at, angle)
  let ref = '', value = ''
  for (const c of node) {
    if (!isList(c)) continue
    const tag = head(c)
    // KiCad 7+: (property "Reference" "U1"); KiCad 5/6: (fp_text reference "U1");
    // older still: (reference "U1").
    if (tag === 'property') {
      if (str(c[1]) === 'Reference') ref = str(c[2])
      if (str(c[1]) === 'Value') value = str(c[2])
    } else if (tag === 'fp_text') {
      if (str(c[1]) === 'reference' && !ref) ref = str(c[2])
      if (str(c[1]) === 'value' && !value) value = str(c[2])
    } else if (tag === 'reference' && !ref) ref = str(c[1])
  }
  return {
    ref, value, lib_id: str(node[1]), at, angle, layer: str(child(node, 'layer')?.[1]),
    pads: node.flatMap(c => (isList(c) && head(c) === 'pad' ? [parsePad(c, nets, xf, angle)] : [])),
    graphics: parseGraphics(node, 'fp_', xf, 0.12, false),
  }
}

function parseSegment(c: SNode[], nets: Map<string, string>): Segment | null {
  const start = child(c, 'start'), end = child(c, 'end')
  if (!start || !end) return null
  return {
    start: xy(start), end: xy(end), layer: str(child(c, 'layer')?.[1]),
    width: num(child(c, 'width')?.[1]) || 0.25, ...netOf(c, nets),
  }
}

// ── Main parser ──────────────────────────────────────────────────────────────

export function parseKicadPcb(src: string): ParsedBoard {
  const root = tokenise(src)
  if (!isList(root) || head(root) !== 'kicad_pcb') throw new Error('Not a kicad_pcb file')

  const nets = new Map<string, string>()
  for (const c of root) if (isList(c) && head(c) === 'net') nets.set(str(c[1]), str(c[2]))

  const board: ParsedBoard = {
    footprints: [], segments: [], arcs: [], vias: [], nets,
    graphics: parseGraphics(root, 'gr_', identity, 0.05, true),
    bounds: { minX: 0, maxX: 0, minY: 0, maxY: 0, width: 0, height: 0, cx: 0, cy: 0 },
  }
  for (const c of root) {
    if (!isList(c)) continue
    switch (head(c)) {
      // `footprint` is KiCad 6+; `module` is the same node in KiCad <= 5 files.
      case 'footprint': case 'module': board.footprints.push(parseFootprint(c, nets)); break
      case 'segment': { const s = parseSegment(c, nets); if (s) board.segments.push(s); break }
      case 'arc': {
        const s = parseSegment(c, nets)
        if (s) {
          const mid = child(c, 'mid')
          board.arcs.push({ ...s, mid: mid ? xy(mid) : { x: (s.start.x + s.end.x) / 2, y: (s.start.y + s.end.y) / 2 } })
        }
        break
      }
      case 'via': {
        const at = child(c, 'at')
        if (!at) break
        const layers = child(c, 'layers')
        board.vias.push({
          at: xy(at), size: num(child(c, 'size')?.[1]) || 0.8, drill: num(child(c, 'drill')?.[1]) || 0.4,
          layers: layers ? layers.slice(1).map(l => str(l)) : ['F.Cu', 'B.Cu'], ...netOf(c, nets),
        })
        break
      }
    }
  }
  board.bounds = computeBounds(board)
  return board
}

// ── Bounds ───────────────────────────────────────────────────────────────────

function computeBounds(board: ParsedBoard): BoardBounds {
  // Prefer the Edge.Cuts outline (board-level or drawn inside a footprint);
  // fall back to every piece of geometry.
  const onEdge = (s: Stroke) => s.layer === 'Edge.Cuts'
  const edge = [...strokePoints(board.graphics, onEdge), ...board.footprints.flatMap(fp => strokePoints(fp.graphics, onEdge))]
  const points = edge.length > 4
    ? edge
    : [
        ...board.segments.flatMap(s => [s.start, s.end]),
        ...board.footprints.flatMap(fp => [fp.at, ...fp.pads.map(p => p.at)]),
        ...board.graphics.lines.flatMap(l => [l.start, l.end]),
      ]
  if (points.length === 0) return { minX: 0, maxX: 100, minY: 0, maxY: 100, width: 100, height: 100, cx: 50, cy: 50 }

  let minX = Infinity, maxX = -Infinity, minY = Infinity, maxY = -Infinity
  for (const p of points) {
    if (p.x < minX) minX = p.x
    if (p.x > maxX) maxX = p.x
    if (p.y < minY) minY = p.y
    if (p.y > maxY) maxY = p.y
  }
  // 5% padding.
  const padX = (maxX - minX) * 0.05, padY = (maxY - minY) * 0.05
  minX -= padX; maxX += padX; minY -= padY; maxY += padY
  return { minX, maxX, minY, maxY, width: maxX - minX, height: maxY - minY, cx: (minX + maxX) / 2, cy: (minY + maxY) / 2 }
}
