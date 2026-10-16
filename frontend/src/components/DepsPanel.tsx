import { useEffect, useState } from 'react'
import { environmentReport, useDeps } from '../hooks/useDeps'
import { tildeHome } from '../lib/display-path'
import { InstallButton, InstallProgress } from './DepInstall'
import { LinkedText } from './LinkedText'
import { BusyLine, Callout, CopyButton, TerminalCommand } from './ui'

// The Environment page: which optional co-sim backends and oracles this machine
// has, what each unlocks, and a one-click install where the server can actually
// do it. Status comes from GET /api/deps (the engine's OWN discovery, the same
// resolvers a real run uses). Datasheet-extraction tools are NOT listed here:
// Settings owns that list (choose, install, sign in), and this page links to it.
// The manual terminal command is always shown too, so nobody is trapped in the
// browser.

/** The datasheet-drafting CLIs are the dependencies that send data off this
 *  machine (the engine marks them with `sends_data_offhost`); Settings owns them. */
const isExtractor = (d: { sends_data_offhost?: string | null }) => !!d.sends_data_offhost

/** The engine words some install hints for the terminal and the app alike ("in
 *  the app use Install on the Environment page"). This IS the Environment page,
 *  so say where the button is instead of naming the page we are on. */
const inThisPage = (text: string) =>
  text.replace(/in the app use Install on the\s+Environment page/g, 'use the Install button on this page')

/** After this long the first probe is slow, not stuck: say so. */
const SLOW_PROBE_MS = 4000

export function DepsPanel({ engineVersion, onOpenSettings }: { engineVersion?: string | null; onOpenSettings?: () => void }) {
  const { fetchState, install, runInstall, reload } = useDeps()
  const [slowProbe, setSlowProbe] = useState(false)

  useEffect(() => {
    if (fetchState.phase !== 'loading') { setSlowProbe(false); return }
    const timer = setTimeout(() => setSlowProbe(true), SLOW_PROBE_MS)
    return () => clearTimeout(timer)
  }, [fetchState.phase])

  const allDeps = fetchState.phase === 'ready' ? fetchState.deps : []
  const deps = allDeps.filter(d => !isExtractor(d))
  const missing = deps.filter(d => !d.present).length

  return (
    <div className="mt-10" data-testid="deps-panel">
      {/* What am I running: version + serving origin. Honest basics that make
          "is this the build I think it is" answerable from the browser. */}
      <div
        className="rounded-xl px-4 py-3 mb-8 text-[12px] flex flex-wrap items-center gap-x-6 gap-y-1"
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
        <span className="flex-1" />
        {fetchState.phase === 'ready' && (
          <CopyButton
            text={environmentReport(allDeps, fetchState.environment)}
            label="Copy environment report"
            testId="env-copy-report"
            small
          />
        )}
      </div>

      <div
        className="text-[11px] font-semibold tracking-[0.2em] uppercase text-center mb-2"
        style={{ color: 'var(--silk-faint)', fontFamily: 'var(--font-mono)' }}
      >
        Simulators on this machine
      </div>

      {fetchState.phase === 'loading' && (
        <BusyLine className="justify-center mt-6" color="var(--silk-dim)">
          Checking local dependency paths ...
        </BusyLine>
      )}
      {fetchState.phase === 'loading' && slowProbe && (
        <div className="text-[12px] text-center mt-2" data-testid="deps-slow" style={{ color: 'var(--silk-faint)' }}>
          This is slower than usual. The check looks for each simulator on this machine, which can take
          several seconds the first time; the list appears when it finishes.
        </div>
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
              onClick={reload}
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
                style={{ background: 'var(--surface)', borderTop: i > 0 ? '1px solid var(--hairline)' : 'none' }}
              >
                <div className="flex items-start gap-3 flex-wrap">
                  <span
                    aria-hidden
                    className="mt-1.5"
                    style={{
                      width: 8,
                      height: 8,
                      borderRadius: 4,
                      flexShrink: 0,
                      // A missing tool here is optional, not a fault: neutral, not red.
                      background: d.present ? 'var(--ok)' : 'var(--silk-faint)',
                      boxShadow: d.present ? '0 0 8px rgba(87,224,160,0.5)' : 'none',
                      display: 'inline-block',
                    }}
                  />
                  <div className="flex-1 min-w-0">
                    <div className="text-[13px] font-semibold" style={{ color: 'var(--silk)' }}>
                      {d.name}
                      <span
                        className="ml-2 text-[10px] font-bold tracking-widest uppercase"
                        style={{ color: d.present ? 'var(--ok)' : 'var(--silk-faint)' }}
                        data-testid={`dep-status-${d.id}`}
                      >
                        {d.present ? 'installed' : 'optional · not installed'}
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
                    {d.present && d.path && (
                      <div
                        className="text-[11px] mt-0.5 truncate"
                        style={{ color: 'var(--silk-faint)', fontFamily: 'var(--font-mono)' }}
                        title={d.path}
                      >
                        {tildeHome(d.path)}
                      </div>
                    )}
                    {d.detail && (
                      <div className="text-[11px] mt-0.5 leading-relaxed" style={{ color: 'var(--silk-faint)' }}>
                        <LinkedText text={inThisPage(tildeHome(d.detail))} />
                      </div>
                    )}
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
                  {!d.present && d.installable && <InstallButton dep={d} install={install} onInstall={id => void runInstall(id)} />}
                </div>
                <InstallProgress id={d.id} install={install} />
              </div>
            ))}
          </div>

          {/* Datasheet drafting has one owner: Settings chooses, installs and
              signs in the tool. This is the pointer, not a second list. */}
          <div
            className="mt-4 rounded-xl px-4 py-3 text-[12px] flex flex-wrap items-center justify-between gap-2"
            style={{ border: '1px solid var(--hairline)', background: 'var(--surface)', color: 'var(--silk-dim)' }}
            data-testid="env-extraction-link"
          >
            <span>Drafting models from datasheets (Claude Code, Codex, Antigravity, API) is set up in Settings.</span>
            {onOpenSettings && (
              <button type="button" data-testid="env-open-settings" onClick={onOpenSettings} className="hb-btn hb-press px-3 text-[12px]" style={{ height: 28 }}>
                Open Settings
              </button>
            )}
          </div>
        </>
      )}
    </div>
  )
}
