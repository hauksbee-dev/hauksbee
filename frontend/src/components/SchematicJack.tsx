export function SchematicJack({
  schematic,
  onFile,
  onClear,
  locked,
}: {
  schematic: File | null
  onFile: (file: File) => void
  onClear: () => void
  locked: boolean
}) {
  return (
    <div className="mt-3 rounded-lg px-3 py-2 text-[12px] flex items-center gap-3" style={{ border: '1px solid var(--hairline)', color: 'var(--silk-dim)' }}>
      <span className="grow">
        Eagle schematic companion:{' '}
        <span style={{ fontFamily: 'var(--font-mono)', color: schematic ? 'var(--silk)' : 'var(--silk-faint)' }}>
          {schematic?.name ?? 'none (.sch, optional)'}
        </span>
      </span>
      {schematic ? (
        <button type="button" className="hb-press" disabled={locked} onClick={onClear}>Remove</button>
      ) : (
        <label className="hb-press cursor-pointer">
          Choose .sch
          <input
            data-testid="schematic-file"
            className="sr-only"
            type="file"
            accept=".sch,application/xml,text/xml"
            disabled={locked}
            onChange={event => {
              const file = event.currentTarget.files?.[0]
              if (file) onFile(file)
              event.currentTarget.value = ''
            }}
          />
        </label>
      )}
    </div>
  )
}
