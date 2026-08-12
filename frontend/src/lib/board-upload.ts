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
