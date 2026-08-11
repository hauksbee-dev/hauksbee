// The report, as a file you can send someone.
//
// Two shapes, one source. `reportJson` is the engine's own JSON, untouched, for
// anything that wants to parse it (the shape `/api/analyze` returns, and what
// `hauksbee run --json` writes). `buildReportHtml` is the same content as a
// single self-contained page: no stylesheet request, no font request, no script,
// nothing that needs this server to still exist. That constraint is the whole
// point of the feature. A "report export" that renders as unstyled text in the
// recipient's mail client, or that quietly needs a CDN, is not a report.
//
// The palette is not re-declared here. It is READ off the live document at
// export time (`--surface`, `--err-border`, ...) and inlined as literal values,
// so the file the user downloads is the theme they were looking at, and this
// module cannot drift from index.css the way a second copy of the tokens would.

import type { WebReport, WebSection } from '../types/report'
import { groupFindings } from './findings'
import { summarizeEvidence } from './evidence'
import { refusalLines } from './refusal-contract'
import { fallbackWindowLine, timingCoverageLine, uncoveredTimingRefusals } from './cosim-coverage'

export interface ReportExportInput {
  report: WebReport
  /** The board's display name in this session (the uploaded file's name). */
  boardLabel: string | null
  firmwareName: string | null
  /** Client clock when the report landed. */
  analyzedAt: number | null
  /** The hauksbee that produced it, from `/api/startup`. */
  engineVersion: string | null
  /** The web build's own version, for when the server did not name one. */
  appVersion: string
  /** The composed spec, as the Checks pane last had it. */
  spec: { toml: string; fileName: string } | null
  /** Last run's counts, when a current run existed. */
  checks: { passed: number; failed: number; invalid: number } | null
  /** The saved session this report belongs to, when it has a name. */
  sessionName: string | null
  /** True when the report was restored from storage rather than just run: the
   *  file behind it is not in hand, and the export says so. */
  restored: boolean
}

/** The engine's JSON, pretty-printed. Byte-identical in shape to what
 *  `/api/analyze` returned; nothing here re-derives or reorders it. */
export function reportJson(report: WebReport): string {
  return `${JSON.stringify(report, null, 2)}\n`
}

/** Hand a string to the browser as a download. One implementation, because
 *  three call sites had three subtly different ones and only two of them
 *  revoked the object URL. */
export function downloadText(fileName: string, contents: string, mime: string) {
  const url = URL.createObjectURL(new Blob([contents], { type: mime }))
  const a = document.createElement('a')
  a.href = url
  a.download = fileName
  a.rel = 'noopener'
  a.click()
  // Revoked on the next task, not synchronously: Safari has not started
  // reading the blob when `click()` returns, and an immediately revoked URL
  // downloads an empty file there.
  setTimeout(() => URL.revokeObjectURL(url), 10_000)
}

/** The tokens the exported page uses. Read from the live root element so the
 *  file matches the theme on screen. */
const TOKENS = [
  'canvas', 'surface', 'surface-2', 'hairline', 'rule',
  'silk', 'silk-dim', 'silk-faint',
  'copper', 'copper-hi', 'copper-deep', 'copper-tint',
  'ok', 'ok-bg', 'ok-border', 'err', 'err-strong', 'err-bg', 'err-border',
  'warn', 'warn-strong', 'warn-bg', 'warn-border', 'note', 'note-accent',
  'instrument', 'instrument-edge', 'instrument-text', 'code-bg',
  'font-sans', 'font-mono',
] as const

/** Fallbacks, used only when this runs somewhere without the app's stylesheet
 *  (a unit test, a stripped page). The dark theme's values. */
const TOKEN_FALLBACK: Record<string, string> = {
  canvas: '#0a0c10', surface: '#14171d', 'surface-2': '#191d24',
  hairline: '#232936', rule: '#1b2735',
  silk: '#eef2f6', 'silk-dim': '#93a1b3', 'silk-faint': '#5f6c7d',
  copper: '#e08a4e', 'copper-hi': '#ffb072', 'copper-deep': '#a65f34',
  'copper-tint': 'rgba(224, 138, 78, 0.10)',
  ok: '#57e0a0', 'ok-bg': 'rgba(87, 224, 160, 0.08)', 'ok-border': '#2f7d5b',
  err: '#f87171', 'err-strong': '#fca5a5', 'err-bg': 'rgba(239, 68, 68, 0.08)', 'err-border': '#7f1d1d',
  warn: '#fbbf24', 'warn-strong': '#fde047', 'warn-bg': 'rgba(202, 138, 4, 0.08)', 'warn-border': '#713f12',
  note: '#94a3b8', 'note-accent': '#475569',
  instrument: '#050d1a', 'instrument-edge': '#1b2735', 'instrument-text': '#cbd5e1',
  'code-bg': '#0d1118',
  'font-sans': '-apple-system, BlinkMacSystemFont, "Segoe UI", Inter, Roboto, system-ui, sans-serif',
  'font-mono': '"SF Mono", ui-monospace, "Fira Code", monospace',
}

function resolveTokens(): Record<string, string> {
  const out: Record<string, string> = {}
  let computed: CSSStyleDeclaration | null = null
  try {
    computed = getComputedStyle(document.documentElement)
  } catch {
    computed = null
  }
  for (const name of TOKENS) {
    const live = computed?.getPropertyValue(`--${name}`).trim()
    out[name] = live || TOKEN_FALLBACK[name]
  }
  return out
}

/** Escape for HTML text and attribute content. Every string that reaches the
 *  exported file goes through this: findings carry net names and datasheet
 *  quotes, a spec carries whatever the user typed, and one unescaped `<` turns
 *  the rest of the report into markup. */
function esc(s: unknown): string {
  return String(s ?? '')
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;')
}

const LEVEL_ACCENT: Record<string, string> = {
  serious: 'var(--err)', warning: 'var(--warn)', note: 'var(--note-accent)',
}
const LEVEL_TEXT: Record<string, string> = {
  serious: 'var(--err-strong)', warning: 'var(--warn-strong)', note: 'var(--note)',
}

function stamp(ms: number | null): string {
  const d = ms ? new Date(ms) : new Date()
  // A local, unambiguous stamp plus the offset, so a report read in another
  // timezone still says when the run happened.
  return `${d.toLocaleString()} (UTC${d.getTimezoneOffset() <= 0 ? '+' : '-'}${
    String(Math.floor(Math.abs(d.getTimezoneOffset()) / 60)).padStart(2, '0')}:${
    String(Math.abs(d.getTimezoneOffset()) % 60).padStart(2, '0')})`
}

function sectionHtml(s: WebSection): string {
  const groups = groupFindings(s.findings)
  const cards = groups.map(g => {
    const accent = LEVEL_ACCENT[g.level] ?? 'var(--note-accent)'
    const tag = LEVEL_TEXT[g.level] ?? 'var(--note)'
    const many = g.items.length > 1
    const head = many
      ? `<span class="tag" style="color:${tag}">${esc(g.level)} &middot; ${g.items.length} similar</span>
         <div class="what">${g.items.length} similar findings, same cause, listed once below.</div>
         <ul>${g.items.map(i => `<li>${esc(i.what)}</li>`).join('')}</ul>`
      : `<span class="tag" style="color:${tag}">${esc(g.level)}</span>
         <div class="what">${esc(g.items[0].what)}</div>`
    return `<div class="card" style="border-left-color:${accent}">
      ${head}
      ${g.why ? `<div class="gloss"><b>Why it matters:</b> ${esc(g.why)}</div>` : ''}
      ${g.fix ? `<div class="gloss"><b>What to do:</b> ${esc(g.fix)}</div>` : ''}
    </div>`
  }).join('')
  const headsUp = (s.heads_up ?? []).map(h => `
    <div class="card" style="border-left-color:var(--copper)">
      <span class="tag" style="color:var(--copper)">Heads up</span>
      <div class="what">${esc(h.what)}</div>
      ${h.why ? `<div class="gloss"><b>Why it matters:</b> ${esc(h.why)}</div>` : ''}
      ${h.fix ? `<div class="gloss"><b>What to do:</b> ${esc(h.fix)}</div>` : ''}
    </div>`).join('')
  return `<section>
    <h2>${esc(s.title)}</h2>
    <p class="verdict-line">${esc(s.verdict)}</p>
    ${cards}${headsUp}
  </section>`
}

/** The bind table: which active ICs bound to a model and which are open. This
 *  is the report's honesty layer, so it is a table rather than a sentence: a
 *  reader deciding whether to trust an analog number needs to see the list. */
function bindHtml(report: WebReport): string {
  const b = report.bind
  if (!b) return ''
  const parts = b.open_parts ?? []
  const rows = parts.map(p => `<tr>
      <td class="mono">${esc(p.reference)}</td>
      <td class="mono">${esc(p.value)}</td>
      <td>${esc(p.reason)}</td>
      <td>${p.active_ic ? 'active IC' : 'passive'}</td>
      <td>${p.bound ? 'bound, left open' : 'no model'}</td>
      <td>${esc(p.consequence)}</td>
    </tr>`).join('')
  const unresolved = b.active_path_unresolved ?? []
  return `<section>
    <h2>Model binding</h2>
    <p class="verdict-line">
      ${esc(b.critical_parts_bound)} active ICs bound to a device model.
      ${unresolved.length > 0
        ? `<span class="warn-text">${esc(unresolved.join(', '))} could not be bound or are left open on the
           live circuit; analog, AC and thermal results on their nets are not trustworthy.</span>`
        : 'Nothing was left open on the live circuit.'}
    </p>
    ${rows
      ? `<div class="scroll-x"><table>
          <thead><tr><th>Ref</th><th>Value</th><th>Why</th><th>Role</th><th>State</th><th>Consequence</th></tr></thead>
          <tbody>${rows}</tbody>
        </table></div>`
      : ''}
  </section>`
}

/** Human evidence projection. The JSON export carries every provenance field;
 * this page keeps the trust decision readable by showing the status totals,
 * every non-clean assertion, and the canonical four-sentence assumption chain. */
function evidenceHtml(report: WebReport): string {
  const maps = report.evidence ?? []
  const assumptions = report.assumptions ?? []
  const inventory = report.inventory ?? []
  if (maps.length === 0 && assumptions.length === 0 && inventory.length === 0) return ''

  const count = (status: string) => maps.filter(map => map.status === status).length
  const caveated = maps.filter(map => map.status !== 'clean')
  const rows = caveated.map(map => `<tr>
      <td>${esc(map.assertion)}</td>
      <td class="mono">${esc(map.status)}</td>
      <td class="mono">${esc((map.assumptions ?? []).join(', ') || 'none')}</td>
    </tr>`).join('')
  const cards = assumptions.map(assumption => `<div class="card" style="border-left-color:${
    assumption.kind === 'open_part' ? 'var(--warn)' : 'var(--note-accent)'
  }">
      <span class="tag mono" style="color:var(--warn-strong)">${esc(assumption.id)}</span>
      <div class="what">${esc(assumption.statement)}</div>
      <div class="gloss"><b>Why:</b> ${esc(assumption.because)}</div>
      <div class="gloss"><b>Effect:</b> ${esc(assumption.consequence)}</div>
      <div class="gloss"><b>What closes it:</b> ${esc(assumption.replacement)}</div>
    </div>`).join('')
  const artifacts = inventory.map(artifact => `<tr>
      <td>${esc(artifact.path)}</td>
      <td class="mono">${esc(artifact.kind)}</td>
      <td class="mono">${esc(artifact.sha256 ? `sha256:${artifact.sha256}` : 'digest unavailable')}</td>
    </tr>`).join('')

  return `<section>
    <h2>Evidence &amp; limitations</h2>
    <p class="verdict-line">
      ${maps.length} ${maps.length === 1 ? 'assertion' : 'assertions'} mapped:
      ${count('clean')} clean, ${count('qualified')} qualified, ${count('undermined')} undermined.
      The machine-readable JSON retains the full artifact, model, parameter and error-budget fields.
    </p>
    ${rows
      ? `<div class="scroll-x"><table>
          <thead><tr><th>Assertion</th><th>Status</th><th>Rests on</th></tr></thead>
          <tbody>${rows}</tbody>
        </table></div>`
      : ''}
    ${artifacts
      ? `<h3>Input artifacts</h3><div class="scroll-x"><table>
          <thead><tr><th>Path</th><th>Kind</th><th>Digest</th></tr></thead>
          <tbody>${artifacts}</tbody>
        </table></div>`
      : ''}
    ${cards}
  </section>`
}

function cosimHtml(report: WebReport): string {
  const c = report.cosim
  if (!c) return ''
  if (!c.ran) {
    const why = (c.findings ?? []).map(f => `${f.what} ${f.why}`.trim()).join(' ')
    return `<section><h2>Firmware co-sim</h2>
      <p class="verdict-line">Co-sim did not run. ${esc(why || 'No co-sim was available for this board.')}</p>
    </section>`
  }
  const findings = groupFindings(c.findings ?? []).map(g => `
    <div class="card" style="border-left-color:${LEVEL_ACCENT[g.level] ?? 'var(--note-accent)'}">
      <span class="tag" style="color:${LEVEL_TEXT[g.level] ?? 'var(--note)'}">${esc(g.level)}</span>
      <div class="what">${g.items.map(i => esc(i.what)).join('; ')}</div>
      ${g.why ? `<div class="gloss"><b>Why it matters:</b> ${esc(g.why)}</div>` : ''}
      ${g.fix ? `<div class="gloss"><b>What to do:</b> ${esc(g.fix)}</div>` : ''}
    </div>`).join('')
  const gpio = (c.gpio_nets ?? []).map(g => `<tr>
      <td class="mono">${esc(g.name)}</td>
      <td class="mono num">${(g.volts || 0).toFixed(3)}</td>
      <td>${g.driven ? 'driven' : 'idle'}</td>
    </tr>`).join('')
  const timingCoverage = (c.timing_coverage ?? []).map(row => `<div>${esc(timingCoverageLine(row))}</div>`).join('')
  const timingRefusals = uncoveredTimingRefusals(c.timing_refusals, report.refusal)
    .map(line => `<div>${esc(line)}</div>`)
    .join('')
  const fallbackWindows = (c.fallback_windows ?? []).map(window => `<div>${esc(fallbackWindowLine(window))}</div>`).join('')
  return `<section>
    <h2>Firmware co-sim</h2>
    <p class="verdict-line">
      Ran the firmware for ${(c.seconds_simulated || 0).toFixed(3)}s on the board's microcontroller.
      ${c.analog_valid ? '' : ' The analog solve did not stay valid for the whole run.'}
    </p>
    ${findings}
    ${timingCoverage ? `<h3>Timing coverage</h3><div class="card">${timingCoverage}</div>` : ''}
    ${timingRefusals ? `<h3>TIMING INVALID</h3><div class="card" style="border-left-color:var(--err)">${timingRefusals}</div>` : ''}
    ${fallbackWindows ? `<h3>Fallback-qualified windows</h3><div class="card" style="border-left-color:var(--warn)">${fallbackWindows}</div>` : ''}
    ${c.uart_output ? `<h3>UART output</h3><pre class="instrument">${esc(c.uart_output)}</pre>` : ''}
    ${gpio
      ? `<h3>GPIO nets</h3><div class="scroll-x"><table>
          <thead><tr><th>Net</th><th>Volts</th><th>Activity</th></tr></thead>
          <tbody>${gpio}</tbody></table></div>`
      : ''}
  </section>`
}

function refusalHtml(report: WebReport): string {
  if (!report.refusal) return ''
  const rows = refusalLines(report.refusal)
    .map(([label, value]) => `<div class="gloss"><b>${esc(label)}:</b> ${esc(value)}</div>`)
    .join('\n      ')
  return `<section>
    <h2>Analysis refusal</h2>
    <div class="card" style="border-left-color:var(--warn)">
      ${rows}
    </div>
  </section>`
}

/** One self-contained HTML document. No external request of any kind: the
 *  styles are inline, there is no script, and no image is referenced. */
export function buildReportHtml(input: ReportExportInput): string {
  const { report: r, spec, checks } = input
  const t = resolveTokens()
  const light = (t.canvas || '').toLowerCase().startsWith('#f')

  let verdictBorder = 'var(--ok-border)', verdictBg = 'var(--ok-bg)'
  const bindOpen = !!r.bind?.active_path_unresolved?.length
  const hasHeadsUp = (r.sections ?? []).some(s => s.heads_up?.length)
  const evidenceSummary = summarizeEvidence(r.evidence)
  if (r.serious > 0) { verdictBorder = 'var(--err-border)'; verdictBg = 'var(--err-bg)' }
  else if (r.total > 0 || bindOpen || hasHeadsUp || evidenceSummary.caveated > 0) { verdictBorder = 'var(--warn-border)'; verdictBg = 'var(--warn-bg)' }

  const version = input.engineVersion ?? input.appVersion
  const title = `hauksbee report: ${r.board_name || r.file_name}`

  // The provenance block, with the duplicates left out. `board_name`, the
  // uploaded file's name and the session's name are the same string in the
  // ordinary case, and three rows of `watchy.kicad_pcb` reads as a rendering
  // fault rather than as identity. Each row appears only when it says something
  // the row above did not.
  const boardTitle = r.board_name || r.file_name
  const meta: [string, string][] = [['Board', boardTitle]]
  if (r.file_name && r.file_name !== boardTitle) meta.push(['File', r.file_name])
  if (input.boardLabel && input.boardLabel !== boardTitle && input.boardLabel !== r.file_name) {
    meta.push(['Uploaded as', input.boardLabel])
  }
  meta.push(
    ['Size', `${r.num_components} ${r.num_components === 1 ? 'part' : 'parts'}, ${
      r.num_nets} ${r.num_nets === 1 ? 'net' : 'nets'}`],
    ['Firmware', input.firmwareName ?? 'none staged'],
    ['Analyzed', stamp(input.analyzedAt)],
    ['Exported', stamp(null)],
    ['hauksbee', version],
  )
  if (input.sessionName && input.sessionName !== boardTitle && input.sessionName !== r.file_name) {
    meta.unshift(['Session', input.sessionName])
  }

  const checksLine = checks
    ? `${checks.passed} passed, ${checks.failed} failed${
      checks.invalid > 0 ? `, ${checks.invalid} could not be judged` : ''}`
    : null

  return `<!doctype html>
<html lang="en" data-theme="${light ? 'light' : 'dark'}">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="generator" content="hauksbee ${esc(version)}">
<title>${esc(title)}</title>
<style>
:root{
${TOKENS.map(n => `  --${n}: ${t[n]};`).join('\n')}
  color-scheme: ${light ? 'light' : 'dark'};
}
*{box-sizing:border-box}
body{
  margin:0; padding:0 1rem 4rem;
  background:var(--canvas); color:var(--silk);
  font-family:var(--font-sans); font-size:15px; line-height:1.55;
  -webkit-font-smoothing:antialiased;
}
.page{max-width:52rem;margin:0 auto}
header.top{padding:2.25rem 0 0}
.wordmark{
  font-family:var(--font-mono); font-size:12px; font-weight:600;
  letter-spacing:.22em; color:var(--copper); margin:0 0 .5rem;
}
h1{font-size:1.5rem;line-height:1.2;margin:0 0 .25rem;text-wrap:balance}
h2{
  font-size:11px;font-weight:700;letter-spacing:.16em;text-transform:uppercase;
  color:var(--silk-faint);margin:2.25rem 0 .35rem;
}
h3{font-size:13px;font-weight:600;color:var(--silk-dim);margin:1.25rem 0 .35rem}
p{margin:.35rem 0;text-wrap:pretty}
.verdict{
  border:1px solid ${verdictBorder}; background:${verdictBg};
  border-radius:12px; padding:.9rem 1rem; margin-top:1.25rem; font-size:15.5px;
}
.verdict .counts{
  font-size:12px;color:var(--silk-dim);margin-top:.4rem;
  font-variant-numeric:tabular-nums;
}
.verdict-line{color:var(--silk-dim);font-size:14px}
.warn-text{color:var(--warn-strong)}
dl.meta{
  display:grid; grid-template-columns:max-content minmax(0,1fr);
  gap:.2rem .9rem; margin:1.1rem 0 0; font-size:13px;
}
dl.meta dt{color:var(--silk-faint)}
dl.meta dd{margin:0;color:var(--silk);overflow-wrap:anywhere}
.card{
  border:1px solid var(--hairline); border-left:4px solid var(--note-accent);
  background:var(--surface); border-radius:8px;
  padding:.7rem .9rem; margin:.5rem 0;
}
.tag{font-size:10px;font-weight:700;letter-spacing:.14em;text-transform:uppercase}
.what{font-weight:600;font-size:14px;margin:.25rem 0 .35rem}
.gloss{font-size:14px;margin:.15rem 0;color:var(--silk)}
.gloss b{color:var(--silk-dim);font-weight:600}
.card ul{margin:.35rem 0 .5rem;padding-left:1.15rem;font-size:14px}
.card li{margin:.15rem 0}
.scroll-x{overflow-x:auto;max-width:100%}
table{border-collapse:collapse;width:100%;font-size:13px;min-width:34rem}
th{
  text-align:left;padding:.35rem .5rem;font-weight:600;color:var(--silk-dim);
  border-bottom:1px solid var(--hairline);white-space:nowrap;
}
td{padding:.35rem .5rem;border-bottom:1px solid var(--rule);vertical-align:top}
.mono{font-family:var(--font-mono)}
.num{font-variant-numeric:tabular-nums}
pre{
  background:var(--code-bg); border:1px solid var(--hairline); border-radius:8px;
  color:var(--silk-dim); font-family:var(--font-mono); font-size:12px;
  line-height:1.5; padding:.75rem; margin:.5rem 0;
  overflow-x:auto; white-space:pre;
}
pre.instrument{
  background:var(--instrument); border-color:var(--instrument-edge);
  color:var(--instrument-text); white-space:pre-wrap;
}
.note-row{
  border:1px solid var(--hairline); border-left:4px solid var(--note-accent);
  background:var(--surface); border-radius:8px; padding:.6rem .9rem;
  margin:.5rem 0; font-size:14px;
}
footer{
  margin-top:3rem; padding-top:1rem; border-top:1px solid var(--hairline);
  color:var(--silk-faint); font-size:12px;
}
footer code{font-family:var(--font-mono)}
@media print{
  body{background:#fff;color:#111}
  .card,.note-row,pre,table{break-inside:avoid}
}
</style>
</head>
<body>
<div class="page">
<header class="top">
  <p class="wordmark">HAUKSBEE</p>
  <h1>${esc(r.board_name || r.file_name)}</h1>
  <div class="verdict">
    ${esc(r.headline)}
    <div class="counts">
      ${r.serious} serious &middot; ${r.total} ${r.total === 1 ? 'finding' : 'findings'} total
      ${checksLine ? `&middot; checks: ${esc(checksLine)}` : ''}
    </div>
  </div>
  <dl class="meta">
    ${meta.map(([k, v]) => `<dt>${esc(k)}</dt><dd>${esc(v)}</dd>`).join('\n    ')}
  </dl>
</header>

${refusalHtml(r)}

${(r.notes ?? [])
  // The bind-role note restates what the Model binding section below says in
  // full; the JSON carries both for CLI parity, and the app renders only the
  // stronger one. So does this.
  .filter(n => !(bindOpen && n.kind === 'bind_role'))
  .map(n => `<div class="note-row"><b>Note:</b> ${esc(n.message)}</div>`).join('\n')}

${bindHtml(r)}

${evidenceHtml(r)}

${(r.sections ?? []).map(sectionHtml).join('\n')}

${cosimHtml(r)}

${spec
  ? `<section>
      <h2>Composed checks (${esc(spec.fileName)})</h2>
      <p class="verdict-line">
        The spec this session composed${checksLine ? `; last run: ${esc(checksLine)}` : ', not yet run'}.
        Save it as <code class="mono">${esc(spec.fileName)}</code> and
        <code class="mono">hauksbee-ci</code> runs exactly these assertions.
      </p>
      <pre>${esc(spec.toml)}</pre>
    </section>`
  : ''}

<footer>
  <p>
    A snapshot of one analysis run, written by hauksbee ${esc(version)}. It holds no board
    file and no firmware image: re-running it needs those files and the same hauksbee.
  </p>
  ${input.restored
    ? `<p>
        This report was restored from a saved browser session rather than produced by a run
        in the exporting tab, so the numbers are those of the earlier run named above.
      </p>`
    : ''}
  <p>
    Findings are grouped by cause, exactly as the app groups them; nothing in the JSON has
    been dropped. For the machine-readable form, export the findings as JSON.
  </p>
</footer>
</div>
</body>
</html>
`
}
