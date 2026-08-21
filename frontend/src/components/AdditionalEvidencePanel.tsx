import { FirmwareJack } from './FirmwareJack'
import { SchematicJack } from './SchematicJack'
import type { InputHTMLAttributes } from 'react'

function EvidenceFile({ label, hint, file, accept, locked, onFile }: {
  label: string
  hint: string
  file: File | null
  accept: string
  locked: boolean
  onFile: (file: File | null) => void
}) {
  return (
    <div className="mt-2 flex items-center gap-3 rounded-lg px-3 py-2" style={{ border: '1px solid var(--hairline)', background: 'var(--surface-2)' }}>
      <div className="min-w-0 flex-1">
        <div className="text-[12px] font-semibold" style={{ color: 'var(--silk)' }}>{label}</div>
        <div className="truncate text-[11px]" style={{ color: file ? 'var(--ok)' : 'var(--silk-faint)' }}>
          {file?.name ?? hint}
        </div>
      </div>
      <label className="hb-btn cursor-pointer px-2.5 py-1.5 text-[11px]" aria-disabled={locked}>
        {file ? 'Replace' : 'Choose'}
        <input
          className="sr-only"
          type="file"
          accept={accept}
          disabled={locked}
          onChange={event => onFile(event.currentTarget.files?.[0] ?? null)}
        />
      </label>
      {file && <button type="button" className="text-[11px]" style={{ color: 'var(--silk-faint)' }} disabled={locked} onClick={() => onFile(null)}>remove</button>}
    </div>
  )
}

/**
 * Every design input supported by the local engine is available here. The
 * same selected files flow into report analysis, Checks, and Live Sim.
 */
export function AdditionalEvidencePanel({
  placement,
  firmware,
  schematic,
  bom,
  placementFile,
  variant,
  asbuilt,
  models,
  onFirmware,
  onClearFirmware,
  onSchematic,
  onClearSchematic,
  onBom,
  onPlacement,
  onVariant,
  onAsbuilt,
  onModels,
  locked = false,
  boardName = null,
  cosimRan,
  showWebControls = true,
}: {
  placement: 'intake' | 'report'
  firmware: File | null
  schematic: File | null
  bom: File | null
  placementFile: File | null
  variant: File | null
  asbuilt: File | null
  models: File[]
  onFirmware: (file: File) => void
  onClearFirmware?: () => void
  onSchematic: (file: File) => void
  onClearSchematic?: () => void
  onBom: (file: File | null) => void
  onPlacement: (file: File | null) => void
  onVariant: (file: File | null) => void
  onAsbuilt: (file: File | null) => void
  onModels: (files: File[]) => void
  locked?: boolean
  boardName?: string | null
  cosimRan?: boolean
  /** Saved reports have no files behind them, so their slots are informational. */
  showWebControls?: boolean
}) {
  void boardName

  return (
    <section
      data-testid={`additional-evidence-${placement}`}
      className={placement === 'intake' ? 'mt-5' : 'mt-3'}
    >
      <div
        className="rounded-xl px-4 py-3.5"
        style={{ border: '1px solid var(--hairline)', background: 'var(--surface)' }}
      >
        <div className="flex items-baseline justify-between gap-3 flex-wrap">
          <h2
            className="text-[11px] font-bold tracking-widest uppercase"
            style={{ color: 'var(--silk-faint)', margin: 0 }}
          >
            Additional evidence
          </h2>
          <span className="text-[11px]" style={{ color: 'var(--silk-faint)' }}>
            shared by report, Checks, and Live Sim
          </span>
        </div>
        <p className="mt-1.5 text-[12px] leading-relaxed" style={{ color: 'var(--silk-dim)' }}>
          Add any manufacturing, assembly, firmware, or model evidence you have.
          A change here re-runs the current board with the complete bundle.
        </p>

        {showWebControls && onClearFirmware && onClearSchematic ? (
          <div className="mt-3">
            <div className="text-[10px] font-bold tracking-widest uppercase" style={{ color: 'var(--copper)' }}>
              Accepted in this browser
            </div>
            <FirmwareJack
              firmware={firmware}
              placement={placement}
              onFile={onFirmware}
              onClear={onClearFirmware}
              locked={locked}
              cosimRan={cosimRan}
            />
            <SchematicJack
              schematic={schematic}
              onFile={onSchematic}
              onClear={onClearSchematic}
              locked={locked}
            />
            <EvidenceFile label="BOM" hint="CSV, TSV, XLSX, or exported BOM" file={bom} accept=".csv,.tsv,.txt,.xlsx" locked={locked} onFile={onBom} />
            <EvidenceFile label="Placement / CPL" hint="pick-and-place CSV, POS, or TXT" file={placementFile} accept=".csv,.pos,.txt,.tsv" locked={locked} onFile={onPlacement} />
            <EvidenceFile label="Assembly variant" hint="fitted / no-fit variant TOML" file={variant} accept=".toml" locked={locked} onFile={onVariant} />
            <EvidenceFile label="As-built overlay" hint="asbuilt.toml" file={asbuilt} accept=".toml" locked={locked} onFile={onAsbuilt} />
            <div className="mt-2 flex items-center gap-3 rounded-lg px-3 py-2" style={{ border: '1px solid var(--hairline)', background: 'var(--surface-2)' }}>
              <div className="min-w-0 flex-1">
                <div className="text-[12px] font-semibold" style={{ color: 'var(--silk)' }}>Model library</div>
                <div className="truncate text-[11px]" style={{ color: models.length ? 'var(--ok)' : 'var(--silk-faint)' }}>
                  {models.length ? `${models.length} model file${models.length === 1 ? '' : 's'} selected` : 'choose a model folder or several .toml files'}
                </div>
              </div>
              <label className="hb-btn cursor-pointer px-2.5 py-1.5 text-[11px]" aria-disabled={locked}>
                Folder
                <input
                  className="sr-only"
                  type="file"
                  accept=".toml"
                  multiple
                  disabled={locked}
                  {...({ webkitdirectory: '' } as InputHTMLAttributes<HTMLInputElement>)}
                  onChange={event => onModels(Array.from(event.currentTarget.files ?? []))}
                />
              </label>
              <label className="hb-btn cursor-pointer px-2.5 py-1.5 text-[11px]" aria-disabled={locked}>
                Files
                <input
                  className="sr-only"
                  type="file"
                  accept=".toml"
                  multiple
                  disabled={locked}
                  onChange={event => onModels(Array.from(event.currentTarget.files ?? []))}
                />
              </label>
              {models.length > 0 && <button type="button" className="text-[11px]" style={{ color: 'var(--silk-faint)' }} disabled={locked} onClick={() => onModels([])}>remove</button>}
            </div>
          </div>
        ) : (
          <div
            className="mt-3 rounded-lg px-3 py-2 text-[12px]"
            style={{ border: '1px solid var(--hairline)', color: 'var(--silk-faint)' }}
          >
            The saved report has no files behind it. Drop the board again to add
            the evidence bundle.
          </div>
        )}

        <div className="mt-3 rounded-lg px-3 py-2 text-[12px]" style={{ border: '1px solid var(--hairline)' }}>
          <div className="font-semibold" style={{ color: 'var(--silk)' }}>Also available</div>
          <div className="mt-0.5 leading-relaxed" style={{ color: 'var(--silk-dim)' }}>
            Datasheet model drafts appear when Model coverage finds an unbound part.
            A PDF leaves this machine only after explicit consent.
          </div>
        </div>

      </div>
    </section>
  )
}
