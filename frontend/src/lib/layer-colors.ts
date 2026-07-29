// KiCad-inspired layer palettes, one per theme. Colors are CSS strings used
// in canvas fillStyle/strokeStyle.
//
// The dark palette is tuned for the navy instrument ground and is the
// original identity; its values must not drift (the dark theme is pixel
// stable). The light palette is a real re-tune, not an inversion: each layer
// keeps its KiCad hue family (F.Cu red, B.Cu blue, inner gold/green/purple/
// cyan) but drops luminance so copper reads as ink on the light substrate,
// and glows become slightly saturated halos rather than bright blooms.

import { isLightTheme } from './theme-tokens'

export interface LayerStyle {
  color: string
  /** For copper layers a slightly brighter glow color */
  glow?: string
  zIndex: number
  visible: boolean
}

const darkPalette: Record<string, LayerStyle> = {
  // Copper
  'F.Cu':    { color: '#c84040', glow: '#ff6060', zIndex: 40, visible: true },
  'B.Cu':    { color: '#4080c8', glow: '#60a0ff', zIndex: 38, visible: true },
  'In1.Cu':  { color: '#c8a030', glow: '#ffd060', zIndex: 37, visible: true },
  'In2.Cu':  { color: '#20b080', glow: '#30e0a0', zIndex: 36, visible: true },
  'In3.Cu':  { color: '#b060c0', glow: '#e080ff', zIndex: 35, visible: true },
  'In4.Cu':  { color: '#20a0c0', glow: '#30c8e8', zIndex: 34, visible: true },

  // Silkscreen
  'F.SilkS':  { color: '#c8c8c8', zIndex: 50, visible: true },
  'B.SilkS':  { color: '#9090c0', zIndex: 48, visible: true },
  'F.Silkscreen': { color: '#c8c8c8', zIndex: 50, visible: true },
  'B.Silkscreen': { color: '#9090c0', zIndex: 48, visible: true },

  // Fab (use faint, mostly for outline guides)
  'F.Fab':   { color: '#506070', zIndex: 20, visible: true },
  'B.Fab':   { color: '#507090', zIndex: 18, visible: true },

  // Courtyard
  'F.CrtYd': { color: '#404040', zIndex: 10, visible: false },
  'B.CrtYd': { color: '#404040', zIndex:  9, visible: false },
  'F.Courtyard': { color: '#404040', zIndex: 10, visible: false },
  'B.Courtyard': { color: '#404040', zIndex:  9, visible: false },

  // Paste / Mask, not usually visible
  'F.Paste': { color: '#606060', zIndex:  5, visible: false },
  'B.Paste': { color: '#606060', zIndex:  4, visible: false },
  'F.Mask':  { color: '#308040', zIndex:  3, visible: false },
  'B.Mask':  { color: '#306880', zIndex:  2, visible: false },

  // Board edge
  'Edge.Cuts': { color: '#e8c040', glow: '#ffe060', zIndex: 60, visible: true },

  // User/drawings
  'Dwgs.User':  { color: '#888888', zIndex: 15, visible: true },
  'User.Drawings': { color: '#888888', zIndex: 15, visible: true },
  'Cmts.User':  { color: '#606060', zIndex: 14, visible: false },
  'User.Comments': { color: '#606060', zIndex: 14, visible: false },
  'Eco1.User':  { color: '#506050', zIndex: 12, visible: false },
  'Eco2.User':  { color: '#505060', zIndex: 11, visible: false },
  'Margin':     { color: '#606050', zIndex:  8, visible: false },
}

// Light palette: same layers, same z-order and visibility, colors dropped to
// ~35-50% luminance so every layer holds at least ~3:1 against the #ece7dd
// substrate. Inner layers stay four clearly separate hues (gold, green,
// purple, teal) so a 6-layer board is still readable at a glance.
const lightPalette: Record<string, LayerStyle> = {
  'F.Cu':    { color: '#b32929', glow: '#8f1f1f', zIndex: 40, visible: true },
  'B.Cu':    { color: '#2a5db3', glow: '#20488f', zIndex: 38, visible: true },
  'In1.Cu':  { color: '#9a7414', glow: '#7a5c0f', zIndex: 37, visible: true },
  'In2.Cu':  { color: '#0f7d5c', glow: '#0a6248', zIndex: 36, visible: true },
  'In3.Cu':  { color: '#8a3f9e', glow: '#6e3280', zIndex: 35, visible: true },
  'In4.Cu':  { color: '#0f7a99', glow: '#0a607a', zIndex: 34, visible: true },

  // Silkscreen: white paint on a dark board becomes dark ink on a light one.
  // Back silk keeps its slate-violet cast so front/back stay tellable apart.
  'F.SilkS':  { color: '#454b54', zIndex: 50, visible: true },
  'B.SilkS':  { color: '#5b5f94', zIndex: 48, visible: true },
  'F.Silkscreen': { color: '#454b54', zIndex: 50, visible: true },
  'B.Silkscreen': { color: '#5b5f94', zIndex: 48, visible: true },

  // Fab outlines stay the quietest drawn layer: barely-there cool grays.
  'F.Fab':   { color: '#aab3bc', zIndex: 20, visible: true },
  'B.Fab':   { color: '#a3b1c4', zIndex: 18, visible: true },

  'F.CrtYd': { color: '#c9c4b8', zIndex: 10, visible: false },
  'B.CrtYd': { color: '#c9c4b8', zIndex:  9, visible: false },
  'F.Courtyard': { color: '#c9c4b8', zIndex: 10, visible: false },
  'B.Courtyard': { color: '#c9c4b8', zIndex:  9, visible: false },

  'F.Paste': { color: '#b5afa2', zIndex:  5, visible: false },
  'B.Paste': { color: '#b5afa2', zIndex:  4, visible: false },
  'F.Mask':  { color: '#7fae8d', zIndex:  3, visible: false },
  'B.Mask':  { color: '#7fa2ae', zIndex:  2, visible: false },

  // Board edge: goldenrod dropped to ink weight; it must outline, not shout.
  'Edge.Cuts': { color: '#8a6d0b', glow: '#6e5709', zIndex: 60, visible: true },

  'Dwgs.User':  { color: '#8a8a8a', zIndex: 15, visible: true },
  'User.Drawings': { color: '#8a8a8a', zIndex: 15, visible: true },
  'Cmts.User':  { color: '#b0aca2', zIndex: 14, visible: false },
  'User.Comments': { color: '#b0aca2', zIndex: 14, visible: false },
  'Eco1.User':  { color: '#9aa596', zIndex: 12, visible: false },
  'Eco2.User':  { color: '#9a96a5', zIndex: 11, visible: false },
  'Margin':     { color: '#a5a196', zIndex:  8, visible: false },
}

export function getLayerStyle(layer: string): LayerStyle {
  const palette = isLightTheme() ? lightPalette : darkPalette
  return palette[layer] ?? { color: isLightTheme() ? '#8f8f8f' : '#666666', zIndex: 1, visible: true }
}

export function isCopperLayer(layer: string): boolean {
  return layer === 'F.Cu' || layer === 'B.Cu' || layer.includes('.Cu')
}

// ── Semantic board colors (everything the renderer draws that is not a
//    KiCad layer). One object per theme; boardTheme() hands the renderer the
//    active one so no draw call ever branches on theme itself. ──────────────

export interface BoardTheme {
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
