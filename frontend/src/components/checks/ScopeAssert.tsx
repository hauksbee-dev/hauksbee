import { useEffect, useState, type RefObject } from 'react'
import type { ScopeExport } from '../ProbeScopePanel'
import { displayNet } from '../../lib/net-name'
import type { Observation } from '../../lib/check-catalog'
import { AssertThis } from './CheckThis'

// "Assert this" for the live scope: pick one of the probed traces and turn what
// it has drawn into a check. The samples are read from the scope's own buffer
// when a button is pressed (the same `series()` the export menu reads), so the
// scope panel itself needs no change. Mount it beside the scope:
//
//   <ScopeAssert scope={scopeRef} probes={probes} />

export function ScopeAssert({ scope, probes }: {
  scope: RefObject<ScopeExport | null>
  probes: readonly string[]
}) {
  const [picked, setPicked] = useState<string | null>(null)
  const net = picked !== null && probes.includes(picked) ? picked : probes[0]
  // A detached probe leaves the choice pointing nowhere; fall back to the first.
  useEffect(() => { if (picked !== null && !probes.includes(picked)) setPicked(null) }, [picked, probes])
  if (!net) return null
  const observation = (): Observation => ({
    kind: 'trace', net,
    points: scope.current?.series().find(s => s.net === net)?.points ?? [],
  })
  return (
    <div data-testid="scope-assert" className="flex flex-col gap-1.5 px-2 pb-2">
      <div className="flex flex-wrap items-center gap-x-2 gap-y-1 text-[11px]" style={{ color: 'var(--silk-dim)' }}>
        {probes.length > 1
          ? (
            <label className="flex items-center gap-1.5">
              Turn the trace of
              <select
                data-testid="scope-assert-net" value={net} onChange={event => setPicked(event.target.value)}
                className="text-[11px] rounded-md px-1.5 py-0.5"
                style={{ background: 'var(--surface-2)', color: 'var(--silk)', border: '1px solid var(--hairline)', fontFamily: 'var(--font-mono)' }}
              >
                {probes.map(p => <option key={p} value={p}>{displayNet(p)}</option>)}
              </select>
              into a check:
            </label>
          )
          : <span>Turn the trace of <span style={{ fontFamily: 'var(--font-mono)', color: 'var(--silk)' }}>{displayNet(net)}</span> into a check:</span>}
      </div>
      <AssertThis observation={observation} testIdSuffix="-scope" />
    </div>
  )
}
