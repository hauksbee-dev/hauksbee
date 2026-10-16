import { drawScope } from './scope-draw'
import type { ScopePlot } from './scope-draw'

// A finished plot as a file. Both formats come out of the SAME painter the
// screen uses (`drawScope`): the PNG is its canvas, and the SVG is its drawing
// calls recorded and replayed as vector elements, so an exported figure cannot
// differ from what was on screen. The glow passes and scanlines are screen
// effects and are left out of the vector copy.

export const EXPORT_W = 960
export const EXPORT_H = 400

const esc = (s: string) => s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')

/** The plot as a PNG, drawn at a fixed readable size. */
export function plotPng(plot: ScopePlot): Promise<Blob | null> {
  const canvas = document.createElement('canvas')
  canvas.width = EXPORT_W
  canvas.height = EXPORT_H
  const ctx = canvas.getContext('2d')
  if (!ctx) return Promise.resolve(null)
  drawScope(ctx, EXPORT_W, EXPORT_H, plot)
  return new Promise(resolve => canvas.toBlob(resolve, 'image/png'))
}

/** The plot as a standalone SVG document. */
export function plotSvg(plot: ScopePlot): string {
  const measure = document.createElement('canvas').getContext('2d')
  const out: string[] = []
  let path = ''
  // Only the state drawScope sets is kept; the methods read it back.
  const rec = {
    fillStyle: '#000', strokeStyle: '#000', lineWidth: 1, font: '9px monospace', textAlign: 'left',
    shadowColor: '', shadowBlur: 0, dash: '',
    clearRect() { /* the SVG starts empty */ },
    fillRect(x: number, y: number, w: number, h: number) {
      if (h <= 1) return // CRT scanlines
      out.push(`<rect x="${x}" y="${y}" width="${w}" height="${h}" fill="${rec.fillStyle}"/>`)
    },
    fillText(t: string, x: number, y: number) {
      const anchor = rec.textAlign === 'center' ? 'middle' : rec.textAlign === 'right' ? 'end' : 'start'
      const size = /(\d+(?:\.\d+)?)px/.exec(rec.font)?.[1] ?? '9'
      const bold = rec.font.includes('bold') ? ' font-weight="bold"' : ''
      out.push(`<text x="${x.toFixed(1)}" y="${y.toFixed(1)}" text-anchor="${anchor}" fill="${rec.fillStyle}" font-family="monospace" font-size="${size}"${bold}>${esc(t)}</text>`)
    },
    setLineDash(d: number[]) { rec.dash = d.join(' ') },
    beginPath() { path = '' },
    moveTo(x: number, y: number) { path += `M${x.toFixed(1)} ${y.toFixed(1)}` },
    lineTo(x: number, y: number) { path += `L${x.toFixed(1)} ${y.toFixed(1)}` },
    stroke() {
      if (/^#[0-9a-f]{8}$/i.test(rec.strokeStyle)) return // a translucent glow pass
      const dash = rec.dash ? ` stroke-dasharray="${rec.dash}"` : ''
      out.push(`<path d="${path}" fill="none" stroke="${rec.strokeStyle}" stroke-width="${rec.lineWidth}"${dash}/>`)
    },
    measureText(t: string) {
      if (!measure) return { width: t.length * 5.5 }
      measure.font = rec.font
      return measure.measureText(t)
    },
  }
  drawScope(rec as unknown as CanvasRenderingContext2D, EXPORT_W, EXPORT_H, plot)
  return `<svg xmlns="http://www.w3.org/2000/svg" width="${EXPORT_W}" height="${EXPORT_H}" viewBox="0 0 ${EXPORT_W} ${EXPORT_H}">${out.join('')}</svg>`
}
