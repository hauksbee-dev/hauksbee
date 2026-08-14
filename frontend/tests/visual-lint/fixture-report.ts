/**
 * Replay the captured engine report without pretending every uploaded board is
 * the capture's source file. Session identity is derived from `file_name`, so
 * preserving the request name is necessary to exercise multi-board behavior.
 */
export function withBoardIdentity<T extends Record<string, unknown>>(
  captured: T,
  fileName: string | null,
  layoutSha256?: string | null,
): T {
  if (!fileName) return { ...captured }
  const inventory = Array.isArray(captured.inventory)
    ? captured.inventory.map((artifact: unknown) => {
      if (!artifact || typeof artifact !== 'object') return artifact
      const row = artifact as Record<string, unknown>
      return row.role === 'layout' && layoutSha256
        ? { ...row, path: fileName, sha256: layoutSha256 }
        : row
    })
    : captured.inventory
  return { ...captured, file_name: fileName, inventory }
}
