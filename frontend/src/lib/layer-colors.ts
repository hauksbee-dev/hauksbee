// KiCad-inspired layer color palette, tuned for dark backgrounds.
// Colors are CSS strings used in canvas fillStyle/strokeStyle.

export interface LayerStyle {
  color: string
  /** For copper layers a slightly brighter glow color */
  glow?: string
  zIndex: number
  visible: boolean
}

const palette: Record<string, LayerStyle> = {
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

export function getLayerStyle(layer: string): LayerStyle {
  return palette[layer] ?? { color: '#666666', zIndex: 1, visible: true }
}

export function isCopperLayer(layer: string): boolean {
  return layer === 'F.Cu' || layer === 'B.Cu' || layer.includes('.Cu')
}

export const PAD_COLOR = '#c8a040'       // gold
export const PAD_GLOW = '#ffd060'
export const VIA_COLOR = '#a0a0a0'
export const VIA_DRILL_COLOR = '#1a1a2e'
