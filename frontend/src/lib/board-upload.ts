/** Build the one multipart contract shared by analysis and live launch. */
export function buildBoardUpload(
  board: File,
  firmware: File | null,
  schematic: File | null,
): FormData {
  const form = new FormData()
  form.append('board', board, board.name)
  if (firmware) form.append('firmware', firmware, firmware.name)
  if (schematic) form.append('schematic', schematic, schematic.name)
  return form
}

/** Build the Checks multipart without allowing that surface to drift from the
 * analysis/live companion-input contract. */
export function buildCheckUpload(
  board: File,
  firmware: File | null,
  schematic: File | null,
  spec: string,
): FormData {
  const form = buildBoardUpload(board, firmware, schematic)
  form.append('spec', new Blob([spec], { type: 'text/plain' }), 'spec.toml')
  return form
}
