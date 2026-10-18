// A large report arrives without anything written twice (the engine's
// `web_transport` module):
//
//  * a list of same-shaped records is a table, `{"$table": [fields], "rows":
//    [[cells]], "const": {field: value}}`, so the field names appear once and a
//    field with the same value in every record is stated once;
//  * a value that occurs more than once is stored once in a root `$defs` array
//    and each use is `{"$ref": index}`.
//
// This puts both back, so everything past the fetch (the views, the session
// store, the JSON export) works with the whole report and never sees either
// form. A report with neither is returned as it came, which is every small
// board's.

const REF_KEY = '$ref'
const DEFS_KEY = '$defs'
const TABLE_KEY = '$table'
const ROWS_KEY = 'rows'
const CONST_KEY = 'const'

type Json = unknown

const isRecord = (value: Json): value is Record<string, Json> =>
  typeof value === 'object' && value !== null && !Array.isArray(value)

/** The index a `{"$ref": n}` node names, or null for any other value. */
function refIndex(node: Record<string, Json>, size: number): number | null {
  const keys = Object.keys(node)
  if (keys.length !== 1 || keys[0] !== REF_KEY) return null
  const index = node[REF_KEY]
  return typeof index === 'number' && Number.isInteger(index) && index >= 0 && index < size ? index : null
}

/** The records of a table node whose cells are already expanded, or null. */
function tableRecords(node: Record<string, Json>): Array<Record<string, Json>> | null {
  const keys = Object.keys(node)
  const fields = node[TABLE_KEY]
  const rows = node[ROWS_KEY]
  const constants = node[CONST_KEY]
  if (!Array.isArray(fields) || !Array.isArray(rows)) return null
  if (keys.length === 3 ? !isRecord(constants) : keys.length !== 2) return null
  if (!fields.every(field => typeof field === 'string')) return null
  if (!rows.every(row => Array.isArray(row) && row.length === fields.length)) return null
  return rows.map((row: Json[]) => {
    const record: Record<string, Json> = isRecord(constants) ? { ...constants } : {}
    fields.forEach((field: string, column) => { record[field] = row[column] })
    return record
  })
}

/** Replace every reference with the value it names and every table with its
 *  records. Repeats share one object in memory; nothing mutates a report in
 *  place. */
export function restoreRepeats<T>(document: T): T {
  if (!isRecord(document)) return document
  const defs = Array.isArray(document[DEFS_KEY]) ? document[DEFS_KEY] as Json[] : []
  const resolved: Array<Json | undefined> = new Array(defs.length)
  const done: boolean[] = new Array(defs.length).fill(false)

  const expand = (node: Json): Json => {
    if (Array.isArray(node)) {
      for (let i = 0; i < node.length; i++) node[i] = expand(node[i])
      return node
    }
    if (!isRecord(node)) return node
    const index = refIndex(node, defs.length)
    if (index !== null) {
      if (!done[index]) {
        done[index] = true
        resolved[index] = expand(defs[index])
      }
      return resolved[index]
    }
    for (const key of Object.keys(node)) node[key] = expand(node[key])
    return tableRecords(node) ?? node
  }

  delete document[DEFS_KEY]
  return expand(document) as T
}
