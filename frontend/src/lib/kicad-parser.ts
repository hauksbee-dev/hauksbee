// Minimal KiCad .kicad_pcb s-expression parser.
// Parses only the geometry needed for 2D rendering:
//   footprints (with pads, fp_lines, fp_arcs, fp_circles)
//   gr_line, gr_arc, gr_circle, gr_rect, gr_poly (board-level graphics)
//   segment, arc (tracks/copper)
//   via
// All coordinates are in KiCad mm units.

// ────────────────────────── S-expr tokeniser ──────────────────────────

type SNode = string | SNode[]

function tokenise(src: string): SNode {
  let i = 0

  function skipWS() {
    while (i < src.length && (src[i] === ' ' || src[i] === '\t' || src[i] === '\r' || src[i] === '\n')) i++
  }

  function readAtom(): string {
    if (src[i] === '"') {
      i++ // skip opening quote
      let s = ''
      while (i < src.length && src[i] !== '"') {
        if (src[i] === '\\') { i++; s += src[i++] } else s += src[i++]
      }
      i++ // skip closing quote
      return s
    }
    let s = ''
    while (i < src.length && src[i] !== '(' && src[i] !== ')' && src[i] !== ' ' && src[i] !== '\t' && src[i] !== '\r' && src[i] !== '\n') {
      s += src[i++]
    }
    return s
  }

  function readList(): SNode {
    i++ // skip '('
    const items: SNode[] = []
    while (true) {
      skipWS()
      if (i >= src.length) break
      if (src[i] === ')') { i++; break }
      if (src[i] === '(') items.push(readList())
      else items.push(readAtom())
    }
    return items
  }

  skipWS()
  if (src[i] === '(') return readList()
  return readAtom()
}

// ────────────────────── Helper accessors ──────────────────────────────

function isList(n: SNode): n is SNode[] { return Array.isArray(n) }
function head(n: SNode[]): string { return isList(n[0]) ? '' : (n[0] as string) }

/** Find the first child list whose head matches `tag`. */
function findChild(parent: SNode[], tag: string): SNode[] | undefined {
  for (const c of parent) {
    if (isList(c) && head(c) === tag) return c
  }
  return undefined
}


function num(n: SNode | undefined): number {
  if (!n || isList(n)) return 0
  return parseFloat(n as string) || 0
}

function str(n: SNode | undefined): string {
  if (!n || isList(n)) return ''
  return n as string
}

/** Parse "(at x y [angle])" → {x, y, angle} */
function parseAt(n: SNode[] | undefined): { x: number; y: number; angle: number } {
  if (!n) return { x: 0, y: 0, angle: 0 }
  return { x: num(n[1]), y: num(n[2]), angle: num(n[3]) }
}

/** Parse "(xy x y)" or "(start x y)" etc. */
function parseXY(n: SNode[] | undefined): { x: number; y: number } {
  if (!n) return { x: 0, y: 0 }
  return { x: num(n[1]), y: num(n[2]) }
}

function layerStr(n: SNode[] | undefined): string {
  if (!n) return ''
  const v = n[1]
  return str(v)
}

// ────────────────────── Board types ───────────────────────────────────

export interface Point { x: number; y: number }

export interface Pad {
  number: string
  type: 'thru_hole' | 'smd' | 'connect' | 'np_thru_hole'
  shape: 'circle' | 'rect' | 'oval' | 'roundrect' | 'trapezoid' | 'custom'
  at: Point
  angle: number
  size: { w: number; h: number }
  drill?: { diameter: number; oval?: boolean; dx?: number; dy?: number }
  net?: string
  netName?: string
}

export interface FpLine { start: Point; end: Point; layer: string; width: number }
export interface FpArc { start: Point; mid: Point; end: Point; layer: string; width: number }
export interface FpCircle { center: Point; end: Point; layer: string; width: number }
export interface FpRect { start: Point; end: Point; layer: string; width: number }

export interface Footprint {
  ref: string
  value: string
  lib_id: string
  at: Point
  angle: number
  layer: string
  pads: Pad[]
  fp_lines: FpLine[]
  fp_arcs: FpArc[]
  fp_circles: FpCircle[]
  fp_rects: FpRect[]
}

export interface Segment {
  start: Point; end: Point; layer: string; width: number; net?: string; netName?: string
}

export interface Track {
  start: Point; end: Point; layer: string; width: number; net?: string; netName?: string
}

export interface Arc {
  start: Point; mid: Point; end: Point; layer: string; width: number; net?: string; netName?: string
}

export interface Via {
  at: Point; size: number; drill: number; layers: string[]; net?: string; netName?: string
}

export interface GrLine { start: Point; end: Point; layer: string; width: number }
export interface GrArc { start: Point; mid: Point; end: Point; layer: string; width: number }
export interface GrCircle { center: Point; end: Point; layer: string; width: number; fill?: boolean }
export interface GrRect { start: Point; end: Point; layer: string; width: number; fill?: boolean }
export interface GrPoly { pts: Point[]; layer: string; width: number; fill?: boolean }

export interface BoardBounds {
  minX: number; maxX: number; minY: number; maxY: number
  width: number; height: number; cx: number; cy: number
}

export interface ParsedBoard {
  footprints: Footprint[]
  segments: Segment[]
  arcs: Arc[]
  vias: Via[]
  gr_lines: GrLine[]
  gr_arcs: GrArc[]
  gr_circles: GrCircle[]
  gr_rects: GrRect[]
  gr_polys: GrPoly[]
  /** net index → name */
  nets: Map<string, string>
  bounds: BoardBounds
}

// ────────────────────── Main parser ───────────────────────────────────

export function parseKicadPcb(src: string): ParsedBoard {
  const root = tokenise(src)
  if (!isList(root) || head(root) !== 'kicad_pcb') {
    throw new Error('Not a kicad_pcb file')
  }

  const board: ParsedBoard = {
    footprints: [],
    segments: [],
    arcs: [],
    vias: [],
    gr_lines: [],
    gr_arcs: [],
    gr_circles: [],
    gr_rects: [],
    gr_polys: [],
    nets: new Map(),
    bounds: { minX: 0, maxX: 0, minY: 0, maxY: 0, width: 0, height: 0, cx: 0, cy: 0 },
  }

  // Build net index → name map
  for (const c of root) {
    if (!isList(c)) continue
    if (head(c) === 'net') {
      const idx = str(c[1])
      const name = str(c[2])
      board.nets.set(idx, name)
    }
  }

  // Parse footprints
  for (const c of root) {
    // `footprint` is KiCad 6+; `module` is the same node in KiCad <= 5 files
    // (and in generated boards like the bundled boot_gate demo). Same shape.
    if (!isList(c) || (head(c) !== 'footprint' && head(c) !== 'module')) continue
    board.footprints.push(parseFootprint(c, board.nets))
  }

  // Parse board-level tracks
  for (const c of root) {
    if (!isList(c)) continue
    switch (head(c)) {
      case 'segment': {
        const s = parseSegment(c, board.nets)
        if (s) board.segments.push(s)
        break
      }
      case 'arc': {
        const a = parseArcTrack(c, board.nets)
        if (a) board.arcs.push(a)
        break
      }
      case 'via': {
        const v = parseVia(c, board.nets)
        if (v) board.vias.push(v)
        break
      }
      case 'gr_line': board.gr_lines.push(parseGrLine(c)); break
      case 'gr_arc': board.gr_arcs.push(parseGrArc(c)); break
      case 'gr_circle': board.gr_circles.push(parseGrCircle(c)); break
      case 'gr_rect': board.gr_rects.push(parseGrRect(c)); break
      case 'gr_poly': board.gr_polys.push(parseGrPoly(c)); break
    }
  }

  board.bounds = computeBounds(board)
  return board
}

function parseFootprint(node: SNode[], nets: Map<string, string>): Footprint {
  const lib_id = str(node[1])
  const atNode = findChild(node, 'at')
  const at = parseAt(atNode)
  const layer = layerStr(findChild(node, 'layer'))

  let ref = ''
  let value = ''
  for (const c of node) {
    if (!isList(c)) continue
    if (head(c) === 'property') {
      const propName = str(c[1])
      const propVal = str(c[2])
      if (propName === 'Reference') ref = propVal
      if (propName === 'Value') value = propVal
    } else if (head(c) === 'fp_text') {
      // Legacy KiCad 5/6 format: (fp_text reference "U1" ...)
      const kind = str(c[1])
      const val = str(c[2])
      if (kind === 'reference' && !ref) ref = val
      if (kind === 'value' && !value) value = val
    } else if (head(c) === 'reference') {
      // Even older: (reference "U1")
      if (!ref) ref = str(c[1])
    }
  }

  const pads: Pad[] = []
  for (const c of node) {
    if (!isList(c) || head(c) !== 'pad') continue
    const pad = parsePad(c, nets, at, at.angle)
    pads.push(pad)
  }

  const fp_lines: FpLine[] = []
  const fp_arcs: FpArc[] = []
  const fp_circles: FpCircle[] = []
  const fp_rects: FpRect[] = []

  for (const c of node) {
    if (!isList(c)) continue
    switch (head(c)) {
      case 'fp_line': fp_lines.push(parseFpLine(c, at, at.angle)); break
      case 'fp_arc': fp_arcs.push(parseFpArc(c, at, at.angle)); break
      case 'fp_circle': fp_circles.push(parseFpCircle(c, at, at.angle)); break
      case 'fp_rect': fp_rects.push(parseFpRect(c, at, at.angle)); break
    }
  }

  return { ref, value, lib_id, at, angle: at.angle, layer, pads, fp_lines, fp_arcs, fp_circles, fp_rects }
}

function rotatePoint(p: Point, cx: number, cy: number, angleDeg: number): Point {
  if (angleDeg === 0) return p
  const rad = (angleDeg * Math.PI) / 180
  const cos = Math.cos(rad); const sin = Math.sin(rad)
  const dx = p.x - cx; const dy = p.y - cy
  return { x: cx + dx * cos - dy * sin, y: cy + dx * sin + dy * cos }
}

function localToBoard(lx: number, ly: number, fpAt: Point, fpAngle: number): Point {
  const local = rotatePoint({ x: lx, y: ly }, 0, 0, fpAngle)
  return { x: fpAt.x + local.x, y: fpAt.y + local.y }
}

function parsePad(c: SNode[], nets: Map<string, string>, fpAt: Point, fpAngle: number): Pad {
  const padNum = str(c[1])
  const typeStr = str(c[2])
  const shapeStr = str(c[3])

  const atNode = findChild(c, 'at')
  const localAt = parseAt(atNode)
  const boardAt = localToBoard(localAt.x, localAt.y, fpAt, fpAngle)
  const totalAngle = fpAngle + localAt.angle

  const sizeNode = findChild(c, 'size')
  // Generated boards (the boot_gate demo, minimal to-code output) may omit
  // (size ...): default to a visible 1 mm pad rather than a 0x0 one the
  // renderer would silently skip.
  const size = sizeNode
    ? { w: num(sizeNode[1]), h: num(sizeNode[2]) }
    : { w: 1, h: 1 }

  const drillNode = findChild(c, 'drill')
  let drill: Pad['drill'] | undefined
  if (drillNode) {
    const isOval = str(drillNode[1]) === 'oval'
    if (isOval) {
      drill = { diameter: num(drillNode[2]), oval: true, dx: num(drillNode[3]), dy: num(drillNode[4]) }
    } else {
      drill = { diameter: num(drillNode[1]) }
    }
  }

  const netNode = findChild(c, 'net')
  const net = netNode ? str(netNode[1]) : undefined
  const netName = net ? (nets.get(net) ?? net) : undefined

  const type = typeStr as Pad['type']
  const shape = shapeStr as Pad['shape']

  return { number: padNum, type, shape, at: boardAt, angle: totalAngle, size, drill, net, netName }
}

function parseFpLine(c: SNode[], fpAt: Point, fpAngle: number): FpLine {
  const startNode = findChild(c, 'start')
  const endNode = findChild(c, 'end')
  const strokeNode = findChild(c, 'stroke')
  const widthNode = strokeNode ? findChild(strokeNode, 'width') : findChild(c, 'width')
  const layer = layerStr(findChild(c, 'layer'))
  const start = parseXY(startNode)
  const end = parseXY(endNode)
  return {
    start: localToBoard(start.x, start.y, fpAt, fpAngle),
    end: localToBoard(end.x, end.y, fpAt, fpAngle),
    layer,
    width: num(widthNode?.[1]) || (strokeNode ? num(findChild(strokeNode, 'width')?.[1]) : 0.12),
  }
}

function parseFpArc(c: SNode[], fpAt: Point, fpAngle: number): FpArc {
  const startNode = findChild(c, 'start')
  const midNode = findChild(c, 'mid')
  const endNode = findChild(c, 'end')
  const strokeNode = findChild(c, 'stroke')
  const widthNode = strokeNode ? findChild(strokeNode, 'width') : findChild(c, 'width')
  const layer = layerStr(findChild(c, 'layer'))
  const start = parseXY(startNode)
  const mid = parseXY(midNode)
  const end = parseXY(endNode)
  return {
    start: localToBoard(start.x, start.y, fpAt, fpAngle),
    mid: localToBoard(mid.x, mid.y, fpAt, fpAngle),
    end: localToBoard(end.x, end.y, fpAt, fpAngle),
    layer,
    width: num(widthNode?.[1]) || 0.12,
  }
}

function parseFpCircle(c: SNode[], fpAt: Point, fpAngle: number): FpCircle {
  const centerNode = findChild(c, 'center')
  const endNode = findChild(c, 'end')
  const strokeNode = findChild(c, 'stroke')
  const layer = layerStr(findChild(c, 'layer'))
  const center = parseXY(centerNode)
  const end = parseXY(endNode)
  return {
    center: localToBoard(center.x, center.y, fpAt, fpAngle),
    end: localToBoard(end.x, end.y, fpAt, fpAngle),
    layer,
    width: num(strokeNode ? findChild(strokeNode, 'width')?.[1] : findChild(c, 'width')?.[1]) || 0.12,
  }
}

function parseFpRect(c: SNode[], fpAt: Point, fpAngle: number): FpRect {
  const startNode = findChild(c, 'start')
  const endNode = findChild(c, 'end')
  const strokeNode = findChild(c, 'stroke')
  const layer = layerStr(findChild(c, 'layer'))
  const start = parseXY(startNode)
  const end = parseXY(endNode)
  return {
    start: localToBoard(start.x, start.y, fpAt, fpAngle),
    end: localToBoard(end.x, end.y, fpAt, fpAngle),
    layer,
    width: num(strokeNode ? findChild(strokeNode, 'width')?.[1] : findChild(c, 'width')?.[1]) || 0.12,
  }
}

function parseSegment(c: SNode[], nets: Map<string, string>): Segment | null {
  const startNode = findChild(c, 'start')
  const endNode = findChild(c, 'end')
  if (!startNode || !endNode) return null
  const layer = layerStr(findChild(c, 'layer'))
  const widthNode = findChild(c, 'width')
  const netNode = findChild(c, 'net')
  const net = netNode ? str(netNode[1]) : undefined
  return {
    start: parseXY(startNode),
    end: parseXY(endNode),
    layer,
    width: num(widthNode?.[1]) || 0.25,
    net,
    netName: net ? (nets.get(net) ?? net) : undefined,
  }
}

function parseArcTrack(c: SNode[], nets: Map<string, string>): Arc | null {
  const startNode = findChild(c, 'start')
  const midNode = findChild(c, 'mid')
  const endNode = findChild(c, 'end')
  if (!startNode || !endNode) return null
  const layer = layerStr(findChild(c, 'layer'))
  const widthNode = findChild(c, 'width')
  const netNode = findChild(c, 'net')
  const net = netNode ? str(netNode[1]) : undefined
  const mid = midNode ? parseXY(midNode) : {
    x: (num(startNode[1]) + num(endNode[1])) / 2,
    y: (num(startNode[2]) + num(endNode[2])) / 2,
  }
  return {
    start: parseXY(startNode),
    mid,
    end: parseXY(endNode),
    layer,
    width: num(widthNode?.[1]) || 0.25,
    net,
    netName: net ? (nets.get(net) ?? net) : undefined,
  }
}

function parseVia(c: SNode[], nets: Map<string, string>): Via | null {
  const atNode = findChild(c, 'at')
  if (!atNode) return null
  const sizeNode = findChild(c, 'size')
  const drillNode = findChild(c, 'drill')
  const layersNode = findChild(c, 'layers')
  const netNode = findChild(c, 'net')
  const net = netNode ? str(netNode[1]) : undefined
  const layers = layersNode
    ? layersNode.slice(1).map(l => str(l as SNode))
    : ['F.Cu', 'B.Cu']
  return {
    at: parseXY(atNode),
    size: num(sizeNode?.[1]) || 0.8,
    drill: num(drillNode?.[1]) || 0.4,
    layers,
    net,
    netName: net ? (nets.get(net) ?? net) : undefined,
  }
}

function parseGrLine(c: SNode[]): GrLine {
  const startNode = findChild(c, 'start')
  const endNode = findChild(c, 'end')
  const strokeNode = findChild(c, 'stroke')
  const layer = layerStr(findChild(c, 'layer'))
  const width = strokeNode ? num(findChild(strokeNode, 'width')?.[1]) : num(findChild(c, 'width')?.[1])
  return { start: parseXY(startNode), end: parseXY(endNode), layer, width: width || 0.05 }
}

function parseGrArc(c: SNode[]): GrArc {
  const startNode = findChild(c, 'start')
  const midNode = findChild(c, 'mid')
  const endNode = findChild(c, 'end')
  const strokeNode = findChild(c, 'stroke')
  const layer = layerStr(findChild(c, 'layer'))
  const width = strokeNode ? num(findChild(strokeNode, 'width')?.[1]) : num(findChild(c, 'width')?.[1])
  const mid = midNode ? parseXY(midNode) : { x: 0, y: 0 }
  return { start: parseXY(startNode), mid, end: parseXY(endNode), layer, width: width || 0.05 }
}

function parseGrCircle(c: SNode[]): GrCircle {
  const centerNode = findChild(c, 'center')
  const endNode = findChild(c, 'end')
  const strokeNode = findChild(c, 'stroke')
  const fillNode = findChild(c, 'fill')
  const layer = layerStr(findChild(c, 'layer'))
  const width = strokeNode ? num(findChild(strokeNode, 'width')?.[1]) : num(findChild(c, 'width')?.[1])
  const fill = fillNode ? str(findChild(fillNode, 'type')?.[1]) === 'solid' || str(fillNode[1]) === 'solid' : false
  return { center: parseXY(centerNode), end: parseXY(endNode), layer, width: width || 0.05, fill }
}

function parseGrRect(c: SNode[]): GrRect {
  const startNode = findChild(c, 'start')
  const endNode = findChild(c, 'end')
  const strokeNode = findChild(c, 'stroke')
  const fillNode = findChild(c, 'fill')
  const layer = layerStr(findChild(c, 'layer'))
  const width = strokeNode ? num(findChild(strokeNode, 'width')?.[1]) : num(findChild(c, 'width')?.[1])
  const fill = fillNode ? str(findChild(fillNode, 'type')?.[1]) === 'solid' || str(fillNode[1]) === 'solid' : false
  return { start: parseXY(startNode), end: parseXY(endNode), layer, width: width || 0.05, fill }
}

function parseGrPoly(c: SNode[]): GrPoly {
  const ptsNode = findChild(c, 'pts')
  const pts: Point[] = []
  if (ptsNode) {
    for (const xy of ptsNode) {
      if (isList(xy) && head(xy) === 'xy') {
        pts.push(parseXY(xy))
      }
    }
  }
  const strokeNode = findChild(c, 'stroke')
  const fillNode = findChild(c, 'fill')
  const layer = layerStr(findChild(c, 'layer'))
  const width = strokeNode ? num(findChild(strokeNode, 'width')?.[1]) : num(findChild(c, 'width')?.[1])
  const fill = fillNode ? str(findChild(fillNode, 'type')?.[1]) === 'solid' || str(fillNode[1]) === 'solid' : false
  return { pts, layer, width: width || 0.05, fill }
}

// ────────────────────── Bounds computation ────────────────────────────

function computeBounds(board: ParsedBoard): BoardBounds {
  const points: Point[] = []

  // Prefer Edge.Cuts for the board outline
  const edgePoints: Point[] = []
  for (const l of board.gr_lines) {
    if (l.layer === 'Edge.Cuts') { edgePoints.push(l.start, l.end) }
  }
  for (const a of board.gr_arcs) {
    if (a.layer === 'Edge.Cuts') { edgePoints.push(a.start, a.mid, a.end) }
  }
  for (const r of board.gr_rects) {
    if (r.layer === 'Edge.Cuts') { edgePoints.push(r.start, r.end) }
  }
  for (const fp of board.footprints) {
    for (const l of fp.fp_lines) {
      if (l.layer === 'Edge.Cuts') edgePoints.push(l.start, l.end)
    }
  }

  const use = edgePoints.length > 4 ? edgePoints : null

  if (use) {
    points.push(...use)
  } else {
    // Fall back to all geometry
    for (const s of board.segments) { points.push(s.start, s.end) }
    for (const fp of board.footprints) {
      points.push(fp.at)
      for (const p of fp.pads) points.push(p.at)
    }
    for (const l of board.gr_lines) { points.push(l.start, l.end) }
  }

  if (points.length === 0) {
    return { minX: 0, maxX: 100, minY: 0, maxY: 100, width: 100, height: 100, cx: 50, cy: 50 }
  }

  let minX = Infinity, maxX = -Infinity, minY = Infinity, maxY = -Infinity
  for (const p of points) {
    if (p.x < minX) minX = p.x
    if (p.x > maxX) maxX = p.x
    if (p.y < minY) minY = p.y
    if (p.y > maxY) maxY = p.y
  }

  // Add 5% padding
  const padX = (maxX - minX) * 0.05
  const padY = (maxY - minY) * 0.05
  minX -= padX; maxX += padX; minY -= padY; maxY += padY

  const width = maxX - minX
  const height = maxY - minY
  return { minX, maxX, minY, maxY, width, height, cx: (minX + maxX) / 2, cy: (minY + maxY) / 2 }
}

// ────────────────────── Net geometry index ────────────────────────────

/** Build a map from netName → all geometric segments/arcs/pads for overlays. */
export interface NetGeometry {
  segments: Segment[]
  arcs: Arc[]
  pads: (Pad & { fpRef: string })[]
}

export function buildNetIndex(board: ParsedBoard): Map<string, NetGeometry> {
  const idx = new Map<string, NetGeometry>()

  function get(name: string): NetGeometry {
    let g = idx.get(name)
    if (!g) { g = { segments: [], arcs: [], pads: [] }; idx.set(name, g) }
    return g
  }

  for (const s of board.segments) {
    if (s.netName) get(s.netName).segments.push(s)
  }
  for (const a of board.arcs) {
    if (a.netName) get(a.netName).arcs.push(a)
  }
  for (const fp of board.footprints) {
    for (const p of fp.pads) {
      if (p.netName) get(p.netName).pads.push({ ...p, fpRef: fp.ref })
    }
  }

  return idx
}
