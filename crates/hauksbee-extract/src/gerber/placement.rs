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
    /// Marked do-not-populate by a P&P/BOM column (`DNP`/`Fitted:No`/…). The
    /// part is on the board drawing but is not assembled, so checks that reason
    /// about populated parts must skip it.
    pub dnp: bool,
}

/// One reference designator's BOM enrichment: value, part number, and whether
/// the BOM marks it do-not-populate.
#[derive(Debug, Clone, Default)]
pub struct BomEntry {
    pub value: String,
    pub mpn: String,
    pub dnp: bool,
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
    // Then prefix/contains (e.g. "centerxmm" contains "centerx"). A bare
    // single-character alias ("x"/"y") must NOT latch onto an unrelated column
    // via `contains` — "index", "maxheight", and "layer" all contain an x/y —
    // so single-char aliases match only as a prefix ("xmm"), never mid-word.
    for a in aliases {
        if let Some(i) = normed
            .iter()
            .position(|h| h.starts_with(a) || (a.len() > 1 && h.contains(a)))
        {
            return Some(i);
        }
    }
    None
}

/// A cell in an explicit do-not-populate column: truthy means "not assembled".
fn cell_is_dnp(s: &str) -> bool {
    matches!(
        s.trim().to_ascii_lowercase().as_str(),
        "dnp" | "dnf" | "yes" | "y" | "true" | "1" | "x" | "noload" | "no-load" | "donotpopulate"
    )
}

/// A cell in a "populate/fitted/assemble" column: reversed polarity — a
/// negative value means the part is NOT assembled.
fn cell_says_not_fitted(s: &str) -> bool {
    matches!(
        s.trim().to_ascii_lowercase().as_str(),
        "no" | "n" | "false" | "0" | "dnp" | "dnf" | "notfitted" | "no-load" | "noload"
    )
}

/// The unit scale implied by a coordinate column HEADER (e.g. `PosX (mil)`),
/// applied only when the cell value itself carries no unit suffix. mm→1,
/// mil→0.0254 mm, inch→25.4 mm.
///
/// Matches unit WORDS (split on non-letters), not substrings: a bare
/// `contains("mil")` fired inside "millimeter"/"millimeters" (a 39x mis-scale),
/// and matching only the spelled-out "inch" missed the "in" abbreviation the
/// cell parser [`parse_len`] accepts (a 25.4x mis-scale). The first recognised
/// unit token wins; an unlabelled header stays mm (scale 1).
fn header_unit_scale(header: &str) -> f64 {
    for tok in header
        .to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphabetic())
    {
        match tok {
            "mil" | "mils" | "thou" => return 0.0254,
            "in" | "inch" | "inches" => return 25.4,
            "mm" | "millimeter" | "millimeters" | "millimetre" | "millimetres" => return 1.0,
            _ => {}
        }
    }
    1.0
}

/// Parse a number that may carry a unit suffix; returns (value, had_unit) where
/// `had_unit` is true when the cell itself named its unit (mm/mil/inch), so a
/// header-implied unit must not be applied on top of it.
fn parse_len(s: &str) -> Option<(f64, bool)> {
    let t = s.trim();
    let lower = t.to_ascii_lowercase();
    // Accept the plural "mils" and the spelled-out "inch"/"inches" too: the doc
    // above advertises cell-named units (mm/mil/inch), but detecting inch only
    // via the "in" abbreviation ("inch" ends in "ch") and mil only via bare "mil"
    // ("mils" ends in "ils") let those spellings fall through as unitless and get
    // silently re-scaled by the header unit — a 25.4x / 39x coordinate error.
    let mil = lower.ends_with("mil") || lower.ends_with("mils");
    let inch = lower.ends_with("in")
        || lower.ends_with("inch")
        || lower.ends_with("inches")
        || lower.ends_with("\"");
    // An explicit "mm" is a unit too: the value is already in mm and must not be
    // re-scaled by a header unit. (Checked after mil so "…mil" isn't read as mm.)
    let mm = !mil && lower.ends_with("mm");
    let cleaned: String = t
        .chars()
        .filter(|c| {
            c.is_ascii_digit() || *c == '.' || *c == '-' || *c == '+' || *c == 'e' || *c == 'E'
        })
        .collect();
    let v: f64 = cleaned.parse().ok()?;
    if mil {
        Some((v * 0.0254, true))
    } else if inch {
        Some((v * 25.4, true))
    } else if mm {
        Some((v, true))
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

    let ref_col = find_col(
        &headers,
        &["ref", "reference", "designator", "refdes", "name", "part"],
    );
    let val_col = find_col(&headers, &["val", "value", "comment", "partvalue"]);
    let pkg_col = find_col(&headers, &["package", "footprint", "pattern", "pkg"]);
    let x_col = find_col(&headers, &["posx", "x", "midx", "centerx", "refx", "px"]);
    let y_col = find_col(&headers, &["posy", "y", "midy", "centery", "refy", "py"]);
    let rot_col = find_col(&headers, &["rot", "rotation", "angle"]);
    let side_col = find_col(&headers, &["side", "layer", "tblayer"]);
    let dnp_col = find_col(&headers, &["dnp", "dnf", "donotpopulate", "noload"]);
    // A "populate/fitted" column has reversed polarity; never reuse the DNP
    // column for it (its header may also `contains("populate")`).
    let fit_col = find_col(&headers, &["populate", "fitted", "assemble", "mount"])
        .filter(|c| Some(*c) != dnp_col);

    // If we can't find a reference and both coordinates, this isn't a P&P file.
    let (Some(rc), Some(xc), Some(yc)) = (ref_col, x_col, y_col) else {
        return Vec::new();
    };
    // A unit named in the coordinate header applies to bare (suffix-less) values.
    let x_scale = header_unit_scale(&headers[xc]);
    let y_scale = header_unit_scale(&headers[yc]);

    let mut out = Vec::new();
    for line in lines {
        if line.trim_start().starts_with('#') {
            continue;
        }
        let cells = split_csv(line);
        let get = |i: Option<usize>| {
            i.and_then(|i| cells.get(i))
                .map(|s| s.as_str())
                .unwrap_or("")
        };
        let reference = cells.get(rc).cloned().unwrap_or_default();
        if reference.is_empty() {
            continue;
        }
        let Some((x, x_had_unit)) = parse_len(cells.get(xc).map(|s| s.as_str()).unwrap_or("")) else {
            continue;
        };
        let Some((y, y_had_unit)) = parse_len(cells.get(yc).map(|s| s.as_str()).unwrap_or("")) else {
            continue;
        };
        let x = if x_had_unit { x } else { x * x_scale };
        let y = if y_had_unit { y } else { y * y_scale };
        let rotation = get(rot_col).trim().parse().unwrap_or(0.0);
        let side_raw = get(side_col).to_ascii_lowercase();
        let top = !(side_raw.contains("bot") || side_raw.contains("back") || side_raw == "b");
        let dnp = cell_is_dnp(get(dnp_col))
            || (fit_col.is_some() && cell_says_not_fitted(get(fit_col)));
        out.push(Placement {
            reference,
            value: get(val_col).to_string(),
            package: get(pkg_col).to_string(),
            x,
            y,
            rotation,
            top,
            dnp,
        });
    }
    out
}

/// Parse an Allegro/Cadence component-location file (`smt_loc.txt`): a
/// `!`-delimited table with a `UUNITS = MILS|MM` header and columns
/// `refdes ! symbol_x ! symbol_y ! rotation ! mirror ! symbol_name`. The
/// `mirror` flag (`m`) marks the bottom side. This is the pick-and-place the
/// uConsole mainboard (and other Allegro `.art` jobs) ships instead of a CSV.
pub fn parse_allegro_loc(text: &str) -> Vec<Placement> {
    let mut mils = false; // default to mm unless the header says MILS
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let up = line.to_ascii_uppercase();
        if up.starts_with("UUNITS") {
            mils = up.contains("MIL");
            continue;
        }
        if up.starts_with("VERSION") || up.starts_with("UNITS") && !line.contains('!') {
            continue;
        }
        if !line.contains('!') {
            continue;
        }
        let cells: Vec<&str> = line.split('!').map(str::trim).collect();
        if cells.len() < 5 {
            continue;
        }
        let reference = cells[0].to_string();
        if reference.is_empty() {
            continue;
        }
        let (Ok(mut x), Ok(mut y)) = (cells[1].parse::<f64>(), cells[2].parse::<f64>()) else {
            continue;
        };
        if mils {
            x *= 0.0254;
            y *= 0.0254;
        }
        let rotation = cells[3].parse().unwrap_or(0.0);
        let mirror = cells
            .get(4)
            .map(|s| s.eq_ignore_ascii_case("m"))
            .unwrap_or(false);
        let package = cells.get(5).map(|s| s.to_string()).unwrap_or_default();
        out.push(Placement {
            reference,
            value: String::new(),
            package,
            x,
            y,
            rotation,
            top: !mirror,
            dnp: false,
        });
    }
    out
}

/// Expand a designator token that may denote a range: `R1-R5`, `R1..R5`, and
/// the short `R1-5` form all become R1,R2,R3,R4,R5. A plain token (or anything
/// that isn't a clean same-prefix ascending range) returns itself unchanged.
fn expand_ref_range(tok: &str) -> Vec<String> {
    let sep = tok
        .find("..")
        .map(|i| (i, i + 2))
        .or_else(|| tok.find('-').map(|i| (i, i + 1)));
    let Some((a, b)) = sep else {
        return vec![tok.to_string()];
    };
    let split_pn = |s: &str| -> Option<(String, u32)> {
        let p: String = s.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
        let num = &s[p.len()..];
        if num.is_empty() || !num.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        num.parse::<u32>().ok().map(|n| (p, n))
    };
    let (Some((lp, ln)), Some((rp, rn))) = (split_pn(&tok[..a]), split_pn(&tok[b..])) else {
        return vec![tok.to_string()];
    };
    // The end prefix must be empty (short form) or match the start prefix, the
    // range must ascend, and it must be sanely bounded.
    if !(rp.is_empty() || rp == lp) || rn < ln || rn - ln > 10_000 {
        return vec![tok.to_string()];
    }
    (ln..=rn).map(|n| format!("{lp}{n}")).collect()
}

/// Parse a BOM CSV into `reference -> BomEntry` enrichment. Handles the common
/// "Designator/Comment" and "Reference(s)/Value/MPN" layouts; a single BOM row
/// often lists many refs, comma/space separated and abbreviated as ranges
/// ("R1-R5"). A DNP / "Fitted:No" column marks parts that are drawn but not
/// assembled.
pub fn parse_bom(text: &str) -> HashMap<String, BomEntry> {
    let mut out = HashMap::new();
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let Some(header_line) = lines.next() else {
        return out;
    };
    let headers = split_csv(header_line);
    let ref_col = find_col(
        &headers,
        &[
            "designator",
            "reference",
            "references",
            "refdes",
            "ref",
            "part",
        ],
    );
    let val_col = find_col(&headers, &["value", "comment", "val"]);
    let mpn_col = find_col(
        &headers,
        &[
            "mpn",
            "partnumber",
            "manufacturerpartnumber",
            "lcsc",
            "partno",
            "part",
        ],
    );
    let dnp_col = find_col(&headers, &["dnp", "dnf", "donotpopulate", "noload"]);
    let fit_col = find_col(&headers, &["fitted", "populate", "assemble", "mount"])
        .filter(|c| Some(*c) != dnp_col);
    let Some(rc) = ref_col else {
        return out;
    };
    for line in lines {
        let cells = split_csv(line);
        let Some(refs) = cells.get(rc) else { continue };
        let cell = |i: Option<usize>| i.and_then(|i| cells.get(i)).map(|s| s.as_str()).unwrap_or("");
        let value = cell(val_col).to_string();
        let mpn = cell(mpn_col).to_string();
        let dnp = cell_is_dnp(cell(dnp_col))
            || (fit_col.is_some() && cell_says_not_fitted(cell(fit_col)));
        for tok in refs
            .split([',', ' ', ';'])
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            for r in expand_ref_range(tok) {
                if r.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) {
                    out.insert(
                        r,
                        BomEntry {
                            value: value.clone(),
                            mpn: mpn.clone(),
                            dnp,
                        },
                    );
                }
            }
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
    fn allegro_loc_mils() {
        let txt = "VERSION = 2.0\n\
UUNITS = MILS\n\
#  refdes ! symbol_x ! symbol_y ! rotation ! mirror ! symbol_name\n\
C1     !  1243.63 !   461.51 !  180 ! m ! C0603 !\n\
U2     !  2000.00 !  1000.00 !   90 !   ! QFN56 !\n";
        let p = parse_allegro_loc(txt);
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].reference, "C1");
        // 1243.63 mil = 31.588 mm
        assert!((p[0].x - 1243.63 * 0.0254).abs() < 1e-6);
        assert!(!p[0].top, "mirror 'm' marks bottom side");
        assert!(p[1].top);
        assert_eq!(p[1].package, "QFN56");
    }

    #[test]
    fn bom_multi_ref() {
        let csv = "Designator,Comment,MPN\n\"R1,R2,R3\",10k,RC0402\n";
        let b = parse_bom(csv);
        assert_eq!(b.len(), 3);
        assert_eq!(b.get("R2").unwrap().value, "10k");
        assert_eq!(b.get("R2").unwrap().mpn, "RC0402");
    }

    #[test]
    fn bom_expands_designator_ranges() {
        // A BOM row that abbreviates ten decoupling caps as a range must enrich
        // each one, not a single literal "C1-C10" key (PNP-4).
        let csv = "Designator,Comment,MPN\n\"C1-C10\",100n,CL10\nR1..R3,10k,RC\n";
        let b = parse_bom(csv);
        assert_eq!(b.len(), 13, "C1..C10 (10) + R1..R3 (3)");
        assert_eq!(b.get("C7").unwrap().value, "100n");
        assert_eq!(b.get("C10").unwrap().mpn, "CL10");
        assert_eq!(b.get("R2").unwrap().value, "10k");
        assert!(!b.contains_key("C1-C10"), "the range key itself is gone");
    }

    #[test]
    fn bom_dnp_column_marks_unpopulated() {
        // A "Fitted" column with reversed polarity (No = do-not-populate) and an
        // explicit DNP column both mark the part absent (PNP-5).
        let csv = "Designator,Value,Fitted\nR1,10k,Yes\nR2,DNP,No\n";
        let b = parse_bom(csv);
        assert!(!b.get("R1").unwrap().dnp, "Fitted:Yes is populated");
        assert!(b.get("R2").unwrap().dnp, "Fitted:No is do-not-populate");
    }

    #[test]
    fn pnp_header_unit_scales_bare_values() {
        // Coordinates are in mil per the column header; suffix-less cells must be
        // scaled to mm (PNP-2). 1000 mil = 25.4 mm.
        let csv = "Ref,PosX (mil),PosY (mil),Rot\nU1,1000,2000,0\n";
        let p = parse_pnp(csv);
        assert_eq!(p.len(), 1);
        assert!((p[0].x - 25.4).abs() < 1e-6, "1000 mil = 25.4 mm, got {}", p[0].x);
        assert!((p[0].y - 50.8).abs() < 1e-6);
    }

    #[test]
    fn pnp_cell_unit_beats_header_unit() {
        // A cell that names its own unit wins over the header unit — no
        // double-scaling (PNP-2 guard).
        let csv = "Ref,PosX (mil),PosY (mil)\nU1,5.0mm,5.0mm\n";
        let p = parse_pnp(csv);
        assert!((p[0].x - 5.0).abs() < 1e-6, "explicit mm not re-scaled");
    }

    #[test]
    fn pnp_bare_xy_alias_does_not_match_unrelated_column() {
        // The X/Y columns are named just "X (mm)"/"Y (mm)" (norm "xmm"/"ymm"),
        // which no alias matches exactly, so resolution falls to the fuzzy pass.
        // A bare "x" alias must not `contains`-match the earlier "Index" column
        // (PNP-3) — it must prefix-match "xmm". Were col 0 chosen for X, U1's X
        // would parse as 1, not 12.5.
        let csv = "Index,Ref,X (mm),Y (mm)\n1,U1,12.5,7.5\n";
        let p = parse_pnp(csv);
        assert_eq!(p.len(), 1);
        assert!((p[0].x - 12.5).abs() < 1e-6, "X column chosen, not Index: got {}", p[0].x);
        assert!((p[0].y - 7.5).abs() < 1e-6);
    }

    #[test]
    fn pnp_dnp_column_flags_placement() {
        let csv = "Ref,PosX,PosY,DNP\nR1,1,1,\nR2,2,2,DNP\n";
        let p = parse_pnp(csv);
        assert!(!p[0].dnp, "blank DNP cell is populated");
        assert!(p[1].dnp, "DNP cell marks unpopulated");
    }

    #[test]
    fn header_unit_scale_matches_unit_words_not_substrings() {
        // Round-28: contains("mil") fired inside "millimeters" (a 39x shrink) and
        // the "in" abbreviation was missed (a 25.4x error). Match whole unit words.
        assert_eq!(header_unit_scale("PosX (millimeters)"), 1.0);
        assert_eq!(header_unit_scale("X (mm)"), 1.0);
        assert_eq!(header_unit_scale("X (in)"), 25.4);
        assert_eq!(header_unit_scale("PosX (inch)"), 25.4);
        assert_eq!(header_unit_scale("PosX (mil)"), 0.0254);
        assert_eq!(header_unit_scale("PosX (mils)"), 0.0254);
        // An unlabelled coordinate header stays millimetres.
        assert_eq!(header_unit_scale("PosX"), 1.0);
    }

    #[test]
    fn pnp_millimeter_labeled_header_is_not_rescaled_as_mils() {
        // End-to-end: a "(millimeters)" column with bare cell values must stay in
        // mm, not collapse ~39x. Before the fix the whole board clustered at ~0.25 mm.
        let csv = "Ref,PosX (millimeters),PosY (millimeters),Rot\nU1,10.0,20.0,0\n";
        let p = parse_pnp(csv);
        assert_eq!(p.len(), 1);
        assert!((p[0].x - 10.0).abs() < 1e-6, "x stays 10 mm, got {}", p[0].x);
        assert!((p[0].y - 20.0).abs() < 1e-6, "y stays 20 mm, got {}", p[0].y);
    }

    #[test]
    fn parse_len_recognizes_spelled_out_and_plural_units() {
        // Round-27: the doc advertises cell-named units (mm/mil/inch), but the
        // spelled-out "inch" and plural "mils" fell through as unitless and were
        // re-scaled by the header (25.4x / 39x error). They must convert like
        // their abbreviations and report had_unit=true so the header is ignored.
        let (v, had) = parse_len("0.5inch").unwrap();
        assert!((v - 12.7).abs() < 1e-9 && had, "0.5inch -> 12.7 mm, unit named");
        let (v, had) = parse_len("10mils").unwrap();
        assert!((v - 0.254).abs() < 1e-9 && had, "10mils -> 0.254 mm, unit named");
        // The abbreviations still behave.
        assert_eq!(parse_len("0.5in").unwrap(), (12.7, true));
        assert_eq!(parse_len("10mil").unwrap(), (0.254, true));
        // A bare number is still unitless (header unit applies).
        assert_eq!(parse_len("5.0").unwrap(), (5.0, false));
    }
}
