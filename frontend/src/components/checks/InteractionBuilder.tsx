import type { PeripheralRow } from '../../lib/check-spec'
import { AddInlineButton, BuilderSection, Field, RemoveButton, RowCard, RowIssues } from './pieces'

/** Physical interactions are experiment inputs, not verdicts. They live
 *  between supplies and assertions so the visual flow reads: power it,
 *  interact with it, then judge it. */
export function InteractionBuilder({ peripherals, validation, onAdd, onUpdate, onRemove }: {
  peripherals: PeripheralRow[]
  validation: Map<number, string[]>
  onAdd: (kind: PeripheralRow['kind']) => void
  onUpdate: (rowId: number, patch: Partial<PeripheralRow>) => void
  onRemove: (rowId: number) => void
}) {
  return (
    <BuilderSection
      testId="interaction-builder"
      title="Interactions & stimuli"
      caption="Real 50 Ω stimulus and contact models attached to board nets; not forced post-solve values."
      actions={(['stimulus', 'pushbutton', 'toggle'] as const).map(kind => (
        <button
          key={kind}
          type="button"
          className="hb-chip hb-press px-2 py-1 text-[11px]"
          onClick={() => onAdd(kind)}
        >
          + {kind === 'stimulus' ? 'waveform' : kind === 'pushbutton' ? 'button' : 'switch'}
        </button>
      ))}
    >
      {peripherals.length === 0 && (
        <div className="text-[12px] py-2" style={{ color: 'var(--silk-dim)' }}>
          Click a trace on the board, then add a waveform, button or switch here. Register-map
          bus devices have their own source-bound section below.
        </div>
      )}

      {peripherals.map(peripheral => {
        const issues = validation.get(peripheral.rowId) ?? []
        const set = (patch: Partial<PeripheralRow>) => onUpdate(peripheral.rowId, patch)
        const patchEvent = (index: number, patch: Partial<PeripheralRow['events'][number]>) =>
          set({ events: peripheral.events.map((item, i) => i === index ? { ...item, ...patch } : item) })
        return (
          <RowCard key={peripheral.rowId} testId={`interaction-${peripheral.rowId}`} invalid={issues.length > 0}>
            <div className="flex flex-wrap items-center gap-2 mb-2">
              <select
                className="hb-input"
                value={peripheral.kind}
                onChange={e => set({ kind: e.target.value as PeripheralRow['kind'] })}
                aria-label="interaction kind"
              >
                <option value="stimulus">voltage waveform</option>
                <option value="pushbutton">pushbutton</option>
                <option value="toggle">toggle switch</option>
              </select>
              <input
                className="hb-input min-w-0"
                style={{ width: 100 }}
                value={peripheral.id}
                aria-label="interaction id"
                placeholder="stable id"
                onChange={e => set({ id: e.target.value })}
              />
              <input
                className="hb-input min-w-0 flex-1"
                style={{ minWidth: 130 }}
                list="net-options"
                value={peripheral.net}
                aria-label="interaction net"
                placeholder="board net"
                onChange={e => set({ net: e.target.value })}
              />
              <RemoveButton className="ml-auto" onClick={() => onRemove(peripheral.rowId)} />
            </div>

            {peripheral.kind === 'stimulus' ? (
              <div className="flex flex-wrap items-center gap-2">
                <label className="text-[11px] flex items-center gap-1.5" style={{ color: 'var(--silk-faint)' }}>
                  waveform
                  <select
                    className="hb-input"
                    value={peripheral.waveform}
                    onChange={e => set({ waveform: e.target.value as PeripheralRow['waveform'] })}
                  >
                    <option value="dc">DC</option>
                    <option value="sine">sine</option>
                    <option value="noise">noise</option>
                  </select>
                </label>
                <Field label="offset (V)" value={peripheral.offset} width={72} onChange={v => set({ offset: v })} />
                {peripheral.waveform !== 'dc' && (
                  <>
                    <Field label="amplitude (V)" value={peripheral.amplitude} width={72} onChange={v => set({ amplitude: v })} />
                    <Field label="frequency (Hz)" value={peripheral.freq_hz} width={82} onChange={v => set({ freq_hz: v })} />
                  </>
                )}
              </div>
            ) : (
              <div className="flex flex-wrap items-center gap-2">
                <label className="text-[11px] flex items-center gap-1.5" style={{ color: 'var(--silk-faint)' }}>
                  other terminal
                  <input
                    className="hb-input"
                    style={{ width: 110 }}
                    list="net-options"
                    value={peripheral.to}
                    onChange={e => set({ to: e.target.value })}
                  />
                </label>
                <Field label="initial (0/1)" value={peripheral.initial} width={65} onChange={v => set({ initial: v })} />
                {peripheral.kind === 'pushbutton' && (
                  <Field label="bounce (ms)" value={peripheral.bounce_ms} width={70} onChange={v => set({ bounce_ms: v })} />
                )}
              </div>
            )}

            <div className="mt-2">
              <div className="flex flex-wrap items-center gap-2">
                <span className="text-[10px] font-bold tracking-wider uppercase" style={{ color: 'var(--silk-faint)' }}>
                  timeline
                </span>
                <AddInlineButton onClick={() => set({ events: [...peripheral.events, { t_ms: '10', value: '1' }] })}>
                  + event
                </AddInlineButton>
              </div>
              {peripheral.events.map((event, index) => (
                <div key={index} className="flex flex-wrap items-center gap-2 mt-1">
                  <Field label="at (ms)" value={event.t_ms} width={70} onChange={v => patchEvent(index, { t_ms: v })} />
                  <Field label="set value" value={event.value} width={70} onChange={v => patchEvent(index, { value: v })} />
                  <RemoveButton
                    label="remove event"
                    onClick={() => set({ events: peripheral.events.filter((_, i) => i !== index) })}
                  />
                </div>
              ))}
            </div>
            <RowIssues issues={issues} />
          </RowCard>
        )
      })}
    </BuilderSection>
  )
}
