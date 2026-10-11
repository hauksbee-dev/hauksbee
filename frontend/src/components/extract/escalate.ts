// The "Escalate to" setting: a ladder of model ids tried in order when the
// previous tier fails. In extract.toml it is absent (the backend's built-in
// ladder), an empty list (never escalate) or a list. In a text field it is
// blank, the word `none`, or a comma-separated list, exactly as
// `hauksbee models backend set <backend>.escalate=...` takes it.

/** The text field's contents for a saved value. */
export function escalateToText(value: readonly string[] | null | undefined): string {
  if (value === undefined || value === null) return ''
  return value.length === 0 ? 'none' : value.join(', ')
}

/** The value to save for the text field. `undefined` means leave the key out
 *  (blank = the default ladder), `[]` is `none`. */
export function escalateFromText(text: string): string[] | undefined {
  const trimmed = text.trim()
  if (trimmed === '') return undefined
  if (trimmed.toLowerCase() === 'none') return []
  const ids = trimmed.split(/[,\s]+/).filter(Boolean)
  return ids.length ? ids : undefined
}

/** "Haiku 5.5 → Sonnet 5.5 → Opus 5.5": the first model then its escalations.
 *  Names come from the server's model list (`labels`: id -> display name); an
 *  id the server gave no label for is shown as written. Nothing here knows
 *  which models exist or which is the default. */
export function ladderLabel(
  model: string,
  escalate: readonly string[] | undefined,
  labels: Readonly<Record<string, string>> = {},
): string {
  return [model, ...(escalate ?? [])].map(id => labels[id] ?? id).join(' → ')
}

/** id -> display name for every listed model that has a label. */
export function labelMap(models: readonly { name: string; label?: string }[] | undefined): Record<string, string> {
  const map: Record<string, string> = {}
  for (const m of models ?? []) if (m.label) map[m.name] = m.label
  return map
}
