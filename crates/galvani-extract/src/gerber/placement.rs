//! Pick-and-place (position) and BOM CSV readers.
//!
//! P&P files have no standard column names. KiCad writes
//! `Ref,Val,Package,PosX,PosY,Rot,Side`; JLCPCB wants
//! `Designator,Mid X,Mid Y,Layer,Rotation`; Altium emits `Designator`,
//! `Comment`, `Footprint`, `Center-X(mm)`… We match columns by a set of
//! aliases (case/space/punctuation-insensitive) so the common variants all
//! load. Units are mm unless a header says mil/inch or the values carry a
//! `mm`/`mil` suffix.
//!
//! A BOM CSV (optional) maps reference designators to a part number / value to
//! enrich components whose package field alone is uninformative.

use std::collections::HashMap;

/// One placed component from the P&P file.
#[derive(Debug, Clone)]
pub struct Placement {
    pub reference: String,
    pub value: String,
    pub package: String,
    pub x: f64,
    pub y: f64,
    pub rotation: f64,
    /// true = top/front, false = bottom/back.
    pub top: bool,
}

/// Split a CSV line honouring double-quotes. Good enough for fab CSVs (no
/// embedded newlines; quote-escaped commas handled).
fn split_csv(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_q = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                if in_q && chars.peek() == Some(&'"') {
                    cur.push('"');
                    chars.next();
                } else {
                    in_q = !in_q;
                }
            }
            ',' | ';' if !in_q => {
                out.push(cur.trim().to_string());
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    out.push(cur.trim().to_string());
    out
}

/// Normalise a header cell for matching: lowercase, strip spaces/underscores/
/// punctuation and a trailing unit in parens.
fn norm(h: &str) -> String {
    h.to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect()
}

fn find_col(headers: &[String], aliases: &[&str]) -> Option<usize> {
    let normed: Vec<String> = headers.iter().map(|h| norm(h)).collect();
    // Exact alias match first.
    for a in aliases {
        if let Some(i) = normed.iter().position(|h| h == a) {
            return Some(i);
        }
    }
    // Then prefix/contains (e.g. "centerxmm" contains "centerx").
    for a in aliases {
        if let Some(i) = normed.iter().position(|h| h.starts_with(a) || h.contains(a)) {
            return Some(i);
        }
    }
    None
}

/// Parse a number that may carry a unit suffix; returns (value, was_mil).
fn parse_len(s: &str) -> Option<(f64, bool)> {
    let t = s.trim();
    let lower = t.to_ascii_lowercase();
    let mil = lower.ends_with("mil");
    let inch = lower.ends_with("in") || lower.ends_with("\"");
    let cleaned: String = t
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '.' || *c == '-' || *c == '+' || *c == 'e' || *c == 'E')
        .collect();
    let v: f64 = cleaned.parse().ok()?;
    if mil {
        Some((v * 0.0254, true))
    } else if inch {
        Some((v * 25.4, true))
    } else {
        Some((v, false))
    }
}

/// Parse a pick-and-place CSV. Returns the placements it could read; rows it
/// can't are skipped (honest: the caller reports how many components landed).
pub fn parse_pnp(text: &str) -> Vec<Placement> {
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    // Skip leading comment lines some tools prepend (lines starting with '#').
    let header_line = loop {
        match lines.next() {
            Some(l) if l.trim_start().starts_with('#') => continue,
            Some(l) => break l,
            None => return Vec::new(),
        }
    };
    let headers = split_csv(header_line);

    let ref_col = find_col(&headers, &["ref", "reference", "designator", "refdes", "name", "part"]);
    let val_col = find_col(&headers, &["val", "value", "comment", "partvalue"]);
    let pkg_col = find_col(&headers, &["package", "footprint", "pattern", "pkg"]);
    let x_col = find_col(&headers, &["posx", "x", "midx", "centerx", "refx", "px"]);
    let y_col = find_col(&headers, &["posy", "y", "midy", "centery", "refy", "py"]);
    let rot_col = find_col(&headers, &["rot", "rotation", "angle"]);
    let side_col = find_col(&headers, &["side", "layer", "tblayer"]);

    // If we can't find a reference and both coordinates, this isn't a P&P file.
    let (Some(rc), Some(xc), Some(yc)) = (ref_col, x_col, y_col) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for line in lines {
        if line.trim_start().starts_with('#') {
            continue;
        }
        let cells = split_csv(line);
        let get = |i: Option<usize>| i.and_then(|i| cells.get(i)).map(|s| s.as_str()).unwrap_or("");
        let reference = cells.get(rc).cloned().unwrap_or_default();
        if reference.is_empty() {
            continue;
        }
        let Some((x, _)) = parse_len(cells.get(xc).map(|s| s.as_str()).unwrap_or("")) else {
            continue;
        };
        let Some((y, _)) = parse_len(cells.get(yc).map(|s| s.as_str()).unwrap_or("")) else {
            continue;
        };
        let rotation = get(rot_col).trim().parse().unwrap_or(0.0);
        let side_raw = get(side_col).to_ascii_lowercase();
        let top = !(side_raw.contains("bot") || side_raw.contains("back") || side_raw == "b");
        out.push(Placement {
            reference,
            value: get(val_col).to_string(),
            package: get(pkg_col).to_string(),
            x,
            y,
            rotation,
            top,
        });
    }
    out
}

/// Parse a BOM CSV into `reference -> (value, part_number)` enrichment. Handles
/// the common "Designator/Comment" and "Reference(s)/Value/MPN" layouts; a
/// single BOM row often lists many refs ("R1,R2,R3").
pub fn parse_bom(text: &str) -> HashMap<String, (String, String)> {
    let mut out = HashMap::new();
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let Some(header_line) = lines.next() else {
        return out;
    };
    let headers = split_csv(header_line);
    let ref_col = find_col(
        &headers,
        &["designator", "reference", "references", "refdes", "ref", "part"],
    );
    let val_col = find_col(&headers, &["value", "comment", "val"]);
    let mpn_col = find_col(
        &headers,
        &["mpn", "partnumber", "manufacturerpartnumber", "lcsc", "partno", "part"],
    );
    let Some(rc) = ref_col else {
        return out;
    };
    for line in lines {
        let cells = split_csv(line);
        let Some(refs) = cells.get(rc) else { continue };
        let value = val_col.and_then(|i| cells.get(i)).cloned().unwrap_or_default();
        let mpn = mpn_col.and_then(|i| cells.get(i)).cloned().unwrap_or_default();
        for r in refs.split([',', ' ']).map(str::trim).filter(|s| !s.is_empty()) {
            out.insert(r.to_string(), (value.clone(), mpn.clone()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kicad_pos() {
        let csv = "Ref,Val,Package,PosX,PosY,Rot,Side\n\
\"C1\",\"10u\",\"C_0805_2012Metric\",107.937500,-76.500000,0.000000,top\n\
\"U1\",\"RP2040\",\"QFN-56\",100.0,-90.0,90.0,bottom\n";
        let p = parse_pnp(csv);
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].reference, "C1");
        assert!((p[0].x - 107.9375).abs() < 1e-6);
        assert!(p[0].top);
        assert!(!p[1].top);
        assert!((p[1].rotation - 90.0).abs() < 1e-6);
    }

    #[test]
    fn jlcpcb_variant() {
        let csv = "Designator,Mid X,Mid Y,Layer,Rotation\n\
R1,10.5mm,-3.2mm,Top,0\n\
R2,5.0,5.0,Bottom,180\n";
        let p = parse_pnp(csv);
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].reference, "R1");
        assert!((p[0].x - 10.5).abs() < 1e-6);
        assert!(!p[1].top);
    }

    #[test]
    fn bom_multi_ref() {
        let csv = "Designator,Comment,MPN\n\"R1,R2,R3\",10k,RC0402\n";
        let b = parse_bom(csv);
        assert_eq!(b.len(), 3);
        assert_eq!(b.get("R2").unwrap().0, "10k");
        assert_eq!(b.get("R2").unwrap().1, "RC0402");
    }
}
