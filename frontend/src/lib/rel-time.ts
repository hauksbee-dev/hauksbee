// "just now", "4 min ago", "2 h ago", "3 d ago".
//
// Shared by the rail's board card and the session switcher, which both answer
// the same question about the same clock. Two copies of it would eventually
// round differently in the same column of the same panel.

/** `ts` and `now` are both client-clock milliseconds. */
export function relTime(ts: number, now: number): string {
  const s = Math.max(0, Math.round((now - ts) / 1000))
  if (s < 45) return 'just now'
  const m = Math.round(s / 60)
  if (m < 60) return `${m} min ago`
  const h = Math.round(m / 60)
  if (h < 24) return `${h} h ago`
  return `${Math.round(h / 24)} d ago`
}
