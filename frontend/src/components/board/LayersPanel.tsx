import { useEffect, useMemo, useRef, useState } from 'react'
import type { ParsedBoard } from '../../lib/kicad-parser'
import { boardTheme, getLayerStyle } from '../../lib/layer-colors'
import { LayersIcon } from '../Icons'
import { SwitchTrack } from '../ui'

// The map's layer controls: which of THIS board's layers are drawn, the
// pads/labels/activity switches, and a net to highlight.

/** Friendly display names for KiCad layer ids. */
const LAYER_LABELS: Record<string, string> = {
  'F.Cu': 'Copper · front',
  'B.Cu': 'Copper · back',
  'In1.Cu': 'Copper · inner 1',
  'In2.Cu': 'Copper · inner 2',
  'In3.Cu': 'Copper · inner 3',
  'In4.Cu': 'Copper · inner 4',
  'F.SilkS': 'Silkscreen · front',
  'F.Silkscreen': 'Silkscreen · front',
  'B.SilkS': 'Silkscreen · back',
  'B.Silkscreen': 'Silkscreen · back',
  'F.Fab': 'Fab outline · front',
  'B.Fab': 'Fab outline · back',
  'Edge.Cuts': 'Board edge',
  'Dwgs.User': 'Drawings',
  'User.Drawings': 'Drawings',
}

/** Only layers the renderer would draw by default (palette-visible), in a
 *  stable copper-first order. */
const LAYER_ORDER = [
  'F.Cu', 'In1.Cu', 'In2.Cu', 'In3.Cu', 'In4.Cu', 'B.Cu',
  'F.SilkS', 'F.Silkscreen', 'B.SilkS', 'B.Silkscreen',
  'F.Fab', 'B.Fab', 'Edge.Cuts', 'Dwgs.User', 'User.Drawings',
]

/** The panel's rows, derived from what the parsed board actually contains:
 *  real copper/silk/fab layers only, never a fixed template. */
function layersPresent(board: ParsedBoard): string[] {
  const found = new Set<string>()
  const add = (items: readonly { layer: string }[]) => {
    for (const item of items) found.add(item.layer)
  }
  add(board.segments)
  add(board.arcs)
  add(board.gr_lines)
  add(board.gr_arcs)
  add(board.gr_circles)
  add(board.gr_rects)
  add(board.gr_polys)
  for (const fp of board.footprints) {
    add(fp.fp_lines)
    add(fp.fp_arcs)
    add(fp.fp_circles)
    add(fp.fp_rects)
  }
  return LAYER_ORDER.filter(l => found.has(l) && getLayerStyle(l).visible)
}

/** One row in the layers panel: swatch, name, and a real switch. */
function LayerRow({ label, swatch, on, onToggle }: {
  label: string
  swatch?: string
  on: boolean
  onToggle: () => void
}) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={on}
      onClick={onToggle}
      className="hb-press flex items-center gap-2 w-full text-left cursor-pointer"
      style={{
        background: 'none', border: 'none', padding: '7px 10px', borderRadius: 7,
        color: on ? 'var(--silk)' : 'var(--silk-faint)', fontSize: 12,
      }}
    >
      <span
        aria-hidden
        style={{
          width: 10, height: 10, borderRadius: 3, flexShrink: 0,
          background: swatch ?? 'var(--silk-faint)',
          opacity: on ? 1 : 0.25,
          outline: '1px solid var(--image-outline)',
          transition: 'opacity 0.15s',
        }}
      />
      <span className="flex-1 truncate">{label}</span>
      <SwitchTrack on={on} size={26} />
    </button>
  )
}

/** What the panel controls, owned by the viewer so the render loop can read it. */
export interface LayerControls {
  hiddenLayers: Set<string>
  setHiddenLayers: React.Dispatch<React.SetStateAction<Set<string>>>
  showPads: boolean
  setShowPads: React.Dispatch<React.SetStateAction<boolean>>
  showLabels: boolean
  setShowLabels: React.Dispatch<React.SetStateAction<boolean>>
  showActivity: boolean
  setShowActivity: React.Dispatch<React.SetStateAction<boolean>>
}

export function useLayerControls(): LayerControls {
  const [hiddenLayers, setHiddenLayers] = useState<Set<string>>(new Set())
  const [showPads, setShowPads] = useState(true)
  const [showLabels, setShowLabels] = useState(true)
  const [showActivity, setShowActivity] = useState(true)
  return {
    hiddenLayers, setHiddenLayers,
    showPads, setShowPads,
    showLabels, setShowLabels,
    showActivity, setShowActivity,
  }
}

/** The Layers button and the panel it opens. */
export function LayersControl({ board, controls, pickerNets, selectedNet, onNetClick, buttonStyle }: {
  board: ParsedBoard
  controls: LayerControls
  pickerNets: string[]
  selectedNet?: string | null
  onNetClick?: (net: string | null) => void
  /** The viewer's shared toolbar-button style, keyed on the open state. */
  buttonStyle: (active: boolean) => React.CSSProperties
}) {
  const [open, setOpen] = useState(false)
  const wrap = useRef<HTMLDivElement>(null)
  const trigger = useRef<HTMLButtonElement>(null)
  const boardLayers = useMemo(() => layersPresent(board), [board])
  const {
    hiddenLayers, setHiddenLayers, showPads, setShowPads,
    showLabels, setShowLabels, showActivity, setShowActivity,
  } = controls

  // Same dismissal as the export menu and the session switcher: an outside
  // click or Escape. The panel covers a third of the map, so re-finding the one
  // button that closes it is not an acceptable way out. Escape hands focus back
  // to the trigger, because a keyboard reader who dismissed the panel has to
  // land somewhere, and the control they just used is the only sane place.
  useEffect(() => {
    if (!open) return
    const onDown = (e: MouseEvent) => {
      if (!wrap.current?.contains(e.target as Node)) setOpen(false)
    }
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== 'Escape') return
      // The expanded view listens for Escape on `window`, which bubbles LAST,
      // after `document`. Stopping propagation here is what makes the panel the
      // innermost dismissible surface: one Escape closes the panel and leaves
      // the map expanded, a second collapses it. A guard on the other effect
      // cannot do this, because within a single keydown React has not yet
      // processed the state change and both listeners are still attached.
      e.stopPropagation()
      setOpen(false)
      trigger.current?.focus()
    }
    document.addEventListener('mousedown', onDown)
    document.addEventListener('keydown', onKey)
    return () => {
      document.removeEventListener('mousedown', onDown)
      document.removeEventListener('keydown', onKey)
    }
  }, [open])

  return (
    <div ref={wrap} className="flex flex-col items-end gap-2 ml-auto pointer-events-auto">
      <button
        type="button"
        ref={trigger}
        data-testid="layers-toggle"
        onClick={() => setOpen(o => !o)}
        aria-expanded={open}
        aria-haspopup="dialog"
        title={open ? 'Close the layer controls (Esc)' : 'Show and hide board layers'}
        className="hb-press rounded-lg"
        style={{
          ...buttonStyle(open),
          border: '1px solid var(--hairline)',
          background: open ? 'var(--copper-tint-strong)' : 'color-mix(in srgb, var(--surface) 88%, transparent)',
          backdropFilter: 'blur(6px)',
          boxShadow: 'var(--shadow-card)',
        }}
      >
        <LayersIcon size={13} /> Layers
      </button>

      {open && (
        <div
          data-testid="layers-panel"
          className="hb-card view-enter overflow-y-auto"
          style={{ width: 228, maxHeight: 'calc(100% - 8px)', padding: 6, boxShadow: 'var(--shadow-pop)' }}
        >
          {boardLayers.map(layer => (
            <LayerRow
              key={layer}
              label={LAYER_LABELS[layer] ?? layer}
              swatch={getLayerStyle(layer).color}
              on={!hiddenLayers.has(layer)}
              onToggle={() => setHiddenLayers(prev => {
                const next = new Set(prev)
                if (next.has(layer)) next.delete(layer)
                else next.add(layer)
                return next
              })}
            />
          ))}
          <div style={{ height: 1, background: 'var(--rule)', margin: '5px 8px' }} />
          <LayerRow label="Pads" swatch={boardTheme().pad} on={showPads} onToggle={() => setShowPads(v => !v)} />
          <LayerRow label="Reference labels" on={showLabels} onToggle={() => setShowLabels(v => !v)} />
          <LayerRow
            label="Activity overlay"
            swatch={boardTheme().activity}
            on={showActivity}
            onToggle={() => setShowActivity(v => !v)}
          />
          {pickerNets.length > 0 && onNetClick && (
            <div style={{ padding: '7px 8px 5px' }}>
              <label className="block text-[10px] font-bold tracking-[0.1em] mb-1" style={{ color: 'var(--silk-faint)' }}>
                HIGHLIGHT A NET
              </label>
              <input
                className="hb-input w-full"
                list="viewer-net-options"
                placeholder="type a net name"
                value={selectedNet ?? ''}
                onChange={e => {
                  const v = e.target.value
                  if (v === '') onNetClick(null)
                  else if (pickerNets.includes(v)) onNetClick(v)
                }}
              />
              <datalist id="viewer-net-options">
                {pickerNets.map(n => <option key={n} value={n} />)}
              </datalist>
            </div>
          )}
        </div>
      )}
    </div>
  )
}
