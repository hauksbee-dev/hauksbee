import type { PowerSupplyWire, SimFault } from '../../types/protocol'
import type { WebReport } from '../../types/report'
import { downloadText } from '../../lib/report-export'
import { APP_VERSION } from '../../lib/version'
import { supplyLabel } from './session-seed'

// Live Sim's exports: what a reviewer cites from a live session. Every file
// starts with the same provenance (sim time, supplies, firmware and board
// hashes, engine and model versions), so a trace is never separated from the
// conditions that produced it.

export interface SimProvenance {
  board: string | null
  boardSha256: string | null
  firmware: string | null
  firmwareSha256: string | null
  engineVersion: string | null
  simTimeS: number
  supplies: Record<string, PowerSupplyWire>
  /** "R1 = model_id (layer, origin)" for every part that bound a model. */
  models: string[]
  exportedAt: string
}

export function provenanceFor(
  report: WebReport | null | undefined,
  board: string | null,
  engineVersion: string | null,
  simTimeS: number,
  supplies: Record<string, PowerSupplyWire>,
): SimProvenance {
  const inv = report?.inventory ?? []
  const fw = inv.find(a => a.role === 'firmware')
  const bd = inv.find(a => a.role === 'layout')
  const packs = inv.filter(a => a.role === 'model_pack')
  const models = (report?.model_coverage?.components ?? [])
    .filter(c => c.model_id)
    .map(c => `${c.reference} = ${c.model_id} (${c.source.layer}, ${c.source.origin})`)
  for (const pk of packs) models.unshift(`pack ${pk.path}${pk.sha256 ? ` sha256 ${pk.sha256}` : ''}`)
  return {
    board: board || report?.file_name || null,
    boardSha256: bd?.sha256 ?? null,
    firmware: fw ? fw.path.split(/[\\/]/).pop() ?? fw.path : null,
    firmwareSha256: fw?.sha256 ?? null,
    engineVersion,
    simTimeS,
    supplies,
    models,
    exportedAt: new Date().toISOString(),
  }
}

/** The provenance as `key: value` lines (the header of the text exports). */
export function provenanceLines(p: SimProvenance): string[] {
  return [
    `hauksbee live simulation export`,
    `board: ${p.board ?? 'unknown'}${p.boardSha256 ? ` sha256 ${p.boardSha256}` : ''}`,
    `firmware: ${p.firmware ?? 'none'}${p.firmwareSha256 ? ` sha256 ${p.firmwareSha256}` : ''}`,
    `sim time: ${p.simTimeS.toFixed(6)} s`,
    ...Object.entries(p.supplies).map(([net, w]) => `supply ${net}: ${supplyLabel(w)}`),
    `engine: hauksbee ${p.engineVersion ?? 'unknown'} (web app ${APP_VERSION})`,
    ...p.models.map(m => `model: ${m}`),
    `exported: ${p.exportedAt}`,
  ]
}

/** RFC-4180 field escape, as the CLI's `csv_escape`. */
const csvField = (s: string) => (/[",\n\r]/.test(s) ? `"${s.replace(/"/g, '""')}"` : s)

/** The scope as `hauksbee run --probe-csv` writes it (`time_s,<net>...`, six
 *  decimals), under `#` provenance lines. Rows are the union of the sample
 *  times; a net with no sample at a row's time leaves its cell empty. */
export function scopeCsv(p: SimProvenance, series: { net: string; points: { t: number; v: number }[] }[]): string {
  const times = [...new Set(series.flatMap(s => s.points.map(pt => pt.t)))].sort((a, b) => a - b)
  const at = series.map(s => new Map(s.points.map(pt => [pt.t, pt.v])))
  const rows = times.map(t => [t.toFixed(6), ...at.map(m => {
    const v = m.get(t)
    return v === undefined ? '' : v.toFixed(6)
  })].join(','))
  return [
    ...provenanceLines(p).map(l => `# ${l}`),
    ['time_s', ...series.map(s => csvField(s.net))].join(','),
    ...rows,
  ].join('\n') + '\n'
}

export interface SerialLine {
  t: number
  mcu: string
  dir: 'rx' | 'tx'
  text: string
}

export function serialTxt(p: SimProvenance, lines: SerialLine[], analysis: { seconds: number; uart: string } | null): string {
  const out = [...provenanceLines(p).map(l => `# ${l}`)]
  if (analysis && analysis.uart) {
    out.push(`# --- analysis (static co-sim, ${analysis.seconds} s) ---`, analysis.uart.replace(/\n$/, ''))
    out.push('# --- live session ---')
  }
  for (const l of lines) out.push(`[t=${l.t.toFixed(6)} s ${l.mcu} ${l.dir}] ${l.text.replace(/\n$/, '')}`)
  return out.join('\n') + '\n'
}

export function faultsJson(p: SimProvenance, faults: SimFault[], unitOf: (kind: string) => string): string {
  return `${JSON.stringify({
    provenance: p,
    faults: faults.map(f => ({
      part: f.component, kind: f.kind, value: f.value, rating: f.limit, unit: unitOf(f.kind),
      sim_time_s: f.t, destroyed: !!f.destroyed,
    })),
  }, null, 2)}\n`
}

export { downloadText }

/** Save a canvas as a PNG download. */
export function downloadCanvas(canvas: HTMLCanvasElement, fileName: string) {
  canvas.toBlob(blob => {
    if (!blob) return
    const url = URL.createObjectURL(blob)
    const a = document.createElement('a')
    a.href = url
    a.download = fileName
    a.rel = 'noopener'
    a.click()
    setTimeout(() => URL.revokeObjectURL(url), 10_000)
  }, 'image/png')
}
