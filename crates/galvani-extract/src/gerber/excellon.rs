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
    let mut current: Option<f64> = None;
    let mut holes = Vec::new();
    let mut plated: Option<bool> = None;
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
        // Attribute comments carry plated-ness.
        if line.starts_with(';') || line.starts_with("; ") {
            let up = line.to_ascii_uppercase();
            if up.contains("FILEFUNCTION") {
                if up.contains("NONPLATED") || up.contains("NPTH") {
                    plated = Some(false);
                } else if up.contains("PLATED") || up.contains("PTH") {
                    plated = Some(true);
                }
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
            int_digits = 2;
            dec_digits = 4;
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
            continue;
        }
        // Tool select: lone `T<idx>` (no C).
        if let Some(idx) = parse_tool_select(line) {
            current = tools.get(&idx).copied();
            continue;
        }
        // Coordinate line: X..Y.. (possibly with G81/G05 prefix or repeats).
        if let Some((x, y)) = parse_xy(line, metric, int_digits, dec_digits, leading_zero_omitted) {
            if let Some(dia) = current {
                holes.push(Hole { x, y, diameter: dia });
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
    let idx: u32 = after_t[..cpos].parse().ok()?;
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

fn parse_xy(
    line: &str,
    metric: bool,
    int_digits: usize,
    dec_digits: usize,
    leading_zero_omitted: bool,
) -> Option<(f64, f64)> {
    // Strip a leading G-code (G81 drill, G05 etc) and trailing canned-cycle.
    let xi = line.find('X')?;
    let rest = &line[xi + 1..];
    let yi = rest.find('Y')?;
    let xs = rest[..yi].trim();
    let after_y = &rest[yi + 1..];
    let yend = after_y
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+'))
        .unwrap_or(after_y.len());
    let ys = after_y[..yend].trim();

    let x = parse_coord(xs, int_digits, dec_digits, leading_zero_omitted)?;
    let y = parse_coord(ys, int_digits, dec_digits, leading_zero_omitted)?;
    let scale = if metric { 1.0 } else { 25.4 };
    Some((x * scale, y * scale))
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
}
