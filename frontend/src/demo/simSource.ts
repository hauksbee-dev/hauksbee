import { createContext } from 'react'
import type { SimulationState } from '../hooks/useSimulation'

// The SimSource seam: everything downstream of useSimulation (board view,
// scope, panels) reads one SimulationState, so swapping how that state is
// produced swaps live-vs-replay without touching the rendering code. The
// context carries a HOOK, not a state object, because both implementations
// own timers/sockets that must live inside React's lifecycle.
//
// Contract: the provided hook's identity must be fixed for the lifetime of
// the consuming component (the demo shell keys SimView by scenario, so a
// scenario switch remounts rather than swapping hooks under a live mount).
export type SimulationHook = () => SimulationState

/** Null means "no override": useSimulation falls back to the live WebSocket
 *  source, which is the entire non-demo app's path, unchanged. */
export const SimSourceContext = createContext<SimulationHook | null>(null)
