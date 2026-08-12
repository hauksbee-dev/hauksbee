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

/** Build the checks multipart from the same exact design inputs as analysis. */
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

/** Compose the portable spec the Checks pane displays, downloads, and exports. */
export function buildPortableCheckSpec(
  boardName: string,
  firmware: File | null,
  schematic: File | null,
  body: string,
): string {
  const hasKey = (key: string) => new RegExp(`^\\s*${key}\\s*=`, 'm').test(body)
  const tomlString = (value: string) => JSON.stringify(value)
  let head = ''
  if (!hasKey('board')) head += `board = ${tomlString(`../hardware/${boardName}`)}\n`
  if (firmware && !hasKey('firmware')) {
    head += `firmware = ${tomlString(`../firmware/${firmware.name}`)}\n`
  }
  if (schematic && !hasKey('schematic')) {
    head += `schematic = ${tomlString(`../hardware/${schematic.name}`)}\n`
  }
  return head ? `${head}\n${body}` : body
}
