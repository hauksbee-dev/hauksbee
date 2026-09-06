import { useCallback, useEffect, useState } from 'react'
import { api, errorText } from '../lib/api'
import type { DepInfo } from '../types/report'
import { BusyLine, Callout, LogWell, TerminalCommand } from './ui'

// The dependency panel (landing page): which optional co-sim backends and
// oracles this machine has, what each unlocks, and a one-click install where
// the server can actually do it. Status comes from GET /api/deps (the engine's
// OWN discovery, the same resolvers a real run uses); installs stream live
// progress from POST /api/deps/install/<id> as Server-Sent Events. The manual
// terminal command is always shown too, so nobody is trapped in the browser.

type InstallState =
  | { phase: 'idle' }
  | { phase: 'running'; id: string; log: string[] }
  | { phase: 'ended'; id: string; ok: boolean; message: string; log: string[] }

type DepsFetch =
  | { phase: 'loading' }
  | { phase: 'ready'; deps: DepInfo[] }
  | { phase: 'unavailable'; reason: string }

export function DepsPanel({ engineVersion }: { engineVersion?: string | null }) {
  const [fetchState, setFetchState] = useState<DepsFetch>({ phase: 'loading' })
  const [install, setInstall] = useState<InstallState>({ phase: 'idle' })

  const fetchDeps = useCallback(async () => {
    try {
      const json = await api.deps()
      if (!Array.isArray(json.deps)) throw new Error('it returned an unexpected shape')
      setFetchState({ phase: 'ready', deps: json.deps })
    } catch (e) {
      // An older server / a serve mode without the dependency endpoints: say
      // so honestly instead of rendering a blank page.
      setFetchState({ phase: 'unavailable', reason: `/api/deps failed: ${errorText(e)}` })
    }
  }, [])

  useEffect(() => { void fetchDeps() }, [fetchDeps])

  const runInstall = useCallback(async (id: string) => {
    setInstall({ phase: 'running', id, log: [] })
    const log: string[] = []
    const append = (line: string) => {
      log.push(line)
      setInstall({ phase: 'running', id, log: [...log] })
    }
    const end = (ok: boolean, message: string) => {
      setInstall({ phase: 'ended', id, ok, message, log: [...log] })
      void fetchDeps()
    }
    try {
      let ended = false
      await api.depsInstall(id, ({ event, data }) => {
        if (event === 'log') append(data)
        else if (event === 'done') { ended = true; end(true, 'Installed and verified.') }
        else if (event === 'error') { ended = true; end(false, data) }
      })
      if (!ended) end(false, 'the connection closed before the install reported a result')
    } catch (e) {
      end(false, `install request failed: ${errorText(e)}`)
    }
  }, [fetchDeps])

  const deps = fetchState.phase === 'ready' ? fetchState.deps : []
  const missing = deps.filter(d => !d.present).length
  const busyId = install.phase === 'running' ? install.id : null
  const activeLog = install.phase === 'running' || install.phase === 'ended' ? install.log : []
  const activeId = install.phase === 'idle' ? null : install.id

  return (
    <div className="mt-10" data-testid="deps-panel">
      {/* What am I running: version + serving origin. Honest basics that make
          "is this the build I think it is" answerable from the browser. */}
      <div
        className="rounded-xl px-4 py-3 mb-8 text-[12px] flex flex-wrap gap-x-6 gap-y-1"
        style={{ border: '1px solid var(--hairline)', background: 'var(--surface)', color: 'var(--silk-dim)' }}
        data-testid="env-about"
      >
        <span>
          engine:{' '}
          <span style={{ color: 'var(--silk)', fontFamily: 'var(--font-mono)' }}>
            {engineVersion ? `hauksbee v${engineVersion}` : 'version not reported by this server'}
          </span>
        </span>
        <span>
          serving:{' '}
          <span style={{ color: 'var(--silk)', fontFamily: 'var(--font-mono)' }}>
            {window.location.origin}
          </span>
        </span>
      </div>

      <div
        className="text-[11px] font-semibold tracking-[0.2em] uppercase text-center mb-2"
        style={{ color: 'var(--silk-faint)', fontFamily: 'var(--font-mono)' }}
      >
        Simulators on this machine
      </div>

      {fetchState.phase === 'loading' && (
        <BusyLine className="justify-center mt-6" color="var(--silk-dim)">
          Probing the co-sim backends and oracles on this machine ...
        </BusyLine>
      )}

      {fetchState.phase === 'unavailable' && (
        <Callout tone="warn" testId="deps-unavailable" className="mt-4" title="Dependency status unavailable">
          This server does not expose the dependency endpoints: {fetchState.reason}. Backend
          availability and one-click installs need a full <code className="hb-inline">hauksbee serve</code>{' '}
          (or <code className="hb-inline">hauksbee run --serve</code>) session. From a terminal,{' '}
          <code className="hb-inline">hauksbee doctor --backends</code> shows backend availability.
          <div className="mt-2">
            <button
              type="button"
              onClick={() => { setFetchState({ phase: 'loading' }); void fetchDeps() }}
              className="hb-btn hb-press px-3 text-[12px]"
              style={{ height: 28 }}
            >
              Retry
            </button>
          </div>
        </Callout>
      )}

      {fetchState.phase === 'ready' && (
        <>
      <div className="text-[12px] text-center mb-5" style={{ color: 'var(--silk-dim)' }}>
        {missing === 0
          ? 'Everything optional is installed. All co-sim backends and oracles are ready.'
          : `Core hauksbee works now. ${missing} optional ${missing === 1 ? 'piece' : 'pieces'} would unlock more, install from here or from the terminal.`}
      </div>

      <div className="rounded-xl overflow-hidden" style={{ border: '1px solid var(--hairline)' }}>
        {deps.map((d, i) => (
          <div
            key={d.id}
            data-testid={`dep-${d.id}`}
            className="px-4 py-3"
            style={{
              background: 'var(--surface)',
              borderTop: i > 0 ? '1px solid var(--hairline)' : 'none',
            }}
          >
            <div className="flex items-start gap-3 flex-wrap">
              {/* status dot */}
              <span
                aria-hidden
                className="mt-1.5"
                style={{
                  width: 8,
                  height: 8,
                  borderRadius: 4,
                  flexShrink: 0,
                  background: d.present ? 'var(--ok)' : 'var(--err)',
                  boxShadow: d.present ? '0 0 8px rgba(87,224,160,0.5)' : '0 0 8px rgba(239,68,68,0.4)',
                  display: 'inline-block',
                }}
              />
              <div className="flex-1 min-w-0">
                <div className="text-[13px] font-semibold" style={{ color: 'var(--silk)' }}>
                  {d.name}
                  <span
                    className="ml-2 text-[10px] font-bold tracking-widest uppercase"
                    style={{ color: d.present ? 'var(--ok)' : 'var(--err-strong)' }}
                  >
                    {d.present ? 'installed' : 'missing'}
                  </span>
                  {d.version && (
                    <span className="ml-2 text-[11px] font-normal" style={{ color: 'var(--silk-faint)', fontFamily: 'var(--font-mono)' }}>
                      {d.version}
                    </span>
                  )}
                </div>
                <div className="text-[12px] mt-0.5 leading-relaxed" style={{ color: 'var(--silk-dim)' }}>
                  {d.present ? 'Unlocks' : 'Would unlock'}: {d.unlocks}
                </div>
                {d.sends_data_offhost && (
                  <div
                    data-testid={`dep-privacy-${d.id}`}
                    className="text-[11px] mt-1.5 leading-relaxed rounded-lg px-2 py-1.5"
                    style={{
                      color: 'var(--silk-dim)',
                      background: 'var(--warn-bg)',
                      border: '1px solid var(--warn-border)',
                    }}
                  >
                    <span className="font-semibold" style={{ color: 'var(--warn-strong)' }}>
                      Leaves your machine:{' '}
                    </span>
                    {d.sends_data_offhost}
                  </div>
                )}
                {d.present && d.path && (
                  <div
                    className="text-[11px] mt-0.5 truncate"
                    style={{ color: 'var(--silk-faint)', fontFamily: 'var(--font-mono)' }}
                    title={d.path}
                  >
                    {d.path}
                  </div>
                )}
                {!d.present && d.detail && (
                  <div className="text-[11px] mt-0.5 leading-relaxed" style={{ color: 'var(--silk-faint)' }}>
                    {d.detail}
                  </div>
                )}
                {/* The cost, for a dep with no Install button. Rendering it
                    only inside the install block hides it for a dependency we
                    deliberately never auto-install (codex: an account and a
                    login are the user's to give) could never show its cost at
                    all. That hid the single most persuasive fact about it,
                    "free if you already pay for ChatGPT", from the exact place
                    someone decides whether to bother. */}
                {!d.present && !d.installable && d.cost && (
                  <div className="text-[11px] mt-1 leading-relaxed" style={{ color: 'var(--silk-dim)' }}>
                    <span style={{ color: 'var(--silk-faint)' }}>Cost: </span>
                    {d.cost}
                  </div>
                )}
                {!d.present && d.manual && (
                  <div className="text-[11px] mt-1.5" style={{ color: 'var(--silk-faint)' }}>
                    <TerminalCommand command={d.manual} testId="dep-manual-copy" />
                  </div>
                )}
              </div>
              {/* Beside the description where the row is wide enough, on its own
                  line below it when it is not. Held to one line at every width,
                  the button and its cost caption ran 5px off a 320px viewport and
                  the caption took the click. */}
              {!d.present && d.installable && (
                <div className="flex flex-col items-end gap-1 w-full sm:w-auto min-w-0" style={{ flexShrink: 0 }}>
                  <button
                    type="button"
                    data-testid={`dep-install-${d.id}`}
                    disabled={busyId !== null}
                    onClick={() => void runInstall(d.id)}
                    className="rounded-lg px-3.5 py-1.5 text-[12px] font-semibold cursor-pointer transition-all hover:opacity-90 disabled:opacity-40 disabled:cursor-not-allowed"
                    style={{
                      background: 'linear-gradient(180deg, var(--copper-hi), var(--copper))',
                      color: 'var(--on-copper)',
                    }}
                  >
                    {busyId === d.id ? 'Installing ...' : 'Install'}
                  </button>
                  <span className="text-[10px] text-right max-w-full" style={{ color: 'var(--silk-faint)', maxWidth: '14rem' }}>
                    {d.cost}
                  </span>
                </div>
              )}
            </div>

            {/* Live install progress + final state, under the row it belongs to */}
            {activeId === d.id && (
              <div className="mt-2.5">
                <LogWell lines={activeLog} testId="dep-log" />
                {install.phase === 'running' && (
                  <BusyLine className="mt-1.5">
                    Installing; this can take a few minutes on a slow connection.
                  </BusyLine>
                )}
                {install.phase === 'ended' && (
                  <Callout
                    tone={install.ok ? 'ok' : 'err'}
                    testId="dep-install-result"
                    live
                    className="mt-1.5 px-3 py-2 text-[12px] whitespace-pre-wrap"
                  >
                    {install.ok
                      ? 'Installed and verified. The status above is refreshed from a real re-probe.'
                      : install.message}
                  </Callout>
                )}
              </div>
            )}
          </div>
        ))}
      </div>
        </>
      )}
    </div>
  )
}
