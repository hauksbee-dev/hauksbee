import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import type { SimulationState } from '../hooks/useSimulation'
import type { BoardInfoMsg, ClientMessage, SimFault, SimFrame, StatusMsg } from '../types/protocol'
import type { LoadedSession } from './manifest'

// The replay SimSource: paces a recorded session along its sim-time axis at
// the chosen playback rate, and answers the transport messages the UI already
// sends (Play/Pause/Step/Reset/SetSpeed) against the recording. Messages that
// would CHANGE the sim (inputs, supplies, serial) are ignored: a recording's
// inputs are fixed, and the demo UI says so instead of pretending.
//
// Scrubbing rebuilds state by re-applying recorded messages from t=0 to the
// target. Sessions are a few hundred frames, so the rebuild is microseconds;
// no checkpointing needed.

/** Applied-state accumulator over the recorded lines. */
interface ApplyState {
  idx: number
  boardInfo: BoardInfoMsg | null
  frame: SimFrame | null
  options: StatusMsg['options'] | null
}

const freshState = (): ApplyState => ({ idx: 0, boardInfo: null, frame: null, options: null })

export function useReplaySimulation(session: LoadedSession): SimulationState {
  const { lines, duration, boardFileUrl } = session

  const [boardInfo, setBoardInfo] = useState<BoardInfoMsg | null>(null)
  const [frame, setFrame] = useState<SimFrame | null>(null)
  const [status, setStatus] = useState<StatusMsg | null>(null)
  const [position, setPosition] = useState(0)

  const state = useRef<ApplyState>(freshState())
  const playhead = useRef(0)
  const playing = useRef(false)
  const speed = useRef(1)

  /** Advance the accumulator to `target`, collecting the faults every applied
   *  frame carried. The server drains each fault into exactly one frame, so a
   *  seek that skips frames must surface their faults on the frame it lands
   *  on, or the fault log would depend on scrub granularity. */
  const applyTo = useCallback((target: number): SimFault[] => {
    const s = state.current
    const faults: SimFault[] = []
    while (s.idx < lines.length && lines[s.idx].at <= target) {
      const m = lines[s.idx].m
      if (m.type === 'SimFrame') {
        s.frame = m
        if (m.faults) faults.push(...m.faults)
      } else if (m.type === 'BoardInfo') {
        // The recorded board_url points at the capture server's /boards
        // route; on the static site the same bytes live in the session dir.
        s.boardInfo = { ...m, board_url: boardFileUrl }
      } else if (m.type === 'Status') {
        s.options = m.options
      }
      s.idx += 1
    }
    return faults
  }, [lines, boardFileUrl])

  /** Push the accumulator into React state. `running` is the REPLAY's own
   *  transport state; sim_time is the playhead. Everything else is recorded. */
  const emit = useCallback((carriedFaults: SimFault[]) => {
    const s = state.current
    setBoardInfo(prev => prev === s.boardInfo ? prev : s.boardInfo)
    setFrame(prev => {
      if (!s.frame) return prev
      if (carriedFaults.length > 0) return { ...s.frame, faults: carriedFaults }
      return prev === s.frame ? prev : s.frame
    })
    if (s.options) {
      setStatus({ type: 'Status', running: playing.current, sim_time: playhead.current, options: s.options })
    }
    setPosition(playhead.current)
  }, [])

  const seek = useCallback((t: number) => {
    const target = Math.max(0, Math.min(t, duration))
    if (target < playhead.current) state.current = freshState()
    playhead.current = target
    emit(applyTo(target))
  }, [applyTo, duration, emit])

  const send = useCallback((msg: ClientMessage) => {
    switch (msg.type) {
      case 'Play':
        // Play at the end of the recording means "watch it again".
        if (playhead.current >= duration) seek(0)
        playing.current = true
        emit([])
        break
      case 'Pause':
        playing.current = false
        emit([])
        break
      case 'SetSpeed':
        speed.current = Math.max(0.1, Math.min(msg.factor, 10))
        break
      case 'Step':
        playing.current = false
        seek(playhead.current + msg.dt)
        break
      case 'Reset':
        playing.current = false
        seek(0)
        break
      default:
        // AddProbe/RemoveProbe: the scope buffers from frames, nothing to do.
        // Everything else mutates a sim that is not running here.
        break
    }
  }, [duration, emit, seek])

  // The pacing loop: wall-clock delta times playback rate moves the playhead
  // along the recorded sim-time axis. rAF-driven, so the emit rate is the
  // render rate, the same latest-wins cadence the live source uses.
  useEffect(() => {
    let raf = 0
    let last = performance.now()
    const tick = (now: number) => {
      raf = requestAnimationFrame(tick)
      const dt = (now - last) / 1000
      last = now
      if (!playing.current) return
      playhead.current = Math.min(playhead.current + dt * speed.current, duration)
      const faults = applyTo(playhead.current)
      if (playhead.current >= duration) playing.current = false
      emit(faults)
    }
    raf = requestAnimationFrame(tick)
    return () => cancelAnimationFrame(raf)
  }, [applyTo, duration, emit])

  // Mount: surface BoardInfo and the first frame immediately, then autoplay.
  // A demo session should be moving when it opens; the visitor did not launch
  // anything, so there is no moment they would express "start".
  useEffect(() => {
    seek(0)
    playing.current = true
    // seek/emit/session identities are mount-constant (session is keyed).
  }, [seek])

  const replay = useMemo(() => ({ duration, position, seek }), [duration, position, seek])

  return {
    // A fully loaded recording is always "connected" in the socket sense;
    // the TransportBar labels it as a replay instead of a live link.
    connected: true,
    boardInfo,
    frame,
    status,
    probeData: [],
    send,
    replay,
  }
}
