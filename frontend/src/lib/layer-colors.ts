// KiCad-inspired layer palettes, one per theme, as CSS strings for canvas
// fillStyle/strokeStyle. The dark palette is tuned for the navy instrument
// ground and is the original identity; its values must not drift (the dark
// theme is pixel stable). The light palette is a real re-tune, not an
// inversion: copper reads as ink on the light substrate.

import { isLightTheme } from './theme-tokens'

export interface LayerStyle {
  color: string
  /** For copper layers a slightly brighter (dark) or tighter (light) glow. */
  glow?: string
  visible: boolean
}

/** Every KiCad layer the renderer knows: [dark colour, dark glow, light
 *  colour, light glow, drawn by default]. Both themes share visibility.
 *  The light palette drops each layer to ~35-50% luminance so it holds at
 *  least ~3:1 against the #ece7dd substrate while keeping its KiCad hue
 *  family, and its glows are same-hue halos rather than blooms. */
const LAYERS: Record<string, [string, string | undefined, string, string | undefined, boolean]> = {
  // Copper: F.Cu red, B.Cu blue, inner gold/green/purple/teal so a 6-layer
  // board is still readable at a glance.
  'F.Cu': ['#c84040', '#ff6060', '#b32929', '#8f1f1f', true],
  'B.Cu': ['#4080c8', '#60a0ff', '#2a5db3', '#20488f', true],
  'In1.Cu': ['#c8a030', '#ffd060', '#9a7414', '#7a5c0f', true],
  'In2.Cu': ['#20b080', '#30e0a0', '#0f7d5c', '#0a6248', true],
  'In3.Cu': ['#b060c0', '#e080ff', '#8a3f9e', '#6e3280', true],
  'In4.Cu': ['#20a0c0', '#30c8e8', '#0f7a99', '#0a607a', true],
  // Silkscreen: white paint on a dark board becomes dark ink on a light one;
  // back silk keeps its slate-violet cast so front/back stay tellable apart.
  'F.SilkS': ['#c8c8c8', undefined, '#454b54', undefined, true],
  'B.SilkS': ['#9090c0', undefined, '#5b5f94', undefined, true],
  'F.Silkscreen': ['#c8c8c8', undefined, '#454b54', undefined, true],
  'B.Silkscreen': ['#9090c0', undefined, '#5b5f94', undefined, true],
  // Fab outlines stay the quietest drawn layer.
  'F.Fab': ['#506070', undefined, '#aab3bc', undefined, true],
  'B.Fab': ['#507090', undefined, '#a3b1c4', undefined, true],
  'F.CrtYd': ['#404040', undefined, '#c9c4b8', undefined, false],
  'B.CrtYd': ['#404040', undefined, '#c9c4b8', undefined, false],
  'F.Courtyard': ['#404040', undefined, '#c9c4b8', undefined, false],
  'B.Courtyard': ['#404040', undefined, '#c9c4b8', undefined, false],
  'F.Paste': ['#606060', undefined, '#b5afa2', undefined, false],
  'B.Paste': ['#606060', undefined, '#b5afa2', undefined, false],
  'F.Mask': ['#308040', undefined, '#7fae8d', undefined, false],
  'B.Mask': ['#306880', undefined, '#7fa2ae', undefined, false],
  // Board edge: goldenrod, dropped to ink weight on paper (outline, not shout).
  'Edge.Cuts': ['#e8c040', '#ffe060', '#8a6d0b', '#6e5709', true],
  'Dwgs.User': ['#888888', undefined, '#8a8a8a', undefined, true],
  'User.Drawings': ['#888888', undefined, '#8a8a8a', undefined, true],
  'Cmts.User': ['#606060', undefined, '#b0aca2', undefined, false],
  'User.Comments': ['#606060', undefined, '#b0aca2', undefined, false],
  'Eco1.User': ['#506050', undefined, '#9aa596', undefined, false],
  'Eco2.User': ['#505060', undefined, '#9a96a5', undefined, false],
  'Margin': ['#606050', undefined, '#a5a196', undefined, false],
}

export function getLayerStyle(layer: string): LayerStyle {
  const light = isLightTheme()
  const row = LAYERS[layer]
  if (!row) return { color: light ? '#8f8f8f' : '#666666', visible: true }
  return light ? { color: row[2], glow: row[3], visible: row[4] } : { color: row[0], glow: row[1], visible: row[4] }
}

export function isCopperLayer(layer: string): boolean {
  return layer === 'F.Cu' || layer === 'B.Cu' || layer.includes('.Cu')
}

// ── Semantic board colors (everything the renderer draws that is not a
//    KiCad layer). One object per theme; boardTheme() hands the renderer the
//    active one so no draw call ever branches on theme itself. ──────────────

interface BoardTheme {
  /** Canvas ground behind the board (matches the --instrument token). */
  bg: string
  /** Radial vignette stops: center (transparent) and edge. */
  vignette0: string
  vignette1: string
  /** Reference label text + its legibility halo. */
  label: string
  labelShadow: string
  /** Pads and vias. */
  pad: string
  padGlow: string
  via: string
  viaDrill: string
  /** Translucent veil over the static blit while a net is highlighted. */
  dimVeil: string
  /** Highlighted net: tracks/vias stroke + glow, pads fill + glow. */
  highlight: string
  highlightGlow: string
  highlightVia: string
  highlightPad: string
  highlightPadGlow: string
  /** The wide soft under-glow pulsing beneath a highlighted net. */
  netGlowStroke: string
  netGlowShadow: string
  /** Swatch for the Layers panel's "Activity overlay" row. */
  activity: string
  /** Voltage tint blend targets (positive rail warm, negative cool). */
  voltWarm: { r: number; g: number; b: number }
  voltCool: { r: number; g: number; b: number }
  voltWarmGlow: string
  voltCoolGlow: string
  /** Signal-flow particles. */
  particle: string
  particleGlow: string
  /** Running-MCU radial glow stops. */
  mcuGlow0: string
  mcuGlow1: string
  mcuGlow2: string
  /** Fault paint: 'r,g,b' fragments (alpha is animated per frame). */
  faultFillRGB: string
  faultStrokeRGB: string
  faultShadow: string
  faultPadGlow: string
  /** Fill for a faulted part's pads; pulse in [0,1] animates the alarm. */
  faultPad: (pulse: number) => string
  /** Probe tooltip. */
  probeBg: string
  probeBorder: string
  probeLabel: string
  probeValue: string
  /** "Show on board" marker ring + label chip. */
  markerStroke: string
  markerShadow: string
  markerLabelBg: string
  markerLabelBorder: string
  markerLabelText: string
}

const darkBoard: BoardTheme = {
  bg: '#020617',
  vignette0: 'rgba(10,18,40,0)',
  vignette1: 'rgba(0,0,0,0.55)',
  label: '#cdd6e4',
  labelShadow: 'rgba(0,0,0,0.9)',
  pad: '#c8a040',
  padGlow: '#ffd060',
  via: '#a0a0a0',
  viaDrill: '#1a1a2e',
  dimVeil: 'rgba(2,6,23,0.45)',
  highlight: '#ffffff',
  highlightGlow: '#80c0ff',
  highlightVia: '#ffffffcc',
  highlightPad: '#ffdd44',
  highlightPadGlow: '#ffe080',
  netGlowStroke: 'rgba(100,200,255,0.35)',
  netGlowShadow: '#40a0ff',
  activity: '#ffb347',
  voltWarm: { r: 0xff, g: 0xc0, b: 0x40 },
  voltCool: { r: 0x60, g: 0xa0, b: 0xff },
  voltWarmGlow: '#ffb347cc',
  voltCoolGlow: '#60a0ffcc',
  particle: '#60ff80',
  particleGlow: '#00ff40',
  mcuGlow0: 'rgba(34,211,238,0.18)',
  mcuGlow1: 'rgba(34,211,238,0.06)',
  mcuGlow2: 'rgba(34,211,238,0)',
  faultFillRGB: '248,60,50',
  faultStrokeRGB: '248,71,71',
  faultShadow: '#ff2222',
  faultPadGlow: '#ff2222',
  faultPad: (pulse: number) => `rgba(248,${Math.round(50 + pulse * 50)},50,1)`,
  probeBg: 'rgba(15, 23, 42, 0.92)',
  probeBorder: '#3b82f6',
  probeLabel: '#94a3b8',
  probeValue: '#60a5fa',
  markerStroke: '#f87171',
  markerShadow: '#ef4444',
  markerLabelBg: 'rgba(15, 23, 42, 0.92)',
  markerLabelBorder: '#ef4444',
  markerLabelText: '#fca5a5',
}

// Light board: a warm paper substrate (a light board-render, not white).
// "Bright" flips meaning here: emphasis is carried by darker, more saturated
// ink, and glows become tight same-hue halos. The fault red is the one thing
// that keeps near-full chroma; it must stay the loudest mark on the surface.
const lightBoard: BoardTheme = {
  bg: '#ece7dd',
  vignette0: 'rgba(120,105,80,0)',
  vignette1: 'rgba(96,84,60,0.20)',
  label: '#3d4652',
  labelShadow: 'rgba(255,255,255,0.9)',
  // Bare gold on paper washes out; bronze keeps the "plated" read.
  pad: '#a97b1e',
  padGlow: '#8a6206',
  via: '#8b8f96',
  // The drill is a hole: slightly deeper than the substrate, never dark.
  viaDrill: '#d8d1c2',
  dimVeil: 'rgba(240,236,228,0.55)',
  // Highlight = the strongest ink on the page, mirroring dark's pure-white
  // core + blue bloom. The core must be unlike EVERY copper hue or a
  // highlighted net on back copper vanishes into ordinary B.Cu: near-black
  // sits ~114 rgb-units from the closest layer where a saturated blue sat
  // ~42 (B.Cu is itself blue). The bloom stays blue so the net still reads
  // as energised rather than merely outlined.
  highlight: '#101828',
  highlightGlow: '#1d4ed8',
  highlightVia: '#101828cc',
  // Emphasis on paper means darker, not brighter: a highlighted pad goes
  // deep bronze, well clear of the ordinary pad gold it has to out-shout.
  highlightPad: '#7c4304',
  highlightPadGlow: '#b45309',
  netGlowStroke: 'rgba(29,78,216,0.30)',
  netGlowShadow: '#1d4ed8',
  activity: '#d97706',
  voltWarm: { r: 0xd9, g: 0x77, b: 0x06 },
  voltCool: { r: 0x25, g: 0x63, b: 0xeb },
  voltWarmGlow: '#d97706aa',
  voltCoolGlow: '#2563ebaa',
  particle: '#047857',
  particleGlow: '#059669',
  mcuGlow0: 'rgba(8,145,178,0.22)',
  mcuGlow1: 'rgba(8,145,178,0.08)',
  mcuGlow2: 'rgba(8,145,178,0)',
  faultFillRGB: '220,38,38',
  faultStrokeRGB: '185,28,28',
  faultShadow: '#dc2626',
  faultPadGlow: '#b91c1c',
  faultPad: (pulse: number) => `rgba(205,${Math.round(24 + pulse * 40)},24,1)`,
  probeBg: 'rgba(255, 255, 255, 0.94)',
  probeBorder: '#1d4ed8',
  probeLabel: '#57626f',
  probeValue: '#1d4ed8',
  markerStroke: '#dc2626',
  markerShadow: '#b91c1c',
  markerLabelBg: 'rgba(255, 255, 255, 0.94)',
  markerLabelBorder: '#dc2626',
  markerLabelText: '#b91c1c',
}

/** The active semantic board palette. Cheap enough to call per draw. */
export function boardTheme(): BoardTheme {
  return isLightTheme() ? lightBoard : darkBoard
}
