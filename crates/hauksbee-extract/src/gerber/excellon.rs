//! Excellon drill-file reader.
//!
//! No mature Rust Excellon crate exists, and the format is far simpler than
//! RS-274X, so we parse it directly. A drill file is a tool table (`T1C0.350`
//! = tool 1, 0.35 mm) in a header, then coordinate lines (`X86.19Y-79.4`) under
//! a selected tool. We read:
//!   - units (`METRIC`/`INCH`, or `INCH,LZ` style),
//!   - coordinate format (decimal point present -> as-is; otherwise the
//!     leading/trailing-zero suppression from `FORMAT=` / `INCH,LZ`),
//!   - per-tool diameter,
//!   - each hole's location and tool size.
//!
//! Plated-ness is taken from the file's `TF.FileFunction` attribute when KiCad
//! wrote it (`Plated,...,PTH` vs `NonPlated,...,NPTH`), else inferred from the
//! filename by the caller. Plated through-holes stitch copper layers and form
//! pads; non-plated holes are mechanical and ignored for connectivity.

/// One drilled hole.
#[derive(Debug, Clone)]
pub struct Hole {
    pub x: f64,
    pub y: f64,
    /// Drill diameter in mm.
    pub diameter: f64,
}

/// Parsed drill file.
#[derive(Debug, Clone, Default)]
pub struct DrillFile {
    pub holes: Vec<Hole>,
    /// True when the file declares itself plated (PTH), or None if unknown.
    pub plated: Option<bool>,
}

pub fn parse(text: &str) -> DrillFile {
    let mut metric = true; // KiCad default is metric; most modern files are
    let mut explicit_units = false;
    let mut tools: std::collections::HashMap<u32, f64> = std::collections::HashMap::new();
    // Tools declared under a `;TYPE=NON_PLATED` (Altium) header are mechanical;
    // their holes do not stitch copper, so we record which tools are NPTH and
    // skip their coordinates for connectivity.
    let mut npth_tools: std::collections::HashSet<u32> = std::collections::HashSet::new();
    let mut in_npth_section = false;
    let mut current: Option<f64> = None;
    let mut current_is_npth = false;
    let mut holes = Vec::new();
    let mut plated: Option<bool> = None;
    // Modal coordinates: an Excellon body line may carry only X (keep the last
    // Y) or only Y (keep the last X), as Altium's exporter does. Track the last
    // value seen so a single-axis line resolves to a full (x, y).
    let mut last_x: Option<f64> = None;
    let mut last_y: Option<f64> = None;
    // Zero-suppression / format: KiCad emits decimal points, so we default to
    // "coordinates already have an explicit decimal". Integer-only formats are
    // handled by detecting the absence of a '.' and applying the tool format.
    let mut int_digits = 2usize; // INCH default 2.4; METRIC default 3.3
    let mut dec_digits = 4usize;
    let mut leading_zero_omitted = true; // LZ means leading kept; default omit

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        // Attribute comments carry plated-ness and, in the Altium dialect, the
        // `;FILE_FORMAT=2:5` integer/decimal split and `;TYPE=PLATED` /
        // `;TYPE=NON_PLATED` section markers.
        if line.starts_with(';') {
            let up = line.to_ascii_uppercase();
            if up.contains("FILEFUNCTION") {
                if up.contains("NONPLATED") || up.contains("NPTH") {
                    plated = Some(false);
                } else if up.contains("PLATED") || up.contains("PTH") {
                    plated = Some(true);
                }
            }
            if let Some(eq) = up.find("FILE_FORMAT=").map(|i| i + "FILE_FORMAT=".len()) {
                // `;FILE_FORMAT=2:5` -> 2 integer, 5 decimal digits.
                let spec: String = up[eq..]
                    .chars()
                    .take_while(|c| c.is_ascii_digit() || *c == ':')
                    .collect();
                if let Some((i, d)) = spec.split_once(':') {
                    if let (Ok(i), Ok(d)) = (i.trim().parse(), d.trim().parse()) {
                        int_digits = i;
                        dec_digits = d;
                    }
                }
            }
            // Altium sections: tools listed after `;TYPE=NON_PLATED` are
            // mechanical. A whole file marked only NON_PLATED is NPTH.
            if up.contains("TYPE=NON_PLATED") || up.contains("TYPE=NONPLATED") {
                in_npth_section = true;
                if plated.is_none() {
                    plated = Some(false);
                }
            } else if up.contains("TYPE=PLATED") {
                in_npth_section = false;
                plated = Some(true);
            }
            continue;
        }
        if line == "METRIC" || line.starts_with("METRIC") || line == "M71" {
            metric = true;
            explicit_units = true;
            int_digits = 3;
            dec_digits = 3;
            apply_zero_mode(line, &mut leading_zero_omitted);
            continue;
        }
        if line == "INCH" || line.starts_with("INCH") || line == "M72" {
            metric = false;
            explicit_units = true;
            // Default INCH is 2.4, but a prior `;FILE_FORMAT=` may have set the
            // real split (e.g. 2:5); only impose the default if none was given.
            if int_digits == 2 && dec_digits == 4 {
                int_digits = 2;
                dec_digits = 4;
            }
            apply_zero_mode(line, &mut leading_zero_omitted);
            continue;
        }
        if line.starts_with("FMAT") || line == "M48" || line == "M95" || line == "%" {
            continue;
        }
        if let Some(rest) = line.strip_prefix("FORMAT=") {
            // e.g. FORMAT={3:3/ absolute / metric / decimal}
            let up = rest.to_ascii_uppercase();
            if up.contains("METRIC") {
                metric = true;
                explicit_units = true;
            } else if up.contains("INCH") {
                metric = false;
                explicit_units = true;
            }
            continue;
        }
        // Tool definition: T<idx>C<dia>  (may have F/S feed-speed suffixes).
        if let Some(t) = parse_tool_def(line) {
            tools.insert(t.0, if metric { t.1 } else { t.1 * 25.4 });
            if in_npth_section {
                npth_tools.insert(t.0);
            }
            continue;
        }
        // Tool select: lone `T<idx>` (no C).
        if let Some(idx) = parse_tool_select(line) {
            current = tools.get(&idx).copied();
            current_is_npth = npth_tools.contains(&idx);
            continue;
        }
        // Coordinate line: X..Y.. , or modal X-only / Y-only (keep last axis).
        if let Some((x, y)) =
            parse_xy_modal(line, metric, int_digits, dec_digits, leading_zero_omitted, &mut last_x, &mut last_y)
        {
            // Skip mechanical (NPTH) holes: they carry no copper to stitch.
            if let Some(dia) = current {
                if !current_is_npth {
                    holes.push(Hole { x, y, diameter: dia });
                }
            }
            continue;
        }
        // G90/G91/G05/M30/etc: ignore.
        let _ = explicit_units;
    }

    DrillFile { holes, plated }
}

fn apply_zero_mode(line: &str, leading_zero_omitted: &mut bool) {
    let up = line.to_ascii_uppercase();
    if up.contains("LZ") {
        // Leading Zeros kept (trailing suppressed).
        *leading_zero_omitted = false;
    } else if up.contains("TZ") {
        *leading_zero_omitted = true;
    }
}

fn parse_tool_def(line: &str) -> Option<(u32, f64)> {
    if !line.starts_with('T') {
        return None;
    }
    let after_t = &line[1..];
    // Need a 'C' for it to be a definition.
    let cpos = after_t.find('C')?;
    // The index is the leading digit run; feed/speed fields (`F00S00`) may sit
    // between the index and the `C` (Altium writes `T1F00S00C0.00787`), so take
    // only the leading digits rather than everything up to `C`.
    let idx_str: String = after_t.chars().take_while(|c| c.is_ascii_digit()).collect();
    let idx: u32 = idx_str.parse().ok()?;
    let after_c = &after_t[cpos + 1..];
    // Diameter runs until a non-number (F/S feed/speed) appears.
    let end = after_c
        .find(|ch: char| !(ch.is_ascii_digit() || ch == '.' || ch == '-' || ch == '+'))
        .unwrap_or(after_c.len());
    let dia: f64 = after_c[..end].parse().ok()?;
    Some((idx, dia))
}

fn parse_tool_select(line: &str) -> Option<u32> {
    if !line.starts_with('T') {
        return None;
    }
    let body: String = line[1..].chars().take_while(|c| c.is_ascii_digit()).collect();
    // Reject if there is a C (that's a definition) or extra junk.
    if line.contains('C') {
        return None;
    }
    body.parse().ok()
}

/// Parse a body coordinate line that may carry both axes (`X..Y..`), only X
/// (`X..`, keep last Y), or only Y (`Y..`, keep last X). Modal single-axis
/// lines are how Altium's Excellon exporter compresses a column/row of holes,
/// so without this almost every hole on such a file is dropped. Returns the
/// resolved absolute (x, y) and updates the modal state.
#[allow(clippy::too_many_arguments)]
fn parse_xy_modal(
    line: &str,
    metric: bool,
    int_digits: usize,
    dec_digits: usize,
    leading_zero_omitted: bool,
    last_x: &mut Option<f64>,
    last_y: &mut Option<f64>,
) -> Option<(f64, f64)> {
    let xi = line.find('X');
    let yi = line.find('Y');
    if xi.is_none() && yi.is_none() {
        return None;
    }
    let scale = if metric { 1.0 } else { 25.4 };
    // Slice the token after X up to the next non-number / 'Y'.
    let axis_tok = |pos: usize| -> Option<f64> {
        let rest = &line[pos + 1..];
        let end = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+'))
            .unwrap_or(rest.len());
        parse_coord(rest[..end].trim(), int_digits, dec_digits, leading_zero_omitted)
            .map(|v| v * scale)
    };
    if let Some(p) = xi {
        if let Some(v) = axis_tok(p) {
            *last_x = Some(v);
        } else {
            return None;
        }
    }
    if let Some(p) = yi {
        if let Some(v) = axis_tok(p) {
            *last_y = Some(v);
        } else {
            return None;
        }
    }
    match (*last_x, *last_y) {
        (Some(x), Some(y)) => Some((x, y)),
        _ => None,
    }
}

/// Parse one Excellon coordinate token into the document unit (mm or inch).
fn parse_coord(
    tok: &str,
    int_digits: usize,
    dec_digits: usize,
    leading_zero_omitted: bool,
) -> Option<f64> {
    if tok.is_empty() {
        return None;
    }
    if tok.contains('.') {
        return tok.parse().ok();
    }
    // Implicit decimal: total width = int_digits + dec_digits.
    let neg = tok.starts_with('-');
    let digits: String = tok.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let total = int_digits + dec_digits;
    let padded = if leading_zero_omitted {
        // Value right-justified: pad leading zeros to `total`.
        format!("{:0>width$}", digits, width = total)
    } else {
        // Trailing zeros suppressed: pad on the right.
        format!("{:0<width$}", digits, width = total)
    };
    let cut = padded.len().saturating_sub(dec_digits);
    let int_part = &padded[..cut];
    let dec_part = &padded[cut..];
    let val: f64 = format!("{}.{}", int_part, dec_part).parse().ok()?;
    Some(if neg { -val } else { val })
}

#[cfg(test)]
mod tests {
    use super::*;

    const KICAD_PTH: &str = "\
M48
; #@! TF.FileFunction,Plated,1,2,PTH
FMAT,2
METRIC
T1C0.350
T2C0.650
%
G90
G05
T1
X86.19Y-79.4
X90.8Y-89.2
T2
X92.0Y-97.0
M30
";

    #[test]
    fn kicad_metric_decimal() {
        let d = parse(KICAD_PTH);
        assert_eq!(d.plated, Some(true));
        assert_eq!(d.holes.len(), 3);
        assert!((d.holes[0].x - 86.19).abs() < 1e-6);
        assert!((d.holes[0].y - (-79.4)).abs() < 1e-6);
        assert!((d.holes[0].diameter - 0.35).abs() < 1e-6);
        assert!((d.holes[2].diameter - 0.65).abs() < 1e-6);
    }

    #[test]
    fn implicit_decimal_metric() {
        // 3.3 metric, leading zeros omitted: X123456 -> 123.456
        let v = parse_coord("123456", 3, 3, true).unwrap();
        assert!((v - 123.456).abs() < 1e-6);
    }

    // The Altium dialect the Inkplate 6 ships: `;FILE_FORMAT=2:5`, `INCH,LZ`,
    // `T1F00S00C...` tool defs (feed/speed before the C), modal single-axis
    // coordinate lines, and `;TYPE=PLATED` / `;TYPE=NON_PLATED` sections.
    const ALTIUM_INKPLATE: &str = "\
M48
;FILE_FORMAT=2:5
INCH,LZ
;TYPE=PLATED
T1F00S00C0.00787
T2F00S00C0.03937
;TYPE=NON_PLATED
T15F00S00C0.12598
%
T01
X0139665Y0208957
X0143996
Y0213287
T02
X0268504Y0012205
T15
X0500000Y0500000
M30
";

    #[test]
    fn altium_modal_inch_2_5_with_npth_section() {
        let d = parse(ALTIUM_INKPLATE);
        // T1/T2 are plated; T15 is NPTH and must be dropped. 4 plated holes:
        // (X..Y..), (X.. keep Y), (Y.. keep X) under T1, and one under T2.
        assert_eq!(d.holes.len(), 4, "NPTH hole under T15 must be skipped");
        // 2:5 INCH,LZ: X0139665 -> 01.39665 inch -> *25.4 mm.
        let h0 = &d.holes[0];
        assert!((h0.x - 1.39665 * 25.4).abs() < 1e-3, "x was {}", h0.x);
        assert!((h0.y - 2.08957 * 25.4).abs() < 1e-3, "y was {}", h0.y);
        // T1 diameter 0.00787 inch -> ~0.2 mm.
        assert!((h0.diameter - 0.00787 * 25.4).abs() < 1e-3);
        // Modal X-only line keeps the previous Y.
        let h1 = &d.holes[1];
        assert!((h1.y - 2.08957 * 25.4).abs() < 1e-3, "modal Y not held");
        assert!((h1.x - 1.43996 * 25.4).abs() < 1e-3);
        // Modal Y-only line keeps the previous X (1.43996 from h1).
        let h2 = &d.holes[2];
        assert!((h2.x - 1.43996 * 25.4).abs() < 1e-3, "modal X not held");
        assert!((h2.y - 2.13287 * 25.4).abs() < 1e-3);
    }

    #[test]
    fn tool_def_with_feed_speed_before_c() {
        // `T1F00S00C0.00787`: the index is the leading digits, not everything
        // up to the C.
        let (idx, dia) = parse_tool_def("T1F00S00C0.00787").unwrap();
        assert_eq!(idx, 1);
        assert!((dia - 0.00787).abs() < 1e-6);
    }
}
