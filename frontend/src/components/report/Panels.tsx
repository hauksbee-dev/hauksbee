import type { ModelCoverageComponent, ModelCoverageSnapshot, WebImportDiagnostics } from '../../types/report'
import { plural, stageWords } from '../../lib/report-view'
import type { ReportView } from '../../lib/report-view'
import { displayNet } from '../../lib/net-name'
import type { LocateFn } from './Findings'

// The three collapsible panels that qualify a report: what the importer
// recovered, which parts have models, and what the evidence supports.

/** A report panel that opens itself when it has something the reader must
 *  see, and edges amber while that is unresolved. */
function ReportPanel({ testId, title, summary, open, warn, children }: {
  testId: string
  title: string
  /** The count line beside the title. */
  summary: React.ReactNode
  open: boolean
  warn: boolean
  children: React.ReactNode
}) {
  return (
    <details
      data-testid={testId}
      open={open}
      className="mt-3 rounded-lg px-4 py-3"
      style={{
        border: `1px solid ${warn ? 'var(--warn-border)' : 'var(--hairline)'}`,
        background: 'var(--surface)',
      }}
    >
      <summary className="cursor-pointer text-sm font-semibold" style={{ color: 'var(--silk)' }}>
        {title}
        <span className="ml-2 text-[11px] font-normal tnum" style={{ color: 'var(--silk-dim)' }}>
          {summary}
        </span>
      </summary>
      {children}
    </details>
  )
}

/** The explanatory paragraph under a panel's summary. */
function PanelNote({ children }: { children: React.ReactNode }) {
  return <p className="mt-2 text-[12px] leading-relaxed" style={{ color: 'var(--silk-dim)' }}>{children}</p>
}

/** The smaller footnote a panel closes with. */
function PanelFootnote({ children }: { children: React.ReactNode }) {
  return <p className="mt-2 text-[11px] leading-relaxed" style={{ color: 'var(--silk-faint)' }}>{children}</p>
}

export function ImportDiagnosticsPanel({
  diagnostics, overlay, selectedNet, onToggleOverlay, onLocate, onInspectNet,
}: {
  diagnostics: WebImportDiagnostics
  overlay: boolean
  selectedNet: string | null
  onToggleOverlay: () => void
  onLocate: LocateFn
  onInspectNet: (net: string) => void
}) {
  const issues = diagnostics.issues ?? []
  const caveated = diagnostics.partial + diagnostics.missing_or_refused
  return (
    <ReportPanel
      testId="import-diagnostics"
      title="Import coverage"
      open={caveated > 0 || issues.length > 0}
      warn={caveated > 0}
      summary={<>
        {diagnostics.recovered} recovered · {diagnostics.partial} partial · {diagnostics.unplaced} unplaced
        {diagnostics.missing_or_refused > 0 ? ` · ${plural(diagnostics.missing_or_refused, 'missing/refused limit')}` : ''}
      </>}
    >
      <div className="mt-2 flex flex-wrap items-center justify-between gap-2 text-[12px]" style={{ color: 'var(--silk-dim)' }}>
        <span>
          Reader: <b style={{ color: 'var(--silk)', fontWeight: 600 }}>{diagnostics.format}</b>.
          Confidence describes fields actually recovered; it is not a claim about the physical board.
        </span>
        <button
          type="button"
          data-testid="toggle-import-overlay"
          className="hb-press rounded-md px-2.5 py-1 text-[11px] font-semibold"
          style={{
            border: '1px solid var(--hairline)',
            background: overlay ? 'var(--copper-tint)' : 'var(--canvas)',
            color: overlay ? 'var(--copper-hi)' : 'var(--silk-dim)',
          }}
          onClick={onToggleOverlay}
        >
          {overlay ? 'Hide board overlay' : 'Show recovered / partial on board'}
        </button>
      </div>

      {issues.map((issue, index) => {
        const locatedOnNet = issue.net
          ? diagnostics.objects.filter(object =>
              object.x !== undefined && object.y !== undefined && object.nets?.includes(issue.net!))
          : []
        return (
          <div
            key={`${index}:${issue.kind}:${issue.title}`}
            data-testid="import-issue"
            className="mt-2 rounded-md px-3 py-2.5 text-[12px] leading-relaxed"
            style={{ border: '1px solid var(--warn-border)', background: 'var(--warn-bg)' }}
          >
            <div className="font-semibold" style={{ color: 'var(--warn-strong)' }}>{issue.title}</div>
            <div className="mt-1" style={{ color: 'var(--silk)' }}>{issue.explanation}</div>
            <div className="mt-1" style={{ color: 'var(--silk-dim)' }}><b>What fixes it:</b> {issue.suggested_fix}</div>
            {issue.net && (
              <>
                <button
                  type="button"
                  className="hb-press mt-2 rounded px-2 py-1 text-[11px] font-semibold"
                  style={{ border: '1px solid var(--hairline)', color: 'var(--copper-hi)', background: 'var(--surface)' }}
                  onClick={() => onInspectNet(issue.net!)}
                >
                  Inspect {displayNet(issue.net)}
                </button>
                {selectedNet === issue.net && (
                  <div data-testid="import-net-inspection" className="mt-2" style={{ color: 'var(--silk-dim)' }}>
                    {locatedOnNet.length > 0
                      ? `Highlighted ${plural(locatedOnNet.length, 'located imported object')} on this net.`
                      : 'No placeable object was recovered for this net. The reader supplied no coordinate to highlight.'}
                  </div>
                )}
              </>
            )}
          </div>
        )
      })}

      <div className="mt-3 max-h-64 overflow-y-auto rounded-md" style={{ border: '1px solid var(--hairline)' }}>
        {diagnostics.objects.map(object => (
          <div
            key={object.id}
            data-testid="import-object"
            className="grid gap-2 px-3 py-2 text-[12px] sm:grid-cols-[minmax(5rem,auto)_auto_minmax(0,1fr)_auto]"
            style={{ borderBottom: '1px solid var(--hairline)', color: 'var(--silk-dim)' }}
          >
            <span style={{ color: 'var(--silk)', fontFamily: 'var(--font-mono)' }}>{object.id}</span>
            <span style={{ color: object.status === 'recovered' ? 'var(--ok)' : 'var(--warn-strong)' }}>{object.status}</span>
            <span title={object.explanation}>{object.confidence} confidence · {object.explanation}</span>
            {object.x !== undefined && object.y !== undefined ? (
              <button
                type="button"
                className="hb-press text-[11px] font-semibold"
                style={{ color: 'var(--copper-hi)', background: 'none', border: 'none' }}
                onClick={() => onLocate(object.x!, object.y!, `Imported ${object.id}: ${object.status}`)}
              >
                Show
              </button>
            ) : (
              <span title="The source supplied no coordinate, so plotting one would be fabricated.">
                not placeable
              </span>
            )}
          </div>
        ))}
      </div>
      {diagnostics.unplaced > 0 && (
        <PanelFootnote>
          Unplaced objects stay in this list. They are not drawn at guessed coordinates.
        </PanelFootnote>
      )}
    </ReportPanel>
  )
}

export function ModelCoveragePanel({ coverage, onSelect, onAuthor }: {
  coverage: ModelCoverageSnapshot
  onSelect: (component: ModelCoverageComponent) => void
  onAuthor: (component: ModelCoverageComponent) => void
}) {
  const { summary } = coverage
  const gaps = coverage.components.filter(component => component.actionable_behavior_gap)
  /** Every cell in a row opens the same inspection; only the Extend chip differs. */
  const cell = (component: ModelCoverageComponent, className: string, style: React.CSSProperties, title: string | undefined, body: React.ReactNode) => (
    <button
      type="button"
      className={`hb-press text-left ${className}`}
      title={title}
      style={{ background: 'none', border: 'none', ...style }}
      onClick={() => onSelect(component)}
    >
      {body}
    </button>
  )
  return (
    <ReportPanel
      testId="model-coverage"
      title="Model coverage"
      open={gaps.length > 0}
      warn={gaps.length > 0}
      summary={<>
        {summary.identified}/{summary.active_connected} identified ·{' '}
        {summary.executable_available}/{summary.active_connected} executable ·{' '}
        {summary.executable_declared} complete for declared scope
      </>}
    >
      <PanelNote>
        “Identified” means the part and pins are known. “Executable” means some behaviour runs.
        Only a row with no declared missing capability is complete for the scope its source supports.
        Click any row to inspect the exact model, datasheet provenance and affected nets on the board.
      </PanelNote>
      <div className="mt-3 max-h-72 overflow-y-auto rounded-md" style={{ border: '1px solid var(--hairline)' }}>
        {coverage.components.map((component, index) => (
          <div
            key={component.reference}
            data-testid={`model-coverage-${component.reference}`}
            className="hb-press grid w-full gap-x-3 gap-y-1 px-3 py-2 text-left text-[11px] sm:grid-cols-[4rem_minmax(8rem,1fr)_minmax(8rem,1fr)]"
            style={{
              border: 'none',
              borderTop: index > 0 ? '1px solid var(--hairline)' : 'none',
              background: 'transparent',
              color: 'var(--silk-dim)',
            }}
          >
            {cell(component, '', { color: 'var(--copper-hi)', fontFamily: 'var(--font-mono)' }, undefined,
              <b>{component.reference}</b>)}
            {cell(component, 'truncate', { color: 'var(--silk-dim)' },
              `Inspect ${component.reference}: ${component.value}`,
              component.value || '(no value)')}
            <span
              className="flex items-center justify-between gap-2"
              style={{ color: component.actionable_behavior_gap ? 'var(--warn-strong)' : 'var(--ok)' }}
            >
              {cell(component, '', { color: 'inherit' }, undefined, <>
                {stageWords(component.stage)}
                {component.missing?.length ? ` · ${plural(component.missing.length, 'gap')}` : ''}
              </>)}
              {component.actionable_behavior_gap && (
                <button
                  type="button"
                  data-testid={`model-author-${component.reference}`}
                  className="hb-chip hb-press px-2 py-0.5 text-[10px]"
                  onClick={() => onAuthor(component)}
                >
                  Extend
                </button>
              )}
            </span>
          </div>
        ))}
      </div>
      {gaps.length > 0 && (
        <PanelFootnote>
          {plural(gaps.length, 'component')} need more behaviour for a fuller claim.
          This list is deterministic and does not require an LLM; datasheet drafting below is optional.
        </PanelFootnote>
      )}
    </ReportPanel>
  )
}

export function EvidencePanel({ evidence: { inventory, assumptions, summary, caveated } }: { evidence: ReportView['evidence'] }) {
  return (
    <ReportPanel
      testId="evidence-panel"
      title="Evidence & limitations"
      open={summary.undermined > 0}
      warn={summary.undermined > 0}
      summary={`${summary.clean} fully supported · ${summary.qualified} supported with limitations · ${summary.undermined} invalid`}
    >
      <PanelNote>
        These are the engine's derived evidence statuses. An invalid assertion is not
        entitled to a pass/fail verdict until the limitation below is closed.
      </PanelNote>
      {inventory.length > 0 && (
        <div className="mt-3 text-[12px]" style={{ color: 'var(--silk-dim)' }}>
          <div className="font-semibold" style={{ color: 'var(--silk)' }}>Input artifacts</div>
          {inventory.map((artifact, index) => (
            <div key={`${index}:${artifact.path}`} className="mt-1 grid gap-x-2 sm:grid-cols-[minmax(0,1fr)_auto]">
              <span className="truncate" title={artifact.path}>{artifact.path}</span>
              <span className="tnum" style={{ fontFamily: 'var(--font-mono)' }}>
                {artifact.sha256 ? `sha256:${artifact.sha256.slice(0, 12)}…` : 'digest unavailable'}
              </span>
            </div>
          ))}
        </div>
      )}
      {assumptions.map(assumption => (
        <div
          key={assumption.id}
          className="mt-2 rounded-md px-3 py-2.5 text-[12px] leading-relaxed"
          style={{ border: '1px solid var(--hairline)', background: 'var(--canvas)' }}
        >
          <div className="text-[10px] font-semibold" style={{ color: 'var(--warn-strong)', fontFamily: 'var(--font-mono)' }}>
            {assumption.id}
          </div>
          <div className="mt-1 font-semibold" style={{ color: 'var(--silk)' }}>{assumption.statement}</div>
          <div className="mt-1" style={{ color: 'var(--silk-dim)' }}><b>Why:</b> {assumption.because}</div>
          <div style={{ color: 'var(--silk-dim)' }}><b>Effect:</b> {assumption.consequence}</div>
          <div style={{ color: 'var(--silk-dim)' }}><b>What closes it:</b> {assumption.replacement}</div>
        </div>
      ))}
      {caveated.length > 0 && (
        <div className="mt-3 text-[12px]" style={{ color: 'var(--silk-dim)' }}>
          <div className="font-semibold" style={{ color: 'var(--silk)' }}>Affected assertions</div>
          {caveated.slice(0, 20).map((map, index) => (
            <div key={`${index}:${map.assertion}:${map.status}`} className="mt-1 flex gap-2">
              <span style={{ color: map.status === 'undermined' ? 'var(--warn-strong)' : 'var(--note)' }}>{map.status}</span>
              <span>{map.assertion}</span>
            </div>
          ))}
          {summary.caveated > 20 && (
            <div className="mt-1">…and {summary.caveated - 20} more in the JSON export.</div>
          )}
        </div>
      )}
    </ReportPanel>
  )
}
