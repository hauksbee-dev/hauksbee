import { useCallback, useEffect, useState } from 'react'
import { api, errorText } from '../lib/api'
import type { DepInfo, DepsEnvironment } from '../types/report'

// What this machine has (GET /api/deps, the engine's own discovery) and the
// one-click install flow (POST /api/deps/install/<id> as Server-Sent Events).
// Environment and Settings both show dependency rows with an Install button, so
// the fetch and the install state machine live here once.

export type InstallState =
  | { phase: 'idle' }
  | { phase: 'running'; id: string; log: string[] }
  | { phase: 'ended'; id: string; ok: boolean; message: string; log: string[] }

export type DepsFetch =
  | { phase: 'loading' }
  | { phase: 'ready'; deps: DepInfo[]; environment: DepsEnvironment | null }
  | { phase: 'unavailable'; reason: string }

export function useDeps(onInstalled?: () => void) {
  const [fetchState, setFetchState] = useState<DepsFetch>({ phase: 'loading' })
  const [install, setInstall] = useState<InstallState>({ phase: 'idle' })

  const refresh = useCallback(async () => {
    try {
      const json = await api.deps()
      if (!Array.isArray(json.deps)) throw new Error('it returned an unexpected shape')
      setFetchState({ phase: 'ready', deps: json.deps, environment: json.environment ?? null })
    } catch (e) {
      // An older server / a serve mode without the dependency endpoints: say
      // so honestly instead of rendering a blank page.
      setFetchState({ phase: 'unavailable', reason: `/api/deps failed: ${errorText(e)}` })
    }
  }, [])

  useEffect(() => { void refresh() }, [refresh])

  const runInstall = useCallback(async (id: string) => {
    setInstall({ phase: 'running', id, log: [] })
    const log: string[] = []
    const append = (line: string) => {
      log.push(line)
      setInstall({ phase: 'running', id, log: [...log] })
    }
    const end = (ok: boolean, message: string) => {
      setInstall({ phase: 'ended', id, ok, message, log: [...log] })
      void refresh()
      if (ok) onInstalled?.()
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
  }, [refresh, onInstalled])

  const reload = useCallback(() => { setFetchState({ phase: 'loading' }); void refresh() }, [refresh])

  return { fetchState, install, runInstall, refresh, reload }
}

/** The text a reviewer attaches to a sign-off record: the engine, the binary's
 *  hash, the host and every dependency's version and path, as the engine found
 *  them. Plain text so it pastes anywhere. */
export function environmentReport(deps: readonly DepInfo[], env: DepsEnvironment | null): string {
  const lines = ['Hauksbee environment report']
  if (env) {
    lines.push(`engine: hauksbee v${env.engine_version}`)
    if (env.exe_sha256) lines.push(`engine binary sha256: ${env.exe_sha256}`)
    lines.push(`host: ${env.os} ${env.arch}`)
  }
  for (const d of deps) {
    const state = d.present ? `installed${d.version ? ` ${d.version}` : ''}${d.path ? ` at ${d.path}` : ''}` : 'not installed'
    lines.push(`${d.name}: ${state}`)
  }
  return lines.join('\n')
}
