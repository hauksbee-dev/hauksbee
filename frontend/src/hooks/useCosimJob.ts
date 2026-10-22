import { useCallback, useEffect, useRef, useState } from 'react'
import { cosimProgressLine, errorText, isAbort, runCosimJob } from '../lib/api'
import type { WebCosimSection } from '../types/report'

// The emulator co-sim as the page sees it: idle, running with a progress line,
// or failed with the server's own words (and a retry). One job at a time; a new
// analysis run (`epoch`) or an unmount cancels the one in flight, so a late
// answer never lands on a report it does not belong to.

export type CosimJobState =
  | { phase: 'idle' }
  | { phase: 'running'; line: string }
  | { phase: 'failed'; error: string }

export interface CosimJob {
  state: CosimJobState
  /** Start (or retry) the job. */
  start: () => void
}

export function useCosimJob(opts: {
  /** The multipart body the analysis used; null when the files are gone. */
  upload: () => FormData | null
  /** The finished section. */
  onDone: (section: WebCosimSection) => void
  /** Changes with every analysis run: the job belongs to one of them. */
  epoch: number
}): CosimJob {
  const { upload, onDone, epoch } = opts
  const [state, setState] = useState<CosimJobState>({ phase: 'idle' })
  const abort = useRef<AbortController | null>(null)
  const latest = useRef({ upload, onDone })
  latest.current = { upload, onDone }

  useEffect(() => {
    setState({ phase: 'idle' })
    return () => abort.current?.abort()
  }, [epoch])

  const start = useCallback(() => {
    const body = latest.current.upload()
    if (!body) {
      setState({ phase: 'failed', error: 'The original files are no longer here, so the emulator cannot be started from the page.' })
      return
    }
    abort.current?.abort()
    const ctl = new AbortController()
    abort.current = ctl
    setState({ phase: 'running', line: cosimProgressLine() })
    runCosimJob(body, {
      signal: ctl.signal,
      onProgress: (done, total) => {
        if (!ctl.signal.aborted) setState({ phase: 'running', line: cosimProgressLine(done, total) })
      },
    }).then(section => {
      if (ctl.signal.aborted) return
      setState({ phase: 'idle' })
      latest.current.onDone(section)
    }).catch((e: unknown) => {
      if (isAbort(e) || ctl.signal.aborted) return
      setState({ phase: 'failed', error: errorText(e) })
    })
  }, [])

  return { state, start }
}
