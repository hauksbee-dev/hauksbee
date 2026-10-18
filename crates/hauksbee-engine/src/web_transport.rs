//! How a large web report travels to the browser: nothing is sent twice.
//!
//! A board with tens of thousands of parts repeats the same text and the same
//! small records in every part's row (the same model source record, the same
//! one-line explanation, the same footprint name), and every row of a list
//! repeats the same field names. Two lossless steps remove that:
//!
//! * A list of at least [`MIN_TABLE_ROWS`] records that all have the same fields
//!   is sent as a table, `{"$table": [fields], "rows": [[cells]], "const": {..}}`,
//!   so the field names are written once, and a field with the same value in
//!   every record is written once, under `const`.
//! * Every other value that occurs more than once is stored once in a root
//!   `$defs` array, and each use is replaced by `{"$ref": <index>}`.
//!
//! The browser puts both back before anything reads the report
//! (`frontend/src/lib/intern.ts`), so the report it works with, and the JSON it
//! exports, are the complete document.
//!
//! This is a transport step, not a report shape: it applies only to a report
//! past [`INTERN_ABOVE_BYTES`], so every small board's response is unchanged,
//! and the library entry points (`analyze_json` and the MCP tools built on it)
//! never apply it.

use serde_json::{Map, Value};
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

/// Reports at or below this many bytes are sent as they are.
pub const INTERN_ABOVE_BYTES: usize = 256 * 1024;

/// A value shorter than this is cheaper to send than its reference
/// (`{"$ref":12345}` is 14 bytes).
const MIN_VALUE_BYTES: usize = 32;

/// Values longer than this are not compared; a value this large is a table of
/// its own, not a repeated cell, and its parts are still shared on their own.
const MAX_VALUE_BYTES: usize = 8 * 1024;

/// A list shorter than this stays a list of records.
pub const MIN_TABLE_ROWS: usize = 3;

/// The reserved keys. Reports never use it; a report that did is sent whole.
const REF_KEY: &str = "$ref";
const DEFS_KEY: &str = "$defs";
const TABLE_KEY: &str = "$table";
const ROWS_KEY: &str = "rows";
const CONST_KEY: &str = "const";

/// Identity of a value: two independent 64-bit hashes of its canonical text,
/// so equal values meet and a collision between different ones is not a
/// practical event (about 2^-128 per pair).
type Fingerprint = (u64, u64);

fn fingerprint(text: &str) -> Fingerprint {
    let mut first = DefaultHasher::new();
    text.hash(&mut first);
    let mut second = DefaultHasher::new();
    0x9e37_79b9_7f4a_7c15_u64.hash(&mut second);
    text.hash(&mut second);
    text.len().hash(&mut second);
    (first.finish(), second.finish())
}

/// What the interning pass did, for the measurement tooling.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InternStats {
    /// Distinct values moved into `$defs`.
    pub defs: usize,
    /// Uses replaced by a reference.
    pub references: usize,
}

/// One pass over the document. In counting mode it only tallies how often each
/// candidate value occurs; in rewriting mode it replaces the repeated ones.
struct Pass<'a> {
    counts: &'a mut HashMap<Fingerprint, u32>,
    /// Set (rewriting mode) to the values worth a table entry.
    repeated: Option<&'a HashSet<Fingerprint>>,
    ids: HashMap<Fingerprint, usize>,
    defs: Vec<Value>,
    references: usize,
    /// A document that already uses the reserved key is left alone.
    reserved_key_seen: bool,
}

impl Pass<'_> {
    /// Visit `value` bottom-up. Returns its canonical JSON text when that is
    /// short enough to compare, `None` when the value is too large to be a
    /// candidate (so none of its ancestors is either).
    fn visit(&mut self, value: &mut Value) -> Option<String> {
        let text = match value {
            Value::Null => Some("null".to_string()),
            Value::Bool(b) => Some(b.to_string()),
            Value::Number(n) => Some(n.to_string()),
            Value::String(s) => {
                (s.len() <= MAX_VALUE_BYTES).then(|| serde_json::to_string(s).unwrap_or_default())
            }
            Value::Array(items) => {
                let mut parts = Some(Vec::with_capacity(items.len()));
                for item in items.iter_mut() {
                    let child = self.visit(item);
                    parts = match (parts, child) {
                        (Some(mut all), Some(text)) => {
                            all.push(text);
                            Some(all)
                        }
                        _ => None,
                    };
                }
                parts
                    .map(|all| format!("[{}]", all.join(",")))
                    .filter(|text| text.len() <= MAX_VALUE_BYTES)
            }
            Value::Object(map) => {
                if map.len() == 1 && map.contains_key(REF_KEY) {
                    self.reserved_key_seen = true;
                }
                let mut parts = Some(Vec::with_capacity(map.len()));
                for (key, item) in map.iter_mut() {
                    let child = self.visit(item);
                    parts = match (parts, child) {
                        (Some(mut all), Some(text)) => {
                            all.push(format!(
                                "{}:{text}",
                                serde_json::to_string(key).unwrap_or_default()
                            ));
                            Some(all)
                        }
                        _ => None,
                    };
                }
                parts
                    .map(|all| format!("{{{}}}", all.join(",")))
                    .filter(|text| text.len() <= MAX_VALUE_BYTES)
            }
        }?;
        if text.len() < MIN_VALUE_BYTES {
            return Some(text);
        }
        let print = fingerprint(&text);
        match self.repeated {
            None => {
                *self.counts.entry(print).or_insert(0) += 1;
            }
            Some(repeated) if repeated.contains(&print) => {
                let next = self.defs.len();
                let id = *self.ids.entry(print).or_insert(next);
                let mut placeholder = Map::new();
                placeholder.insert(REF_KEY.to_string(), Value::from(id));
                let placeholder = Value::Object(placeholder);
                let original = std::mem::replace(value, placeholder);
                if id == next {
                    self.defs.push(original);
                }
                self.references += 1;
            }
            Some(_) => {}
        }
        Some(text)
    }
}

/// True when any object in `value` uses a key this transport reserves.
fn uses_reserved_key(value: &Value) -> bool {
    match value {
        Value::Array(items) => items.iter().any(uses_reserved_key),
        Value::Object(map) => {
            map.contains_key(TABLE_KEY)
                || map.contains_key(DEFS_KEY)
                || map.values().any(uses_reserved_key)
        }
        _ => false,
    }
}

/// Rewrite every list of same-shaped records as a table. Returns how many
/// lists became tables. Bottom-up, so a table of records whose cells hold
/// lists of records is tabulated at both levels.
pub fn tabulate(value: &mut Value) -> usize {
    match value {
        Value::Object(map) => map.values_mut().map(tabulate).sum(),
        Value::Array(items) => {
            let mut tables: usize = items.iter_mut().map(tabulate).sum();
            if let Some(table) = as_table(items) {
                *value = table;
                tables += 1;
            }
            tables
        }
        _ => 0,
    }
}

/// `items` as a table, when it is a list of records with one shared field list
/// and the table is smaller than the list. A field whose value is the same in
/// every record is stated once, under `const`, instead of in every row.
fn as_table(items: &mut [Value]) -> Option<Value> {
    if items.len() < MIN_TABLE_ROWS {
        return None;
    }
    let Value::Object(first) = items.first()? else {
        return None;
    };
    let fields: Vec<String> = first.keys().cloned().collect();
    if fields.len() < 2 {
        return None;
    }
    let same_shape = items.iter().all(|item| {
        item.as_object()
            .is_some_and(|map| map.len() == fields.len() && map.keys().eq(fields.iter()))
    });
    if !same_shape {
        return None;
    }
    // A field is constant when every record carries the first record's value.
    // At least one field stays in the rows, so a row is never empty.
    let mut constant: Vec<bool> = vec![true; fields.len()];
    for item in items.iter().skip(1) {
        let Value::Object(map) = item else {
            return None;
        };
        for (flag, (cell, reference)) in constant.iter_mut().zip(map.values().zip(first.values())) {
            *flag = *flag && cell == reference;
        }
    }
    if constant.iter().all(|&flag| flag) {
        constant[0] = false;
    }
    // Bytes the plain list spends on field names, and on constant values, in
    // every record after the first; the table spends them once.
    let per_row: usize = fields
        .iter()
        .zip(&constant)
        .zip(first.values())
        .map(|((field, &flag), cell)| {
            field.len()
                + 3
                + if flag {
                    serde_json::to_string(cell).map_or(0, |text| text.len())
                } else {
                    0
                }
        })
        .sum();
    let wrapper = per_row + TABLE_KEY.len() + ROWS_KEY.len() + CONST_KEY.len() + 24;
    if per_row * items.len() < wrapper + 64 {
        return None;
    }
    let constants: Map<String, Value> = fields
        .iter()
        .zip(&constant)
        .zip(first.values())
        .filter(|((_, &flag), _)| flag)
        .map(|((field, _), cell)| (field.clone(), cell.clone()))
        .collect();
    let rows: Vec<Value> = items
        .iter_mut()
        .map(|item| {
            let Value::Object(map) = item else {
                return Value::Null;
            };
            Value::Array(
                std::mem::take(map)
                    .into_iter()
                    .zip(&constant)
                    .filter(|(_, &flag)| !flag)
                    .map(|((_, cell), _)| cell)
                    .collect(),
            )
        })
        .collect();
    let mut table = Map::new();
    table.insert(
        TABLE_KEY.to_string(),
        Value::Array(
            fields
                .into_iter()
                .zip(&constant)
                .filter(|(_, &flag)| !flag)
                .map(|(field, _)| Value::String(field))
                .collect(),
        ),
    );
    table.insert(ROWS_KEY.to_string(), Value::Array(rows));
    if !constants.is_empty() {
        table.insert(CONST_KEY.to_string(), Value::Object(constants));
    }
    Some(Value::Object(table))
}

/// The records of `map` when it is a table written by [`as_table`].
fn table_records(map: &Map<String, Value>) -> Option<Vec<Value>> {
    let constants = match map.len() {
        2 => None,
        3 => Some(map.get(CONST_KEY)?.as_object()?),
        _ => return None,
    };
    let fields = map.get(TABLE_KEY)?.as_array()?;
    let rows = map.get(ROWS_KEY)?.as_array()?;
    let names: Vec<&str> = fields.iter().map(Value::as_str).collect::<Option<_>>()?;
    rows.iter()
        .map(|row| {
            let cells = row.as_array().filter(|cells| cells.len() == names.len())?;
            let mut record: Map<String, Value> = constants.cloned().unwrap_or_default();
            for (name, cell) in names.iter().zip(cells) {
                record.insert((*name).to_string(), cell.clone());
            }
            Some(Value::Object(record))
        })
        .collect()
}

/// Store every repeated value of `document` once in a root `$defs` array.
///
/// Each use is replaced with `{"$ref": index}`, and a `$defs` entry may itself
/// contain references. Returns `None` and leaves the document untouched when it is not
/// an object, already uses the reserved keys, or has nothing worth sharing.
pub fn intern_repeats(document: &mut Value) -> Option<InternStats> {
    if !document.is_object() || document.get(DEFS_KEY).is_some() {
        return None;
    }
    let mut counts = HashMap::new();
    let mut counting = Pass {
        counts: &mut counts,
        repeated: None,
        ids: HashMap::new(),
        defs: Vec::new(),
        references: 0,
        reserved_key_seen: false,
    };
    // Counting does not modify the value; the clone-free `&mut` is shared with
    // the rewriting pass below.
    counting.visit(document);
    if counting.reserved_key_seen {
        return None;
    }
    let repeated: HashSet<Fingerprint> = counts
        .iter()
        .filter(|&(_, &n)| n > 1)
        .map(|(&print, _)| print)
        .collect();
    if repeated.is_empty() {
        return None;
    }
    let mut scratch = HashMap::new();
    let mut rewriting = Pass {
        counts: &mut scratch,
        repeated: Some(&repeated),
        ids: HashMap::new(),
        defs: Vec::new(),
        references: 0,
        reserved_key_seen: false,
    };
    rewriting.visit(document);
    let stats = InternStats {
        defs: rewriting.defs.len(),
        references: rewriting.references,
    };
    let defs = std::mem::take(&mut rewriting.defs);
    let Value::Object(root) = document else {
        return None;
    };
    insert_defs(root, defs);
    Some(stats)
}

fn insert_defs(root: &mut Map<String, Value>, defs: Vec<Value>) {
    root.insert(DEFS_KEY.to_string(), Value::Array(defs));
}

/// Put the document back as it was.
///
/// Every `{"$ref": n}` is replaced by the value it names and every table is
/// turned back into its list of records. The inverse of
/// [`shrink`]; the browser does the same in `frontend/src/lib/intern.ts`.
pub fn restore(document: &mut Value) {
    let mut defs = match document {
        Value::Object(root) => match root.remove(DEFS_KEY) {
            Some(Value::Array(defs)) => defs,
            _ => Vec::new(),
        },
        _ => Vec::new(),
    };
    let mut resolved: Vec<Option<Value>> = vec![None; defs.len()];
    for index in 0..defs.len() {
        resolve(index, &mut defs, &mut resolved);
    }
    expand(document, &mut defs, &mut resolved);
}

fn resolve(index: usize, defs: &mut [Value], resolved: &mut [Option<Value>]) {
    if resolved[index].is_some() {
        return;
    }
    let mut value = std::mem::take(&mut defs[index]);
    expand(&mut value, defs, resolved);
    resolved[index] = Some(value);
}

fn expand(value: &mut Value, defs: &mut [Value], resolved: &mut [Option<Value>]) {
    match value {
        Value::Array(items) => {
            for item in items {
                expand(item, defs, resolved);
            }
        }
        Value::Object(map) => {
            if map.len() == 1 {
                if let Some(index) = map
                    .get(REF_KEY)
                    .and_then(Value::as_u64)
                    .and_then(|n| usize::try_from(n).ok())
                    .filter(|&n| n < resolved.len())
                {
                    resolve(index, defs, resolved);
                    if let Some(full) = &resolved[index] {
                        *value = full.clone();
                    }
                    return;
                }
            }
            for item in map.values_mut() {
                expand(item, defs, resolved);
            }
            if let Some(records) = table_records(map) {
                *value = Value::Array(records);
            }
        }
        _ => {}
    }
}

/// What [`shrink`] did, for the measurement tooling.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ShrinkStats {
    /// Lists of records sent as tables.
    pub tables: usize,
    /// Repeated values and their uses.
    pub interned: Option<InternStats>,
}

/// Send `document` without writing anything twice.
///
/// Lists of same-shaped records become tables and repeated values move to
/// `$defs`. Returns `None` and leaves
/// the document untouched when it already uses a reserved key or neither step
/// finds anything to save.
pub fn shrink(document: &mut Value) -> Option<ShrinkStats> {
    if !document.is_object() || uses_reserved_key(document) {
        return None;
    }
    let tables = tabulate(document);
    let interned = intern_repeats(document);
    if tables == 0 && interned.is_none() {
        return None;
    }
    Some(ShrinkStats { tables, interned })
}

/// The browser transport form of a report document: unchanged when small,
/// shrunk when large. Anything that is not a JSON object (an error document)
/// passes through as it came.
#[must_use]
pub fn for_browser(json: String) -> String {
    if json.len() <= INTERN_ABOVE_BYTES {
        return json;
    }
    let Ok(mut document) = serde_json::from_str::<Value>(&json) else {
        return json;
    };
    if shrink(&mut document).is_none() {
        return json;
    }
    serde_json::to_string(&document).unwrap_or(json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn big_document(parts: usize) -> Value {
        let source = json!({"tier": "estimated-fallback", "layer": "engine-fallback",
            "uncertainty": [{"parameter": "r_fallback.model", "status": "unknown"}]});
        let rows: Vec<Value> = (0..parts)
            .map(|n| {
                json!({"reference": format!("R{n}"), "source": source,
                    "explanation": "Reference, value, pins and board location were recovered from the input.",
                    "short": "ok"})
            })
            .collect();
        json!({"ok": true, "rows": rows, "note": source})
    }

    #[test]
    fn repeats_are_shared_and_come_back_exactly() {
        let original = big_document(50);
        let mut shared = original.clone();
        let stats = intern_repeats(&mut shared).expect("repeats exist");
        assert!(stats.references >= 100, "{stats:?}");
        assert!(
            serde_json::to_string(&shared).unwrap().len()
                < serde_json::to_string(&original).unwrap().len() / 2
        );
        restore(&mut shared);
        assert_eq!(shared, original);
    }

    #[test]
    fn short_values_and_unique_values_are_left_alone() {
        let mut doc = json!({"a": ["x", "x", "x"], "b": "a unique sentence that is long enough to be a candidate"});
        assert_eq!(intern_repeats(&mut doc), None);
        assert!(doc.get(DEFS_KEY).is_none());
    }

    #[test]
    fn a_document_that_uses_the_reserved_key_is_sent_whole() {
        let mut doc = big_document(20);
        doc["rows"][0]["odd"] = json!({"$ref": 3});
        let before = doc.clone();
        assert_eq!(intern_repeats(&mut doc), None);
        assert_eq!(doc, before);
    }

    #[test]
    fn nested_repeats_resolve_through_their_parents() {
        let inner = json!({"unit": "volts and amps, with a long enough description"});
        let outer = json!({"inner": inner, "also": "a second field that makes the outer record long enough"});
        let original = json!({"rows": [outer, outer, outer, {"inner": inner}]});
        let mut shared = original.clone();
        intern_repeats(&mut shared).expect("repeats exist");
        restore(&mut shared);
        assert_eq!(shared, original);
    }

    #[test]
    fn same_shaped_records_become_a_table_and_come_back() {
        let rows: Vec<Value> = (0..40)
            .map(|n| {
                json!({"reference": format!("U{n}"), "x": n, "y": n * 2, "pins": [
                {"number": "1", "net": format!("N{n}")},
                {"number": "2", "net": "GND"},
                {"number": "3", "net": "VCC"}]})
            })
            .collect();
        let original = json!({"ok": true, "components": rows});
        let mut sent = original.clone();
        let stats = shrink(&mut sent).expect("a table is smaller");
        assert_eq!(stats.tables, 1, "{stats:?}");
        assert!(sent["components"].get(TABLE_KEY).is_some());
        assert!(
            serde_json::to_string(&sent).unwrap().len()
                < serde_json::to_string(&original).unwrap().len()
        );
        restore(&mut sent);
        assert_eq!(sent, original);
    }

    #[test]
    fn a_field_that_never_varies_is_stated_once() {
        let rows: Vec<Value> = (0..40)
            .map(|n| json!({"id": format!("U{n}"), "status": "recovered", "confidence": "high", "x": n}))
            .collect();
        let original = json!({"objects": rows});
        let mut sent = original.clone();
        assert_eq!(tabulate(&mut sent), 1);
        assert_eq!(
            sent["objects"]["const"],
            json!({"confidence": "high", "status": "recovered"})
        );
        assert_eq!(sent["objects"]["$table"], json!(["id", "x"]));
        restore(&mut sent);
        assert_eq!(sent, original);
    }

    #[test]
    fn records_that_are_all_identical_keep_one_field_in_the_rows() {
        let rows: Vec<Value> = (0..40)
            .map(|_| json!({"status": "recovered", "confidence": "high"}))
            .collect();
        let original = json!({"objects": rows});
        let mut sent = original.clone();
        assert_eq!(tabulate(&mut sent), 1);
        restore(&mut sent);
        assert_eq!(sent, original);
    }

    #[test]
    fn records_with_different_fields_stay_a_list() {
        let mut rows: Vec<Value> = (0..40)
            .map(|n| json!({"reference": format!("U{n}"), "x": n, "y": n}))
            .collect();
        rows[7] = json!({"reference": "U7", "x": 7});
        let original = json!({"components": rows});
        let mut sent = original.clone();
        assert_eq!(tabulate(&mut sent), 0);
        assert_eq!(sent, original);
    }

    #[test]
    fn a_document_that_already_uses_the_table_key_is_sent_whole() {
        let mut doc = json!({"a": {"$table": 1}, "rows": (0..40).map(|n| json!({"k": n, "v": n})).collect::<Vec<_>>()});
        let before = doc.clone();
        assert_eq!(shrink(&mut doc), None);
        assert_eq!(doc, before);
    }

    #[test]
    fn small_reports_pass_through_unchanged() {
        let small = serde_json::to_string(&big_document(3)).unwrap();
        assert!(small.len() < INTERN_ABOVE_BYTES);
        assert_eq!(for_browser(small.clone()), small);
    }

    #[test]
    fn a_large_report_shrinks_and_restores() {
        let original = big_document(4000);
        let text = serde_json::to_string(&original).unwrap();
        assert!(text.len() > INTERN_ABOVE_BYTES);
        let shared = for_browser(text.clone());
        assert!(
            shared.len() * 3 < text.len(),
            "{} vs {}",
            shared.len(),
            text.len()
        );
        let mut parsed: Value = serde_json::from_str(&shared).unwrap();
        restore(&mut parsed);
        assert_eq!(parsed, original);
    }
}
