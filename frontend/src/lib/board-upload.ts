export interface SupplementalDesignFiles {
  bom: File | null
  placement: File | null
  variant: File | null
  asbuilt: File | null
  models: File[]
}

/** Build the one multipart contract shared by analysis and live launch. */
export function buildBoardUpload(
  board: File,
  firmware: File | null,
  schematic: File | null,
  supplemental?: SupplementalDesignFiles,
): FormData {
  const form = new FormData()
  form.append('board', board, board.name)
  if (firmware) form.append('firmware', firmware, firmware.name)
  if (schematic) form.append('schematic', schematic, schematic.name)
  if (supplemental?.bom) form.append('bom', supplemental.bom, supplemental.bom.name)
  if (supplemental?.placement) {
    form.append('placement', supplemental.placement, supplemental.placement.name)
  }
  if (supplemental?.variant) {
    form.append('variant', supplemental.variant, supplemental.variant.name)
  }
  if (supplemental?.asbuilt) {
    form.append('asbuilt', supplemental.asbuilt, supplemental.asbuilt.name)
  }
  for (const model of supplemental?.models ?? []) {
    form.append('model_file', model, model.name)
  }
  return form
}

/** Build the checks multipart from the same exact design inputs as analysis. */
export function buildCheckUpload(
  board: File,
  firmware: File | null,
  schematic: File | null,
  spec: string,
  supplemental?: SupplementalDesignFiles,
): FormData {
  const form = buildBoardUpload(board, firmware, schematic, supplemental)
  form.append('spec', new Blob([spec], { type: 'text/plain' }), 'spec.toml')
  return form
}

/** Compose the portable spec the Checks pane displays, downloads, and exports. */
export function buildPortableCheckSpec(
  boardName: string,
  firmware: File | null,
  schematic: File | null,
  body: string,
  includeFirmware = firmware !== null,
  supplemental?: SupplementalDesignFiles,
): string {
  // Raw mode may contain a firmware path the builder did not create. When the
  // user explicitly chooses "run without firmware", remove that path from the
  // portable artifact too; otherwise the downloaded spec would still request
  // the companion even though the in-browser run omitted it.
  const portableBody = includeFirmware
    ? body
    : body.replace(/^\s*firmware\s*=.*(?:\r?\n|$)/gm, '')
  const selectedFirmware = includeFirmware ? firmware : null
  const hasKey = (key: string) => new RegExp(`^\\s*${key}\\s*=`, 'm').test(portableBody)
  const tomlString = (value: string) => JSON.stringify(value)
  let head = ''
  if (!hasKey('board')) head += `board = ${tomlString(`../hardware/${boardName}`)}\n`
  if (selectedFirmware && !hasKey('firmware')) {
    head += `firmware = ${tomlString(`../firmware/${selectedFirmware.name}`)}\n`
  }
  if (schematic && !hasKey('schematic')) {
    head += `schematic = ${tomlString(`../hardware/${schematic.name}`)}\n`
  }
  if (supplemental?.bom && !hasKey('bom')) {
    head += `bom = ${tomlString(`../manufacturing/${supplemental.bom.name}`)}\n`
  }
  if (supplemental?.placement && !hasKey('placement')) {
    head += `placement = ${tomlString(`../manufacturing/${supplemental.placement.name}`)}\n`
  }
  if (supplemental?.variant && !hasKey('variant')) {
    head += `variant = ${tomlString(`../manufacturing/${supplemental.variant.name}`)}\n`
  }
  if (supplemental?.asbuilt && !hasKey('asbuilt')) {
    head += `asbuilt = ${tomlString(`../hardware/${supplemental.asbuilt.name}`)}\n`
  }
  if (supplemental?.models.length && !hasKey('models_dir')) {
    head += 'models_dir = "../models"\n'
  }
  return head ? `${head}\n${portableBody}` : portableBody
}
