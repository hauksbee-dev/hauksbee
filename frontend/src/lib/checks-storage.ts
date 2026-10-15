import type { WebReport } from '../types/report'
import { sessionIdFor } from './session-store'

/** Storage key for a board's saved checks. The file name alone collides for
 *  common names (every project has a board.kicad_pcb), so a cheap fingerprint
 *  from the report disambiguates. The shell also uses this as the Checks view's
 *  React key so it remounts per board: the mount-time restore is then
 *  authoritative and one board's state can never leak into another's. Kept
 *  apart from the view so the shell can key it without loading it. */
export function checksStorageKey(report: WebReport): string {
  return `hauksbee.checks.${sessionIdFor(report)}`
}
