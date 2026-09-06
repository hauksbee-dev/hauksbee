import { parse as parseToml } from 'smol-toml'
import type { ActionResultMsg } from '../../types/protocol'
import type { SensorCatalogEntry } from '../../types/report'
import type { SensorInputRow, SensorRow } from '../../lib/check-spec'
import { AddInlineButton, BuilderSection, Field, RemoveButton, RowCard, RowIssues } from './pieces'

/** Firmware-visible peripherals are data, not a hard-coded dropdown of
 *  whatever parts Hauksbee happened to ship. A local validated sensor spec is
 *  embedded into the exported scenario, so the result is portable and never
 *  requires an LLM or a path that exists only on this computer. */
export function SensorBuilder({
  sensors, validation, catalog, catalogError, liveRequests, liveActionResult,
  liveAttachAvailable, nextInputId, onAdd, onUpdate, onRemove, onAttachLive,
}: {
  sensors: SensorRow[]
  validation: Map<number, string[]>
  catalog: SensorCatalogEntry[]
  catalogError: string | null
  /** rowId -> the request id its last "attach live" press produced. */
  liveRequests: Map<number, number>
  liveActionResult?: ActionResultMsg | null
  liveAttachAvailable: boolean
  nextInputId: () => number
  onAdd: () => void
  onUpdate: (rowId: number, patch: Partial<SensorRow>) => void
  onRemove: (rowId: number) => void
  onAttachLive: (sensor: SensorRow) => void
}) {
  return (
    <BuilderSection
      testId="sensor-builder"
      title="Register-map devices"
      caption="I²C/SPI behavior from validated local TOML. The exact bytes are embedded in this scenario."
      actions={
        <button type="button" className="hb-chip hb-press px-2 py-1 text-[11px]" onClick={onAdd}>
          + bus device
        </button>
      }
    >
      {sensors.length === 0 && (
        <div className="text-[12px] py-2" style={{ color: 'var(--silk-dim)' }}>
          Click a sensor or memory IC on the board, or add one here. Choose a checked-in sensor
          spec; Hauksbee never guesses register behavior from a part number.
        </div>
      )}

      {sensors.map(sensor => {
        const issues = validation.get(sensor.rowId) ?? []
        const set = (patch: Partial<SensorRow>) => onUpdate(sensor.rowId, patch)
        const liveRequestId = liveRequests.get(sensor.rowId)
        const receipt = liveActionResult?.action === 'attach_register_map'
          && liveActionResult.request_id === liveRequestId
          ? liveActionResult
          : null
        return (
          <RowCard key={sensor.rowId} testId={`sensor-${sensor.rowId}`} invalid={issues.length > 0}>
            <div className="flex flex-wrap items-center gap-2 mb-2">
              <input
                className="hb-input" style={{ width: 110 }} value={sensor.id}
                aria-label="sensor id" placeholder="stable id"
                onChange={e => set({ id: e.target.value })}
              />
              <input
                className="hb-input" style={{ width: 110 }} value={sensor.controller}
                aria-label="sensor controller" placeholder="spi2 (optional)"
                onChange={e => set({ controller: e.target.value })}
              />
              <input
                className="hb-input" style={{ width: 130 }} list="net-options" value={sensor.csNet}
                aria-label="sensor chip select net" placeholder="CS net (SPI optional)"
                onChange={e => set({ csNet: e.target.value })}
              />
              <select
                className="hb-input"
                style={{ width: 190 }}
                data-testid={`sensor-catalog-${sensor.rowId}`}
                aria-label="bundled sensor behavior"
                value=""
                disabled={catalog.length === 0}
                title={catalogError ?? 'Checked-in local behavior; no network or LLM'}
                onChange={e => {
                  const entry = catalog.find(item => item.id === e.target.value)
                  if (!entry) return
                  let inputs: SensorInputRow[] = []
                  try {
                    const parsed = parseToml(entry.spec_toml) as {
                      sensor?: { input?: Array<{ name?: unknown; default?: unknown }> }
                    }
                    inputs = (parsed.sensor?.input ?? []).flatMap(input =>
                      typeof input.name === 'string' && typeof input.default === 'number'
                        ? [{ rowId: nextInputId(), name: input.name, value: String(input.default) }]
                        : [])
                  } catch { /* catalog endpoint is separately validated; keep exact bytes usable */ }
                  set({ spec: entry.spec_toml, specName: `bundled:${entry.id}`, inputs })
                }}
              >
                <option value="">
                  {catalogError
                    ? 'bundled library unavailable'
                    : catalog.length === 0 ? 'loading bundled behavior…' : 'choose bundled behavior…'}
                </option>
                {catalog.map(entry => (
                  <option key={entry.id} value={entry.id} title={entry.scope}>
                    {entry.name} · {entry.bus.toUpperCase()}
                  </option>
                ))}
              </select>
              <label className="hb-chip hb-press px-2.5 py-1.5 text-[11px] cursor-pointer">
                {sensor.specName ? `loaded ${sensor.specName}` : 'choose sensor TOML'}
                <input
                  type="file"
                  accept=".toml,text/plain"
                  className="sr-only"
                  data-testid={`sensor-file-${sensor.rowId}`}
                  onChange={e => {
                    const file = e.target.files?.[0]
                    if (!file) return
                    void file.text().then(spec => set({ spec, specName: file.name }))
                  }}
                />
              </label>
              <RemoveButton className="ml-auto" onClick={() => onRemove(sensor.rowId)} />
            </div>
            {(sensor.componentRef || sensor.modelId) && (
              <div className="text-[10px] mb-2" style={{ color: 'var(--silk-faint)' }}>
                from clicked component {sensor.componentRef || sensor.id}
                {sensor.modelId ? <> · model <code style={{ fontFamily: 'var(--font-mono)' }}>{sensor.modelId}</code></> : null}
              </div>
            )}
            <label className="block text-[11px]" style={{ color: 'var(--silk-faint)' }}>
              Spec bytes (paste or edit; checked again by the real runner)
              <textarea
                className="hb-input w-full mt-1 text-[11px]"
                style={{ minHeight: 110, fontFamily: 'var(--font-mono)', lineHeight: 1.45, padding: 8 }}
                value={sensor.spec}
                aria-label="sensor spec"
                placeholder={'[sensor]\nname = "my device"\nbus = "i2c"\ni2c_address = 0x48\n...'}
                onChange={e => set({ spec: e.target.value, specName: '' })}
              />
            </label>
            <div className="flex flex-wrap items-center gap-2 mt-2">
              <span className="text-[10px] font-bold tracking-wider uppercase" style={{ color: 'var(--silk-faint)' }}>
                physical inputs
              </span>
              <AddInlineButton
                onClick={() => set({ inputs: [...sensor.inputs, { rowId: nextInputId(), name: '', value: '25' }] })}
              >
                + override
              </AddInlineButton>
            </div>
            {sensor.inputs.map(input => {
              const patch = (next: Partial<SensorInputRow>) => set({
                inputs: sensor.inputs.map(item => item.rowId === input.rowId ? { ...item, ...next } : item),
              })
              return (
                <div key={input.rowId} className="flex flex-wrap items-center gap-2 mt-1">
                  <input
                    className="hb-input" style={{ width: 150 }} value={input.name}
                    aria-label="sensor input name" placeholder="temperature_c"
                    onChange={e => patch({ name: e.target.value })}
                  />
                  <Field label="value" value={input.value} width={80} onChange={value => patch({ value })} />
                  <RemoveButton
                    label="remove input"
                    onClick={() => set({ inputs: sensor.inputs.filter(item => item.rowId !== input.rowId) })}
                  />
                </div>
              )
            })}
            {liveAttachAvailable && (
              <button
                type="button"
                data-testid={`sensor-attach-live-${sensor.rowId}`}
                className="hb-btn-primary hb-press px-2.5 py-1.5 text-[11px] mt-2"
                onClick={() => onAttachLive(sensor)}
              >
                Attach these exact bytes to the live simulation
              </button>
            )}
            {liveRequestId !== undefined && (
              <div
                data-testid={`sensor-live-result-${sensor.rowId}`}
                className="text-[11px] mt-2 rounded-md px-2.5 py-2"
                style={{
                  color: receipt ? (receipt.ok ? 'var(--ok)' : 'var(--err)') : 'var(--silk-dim)',
                  background: receipt ? (receipt.ok ? 'var(--ok-bg)' : 'var(--err-bg)') : 'var(--surface-1)',
                  border: `1px solid ${receipt ? (receipt.ok ? 'var(--ok-border)' : 'var(--err)') : 'var(--hairline)'}`,
                }}
              >
                {receipt
                  ? receipt.message
                  : 'Waiting for the simulation engine to validate and attach these exact bytes…'}
              </div>
            )}
            <RowIssues issues={issues} />
          </RowCard>
        )
      })}
    </BuilderSection>
  )
}
