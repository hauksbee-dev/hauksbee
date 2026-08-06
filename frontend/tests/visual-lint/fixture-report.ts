/**
 * Replay the captured engine report without pretending every uploaded board is
 * the capture's source file. Session identity is derived from `file_name`, so
 * preserving the request name is necessary to exercise multi-board behavior.
 */
export function withBoardIdentity<T extends Record<string, unknown>>(
  captured: T,
  fileName: string | null,
): T {
  if (!fileName) return { ...captured }
  return { ...captured, file_name: fileName }
}
