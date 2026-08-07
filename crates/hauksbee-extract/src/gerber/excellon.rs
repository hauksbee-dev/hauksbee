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
//!
//! Long-form how-and-why: docs/how-and-why/hauksbee-extract/gerber.md.

/// One drilled hit: a round hole, or a slot when `to` is set.
#[derive(Debug, Clone)]
pub struct Hole {
    pub x: f64,
    pub y: f64,
    /// Drill diameter in mm. For a slot this is the routed *width*: the cutter
    /// diameter swept along the path, so the finished slot is a stadium of this
    /// width, not a rectangle of this height.
    pub diameter: f64,
    /// Far end of a slot, in the same units as `x`/`y`. `None` is a round hole.
    /// A G85 canned slot carries one segment; a routed slot (`M15` plunge, a
    /// run of `G01` cuts, `M16` retract) contributes one `Hole` per cut segment,
    /// so a multi-segment rout is a chain of overlapping stadiums whose union is
    /// the real cut.
    pub to: Option<(f64, f64)>,
}

/// The copper layer pair an Excellon file declares its hits span, as the 1-based
/// physical layer numbers the X2 `TF.FileFunction` attribute uses (`1` is the
/// top copper). `Plated,1,4,PTH` on a four-layer board is a through-hole;
/// `Plated,1,2,PTH` is a blind via that reaches only the first two layers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayerPair {
    pub from: u32,
    pub to: u32,
}

/// What a drill file's `TF.FileFunction` attribute said about the copper layers
/// its hits reach.
///
/// The three states are genuinely different to the caller. `Absent` leaves the
/// file name as the next place to look, and a plain single-drill job can safely
/// read it as a through-hole. `Unreadable` means the file DID declare a span
/// and we could not use it, which is never a licence to assume the whole stack:
/// the declaration exists precisely because the span is not obvious.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeclaredSpan {
    /// The file carries no `TF.FileFunction` layer pair.
    #[default]
    Absent,
    /// A layer pair we read.
    Pair(LayerPair),
    /// A `TF.FileFunction` was present but its layer pair is malformed,
    /// reversed, or degenerate.
    Unreadable,
}

/// Parsed drill file.
#[derive(Debug, Clone, Default)]
pub struct DrillFile {
    pub holes: Vec<Hole>,
    /// True when the file declares itself plated (PTH), or None if unknown.
    pub plated: Option<bool>,
    /// What the file's X2 attribute said about the copper layers its hits span.
    pub span: DeclaredSpan,
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
    let mut span = DeclaredSpan::Absent;
    // Routed-slot state. `M15` plunges the cutter, `G01` moves cut copper, `M16`
    // (or `M17`) retracts it. We only enter this mode on a file that actually
    // carries an `M15`: on every other file a `G00`/`G01` line keeps its old
    // meaning (a positioned hit), so no existing job changes behaviour.
    let rout_capable = text
        .lines()
        .any(|l| matches!(l.trim(), "M15" | "M15;" | "M015"));
    let mut tool_down = false;
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
    // True once an explicit `;FILE_FORMAT=` set the coordinate split, so a
    // later INCH/METRIC line does not clobber it with its unit default.
    let mut format_set = false;
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
                if span == DeclaredSpan::Absent {
                    span = parse_file_function_span(&up);
                }
            }
            if let Some(eq) = up.find("FILE_FORMAT=").map(|i| i + "FILE_FORMAT=".len()) {
                // `;FILE_FORMAT=2:5` -> 2 integer, 5 decimal digits.
                let spec: String = up[eq..]
                    .chars()
                    .take_while(|c| c.is_ascii_digit() || *c == ':')
                    .collect();
                if let Some((i, d)) = spec.split_once(':') {
                    if let (Ok(i), Ok(d)) = (i.trim().parse::<usize>(), d.trim().parse::<usize>()) {
                        // Guard a corrupt/hostile FILE_FORMAT (e.g. `99999:1`):
                        // these widths feed `format!("{:0>width$}")` and the
                        // slicing below, so an absurd value panics
                        // ("Formatting argument out of range") or allocates
                        // wildly. Real Excellon coordinate formats are tiny
                        // (≤ ~6 digits per side); ignore anything larger and
                        // keep the sane defaults.
                        if i <= 12 && d <= 12 {
                            int_digits = i;
                            dec_digits = d;
                            format_set = true;
                        }
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
            // METRIC default is 3.3, but a prior `;FILE_FORMAT=` may have set
            // the real split (e.g. 4:4); only impose the default if none was
            // given, symmetric with the INCH handler below.
            if !format_set {
                int_digits = 3;
                dec_digits = 3;
            }
            apply_zero_mode(line, &mut leading_zero_omitted);
            continue;
        }
        if line == "INCH" || line.starts_with("INCH") || line == "M72" {
            metric = false;
            explicit_units = true;
            // Default INCH is 2.4, but a prior `;FILE_FORMAT=` may have set the
            // real split (e.g. 2:5); only impose the default if none was given.
            if !format_set {
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
        // ── Routing (slots cut by a moving cutter) ──────────────────────────
        if rout_capable {
            let up = line.to_ascii_uppercase();
            if up.starts_with("M15") || up.starts_with("M015") {
                tool_down = true;
                continue;
            }
            if up.starts_with("M16") || up.starts_with("M17") || up.starts_with("M016") {
                tool_down = false;
                continue;
            }
            // The G-code is the leading `G` plus its digit run, read as a
            // number. Matching on text prefixes instead let `G5` swallow `G50`
            // and made `G2X..` an arc while `G2Y..` was not, so which motions
            // counted depended on which axis the exporter happened to write
            // first.
            let g = leading_g_code(&up);
            if g == Some(5) {
                // Back to drill mode: the cutter is up by definition.
                tool_down = false;
                continue;
            }
            // A `G00` rapid positions the cutter without cutting, so it is a
            // move and never a hit. A `G01` while the cutter is down cuts a
            // slot from where it was to where it lands.
            let is_rapid = g == Some(0);
            let is_cut = g == Some(1);
            if is_rapid || is_cut {
                let (px, py) = (last_x, last_y);
                let moved = parse_xy_modal(
                    line,
                    metric,
                    int_digits,
                    dec_digits,
                    leading_zero_omitted,
                    &mut last_x,
                    &mut last_y,
                );
                if let (Some((nx, ny)), Some(dia), true, Some(sx), Some(sy)) = (
                    moved,
                    current,
                    is_cut && tool_down && !current_is_npth,
                    px,
                    py,
                ) {
                    holes.push(Hole {
                        x: sx,
                        y: sy,
                        diameter: dia,
                        to: Some((nx, ny)),
                    });
                }
                continue;
            }
            // A `G02`/`G03` arc cut. Its wall is a curve, so the chord from
            // start to end would miss every piece of copper the arc bulges
            // towards; we tessellate it into short stadiums, the same treatment
            // the RS-274X plotter gives a copper arc. Without the I/J centre
            // offsets there is no arc to build, and the honest answer is no
            // geometry at all: planting a round hole at the endpoint (what the
            // fall-through to the plain coordinate reader would do) invents a
            // hit the file never described.
            let is_arc = g == Some(2) || g == Some(3);
            if is_arc {
                let clockwise = g == Some(2);
                let (px, py) = (last_x, last_y);
                let moved = parse_xy_modal(
                    line,
                    metric,
                    int_digits,
                    dec_digits,
                    leading_zero_omitted,
                    &mut last_x,
                    &mut last_y,
                );
                let scale = if metric { 1.0 } else { 25.4 };
                let ij = arc_center_offset(line, int_digits, dec_digits, leading_zero_omitted)
                    .map(|(i, j)| (i * scale, j * scale));
                if let (Some((nx, ny)), Some(dia), true, Some(sx), Some(sy), Some((i, j))) =
                    (moved, current, tool_down && !current_is_npth, px, py, ij)
                {
                    for (ax, ay, bx, by) in tessellate_arc(sx, sy, nx, ny, i, j, clockwise) {
                        holes.push(Hole {
                            x: ax,
                            y: ay,
                            diameter: dia,
                            to: Some((bx, by)),
                        });
                    }
                }
                continue;
            }
        }

        // ── G85 canned slot: `X<a>Y<b>G85X<c>Y<d>` cuts from (a,b) to (c,d) ──
        // The two coordinate pairs live on one line, so the plain modal parser
        // (which keys on the FIRST `X` and `Y`) sees only the start point and
        // would record a round hole where the file describes a slot. Split on
        // the G85 and parse both halves through the SAME modal reader, so the
        // units and zero-suppression that apply to the start apply to the end.
        if let Some((head, tail)) = split_g85(line) {
            let start = parse_xy_modal(
                head,
                metric,
                int_digits,
                dec_digits,
                leading_zero_omitted,
                &mut last_x,
                &mut last_y,
            );
            let end = parse_xy_modal(
                tail,
                metric,
                int_digits,
                dec_digits,
                leading_zero_omitted,
                &mut last_x,
                &mut last_y,
            );
            if let (Some((sx, sy)), Some((ex, ey)), Some(dia)) = (start, end, current) {
                if !current_is_npth {
                    holes.push(Hole {
                        x: sx,
                        y: sy,
                        diameter: dia,
                        to: Some((ex, ey)),
                    });
                }
            }
            continue;
        }

        // Coordinate line: X..Y.. , or modal X-only / Y-only (keep last axis).
        if let Some((x, y)) = parse_xy_modal(
            line,
            metric,
            int_digits,
            dec_digits,
            leading_zero_omitted,
            &mut last_x,
            &mut last_y,
        ) {
            // Skip mechanical (NPTH) holes: they carry no copper to stitch.
            if let Some(dia) = current {
                if !current_is_npth {
                    holes.push(Hole {
                        x,
                        y,
                        diameter: dia,
                        to: None,
                    });
                }
            }
            continue;
        }
        // G90/G91/G05/M30/etc: ignore.
        let _ = explicit_units;
    }

    DrillFile {
        holes,
        plated,
        span,
    }
}

/// The numeric G-code a line opens with, if any: `G0`, `G00` and `G000` all
/// read as 0, and `G50` reads as 50 rather than as a `G5` with junk after it.
/// The line must be already uppercased.
fn leading_g_code(up: &str) -> Option<u32> {
    let rest = up.strip_prefix('G')?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

/// Read the `I`/`J` centre offsets of a rout arc, in document units. Both must
/// be present: an arc missing either has no centre and so no recoverable curve.
fn arc_center_offset(
    line: &str,
    int_digits: usize,
    dec_digits: usize,
    leading_zero_omitted: bool,
) -> Option<(f64, f64)> {
    let axis = |mark: char| -> Option<f64> {
        let pos = line.find(mark)?;
        let rest = &line[pos + 1..];
        let end = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+'))
            .unwrap_or(rest.len());
        parse_coord(
            rest[..end].trim(),
            int_digits,
            dec_digits,
            leading_zero_omitted,
        )
    };
    Some((axis('I')?, axis('J')?))
}

/// Longest arc step (radians) before we split. Matches the RS-274X plotter's
/// 16-segments-per-full-circle resolution, so a routed arc wall and a drawn
/// copper arc are approximated to the same fidelity and a tangent contact
/// resolves the same way on both.
const ARC_STEP_RAD: f64 = std::f64::consts::TAU / 16.0;

/// Break a rout arc into straight cut segments, each returned as
/// `(start_x, start_y, end_x, end_y)`.
///
/// `(i, j)` is the centre offset from the start point, the multi-quadrant
/// convention Excellon shares with RS-274X. Each chord sits inside its arc, so
/// its stadium falls short of the true wall on the outside and reaches past it
/// on the inside; both errors are bounded by the sagitta of one step, which at
/// this resolution is under 2% of the arc radius. That is the same
/// approximation, and the same error bound, that [`super::rs274x`] gives a drawn
/// copper arc, so a slot wall and a copper arc resolve a marginal contact
/// identically instead of disagreeing about the same curve. A zero radius or a
/// non-finite sweep yields nothing at all rather than a guess.
fn tessellate_arc(
    sx: f64,
    sy: f64,
    ex: f64,
    ey: f64,
    i: f64,
    j: f64,
    clockwise: bool,
) -> Vec<(f64, f64, f64, f64)> {
    let (cx, cy) = (sx + i, sy + j);
    let r = (sx - cx).hypot(sy - cy);
    if !r.is_finite() || r <= 0.0 {
        return Vec::new();
    }
    let a0 = (sy - cy).atan2(sx - cx);
    let a1 = (ey - cy).atan2(ex - cx);
    let mut sweep = a1 - a0;
    if clockwise {
        while sweep > 0.0 {
            sweep -= std::f64::consts::TAU;
        }
        // A start that coincides with the end is a full circle, not a no-op.
        if sweep > -1e-12 {
            sweep = -std::f64::consts::TAU;
        }
    } else {
        while sweep < 0.0 {
            sweep += std::f64::consts::TAU;
        }
        if sweep < 1e-12 {
            sweep = std::f64::consts::TAU;
        }
    }
    if !sweep.is_finite() {
        return Vec::new();
    }
    let steps = ((sweep.abs() / ARC_STEP_RAD).ceil() as usize).max(1);
    let mut out = Vec::with_capacity(steps);
    let mut prev = (sx, sy);
    for k in 1..=steps {
        let a = a0 + sweep * (k as f64 / steps as f64);
        let p = (cx + r * a.cos(), cy + r * a.sin());
        out.push((prev.0, prev.1, p.0, p.1));
        prev = p;
    }
    out
}

/// Split a `G85` canned-slot line into its start and end coordinate halves.
/// Returns `None` for any line that is not a two-point G85 record, including a
/// bare `G85` mode line with no coordinates on either side.
fn split_g85(line: &str) -> Option<(&str, &str)> {
    let at = line.find("G85").or_else(|| line.find("g85"))?;
    let head = &line[..at];
    let tail = &line[at + 3..];
    let has_axis =
        |s: &str| s.contains('X') || s.contains('Y') || s.contains('x') || s.contains('y');
    (has_axis(head) && has_axis(tail)).then_some((head, tail))
}

/// Pull the copper layer pair out of an already-uppercased `TF.FileFunction`
/// attribute line, e.g. `; #@! TF.FILEFUNCTION,PLATED,1,2,PTH` -> `1..2`.
///
/// The pair is accepted only when both fields parse as layer numbers naming
/// distinct layers in increasing order. Anything else the attribute offered as a
/// pair comes back [`DeclaredSpan::Unreadable`] rather than [`DeclaredSpan::Absent`],
/// because a file that tried to state a span and failed is the LAST file whose
/// hits should be assumed to reach the whole stack.
fn parse_file_function_span(up: &str) -> DeclaredSpan {
    let Some(at) = up.find("FILEFUNCTION").map(|i| i + "FILEFUNCTION".len()) else {
        return DeclaredSpan::Absent;
    };
    let rest = up[at..].trim_start_matches([',', ' ']);
    let fields: Vec<&str> = rest.split(',').map(|f| f.trim()).collect();
    // Field 0 is Plated/NonPlated; fields 1 and 2 are the layer pair. A
    // two-field attribute (`Plated,PTH`) never offered a pair at all.
    let (Some(a), Some(b)) = (fields.get(1), fields.get(2)) else {
        return DeclaredSpan::Absent;
    };
    match (a.parse::<u32>(), b.parse::<u32>()) {
        (Ok(from), Ok(to)) if from >= 1 && to > from => DeclaredSpan::Pair(LayerPair { from, to }),
        // Two numbers that do not make a span (reversed, equal, zero-based) are
        // a declaration we cannot use.
        (Ok(_), Ok(_)) => DeclaredSpan::Unreadable,
        // Not a numeric pair at all: this attribute form carries no span field,
        // so the file simply did not declare one.
        _ => DeclaredSpan::Absent,
    }
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
    let body: String = line[1..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
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
        parse_coord(
            rest[..end].trim(),
            int_digits,
            dec_digits,
            leading_zero_omitted,
        )
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
    fn absurd_file_format_width_does_not_panic() {
        // A corrupt/hostile FILE_FORMAT must not drive the implicit-decimal
        // format! width past its limit; unclamped, `;FILE_FORMAT=99999:1`
        // panics with "Formatting argument out of range". Integer coordinates
        // (no decimal point) exercise the width path.
        let drill = "\
M48
FMAT,2
INCH
;FILE_FORMAT=99999:1
T1C0.035
%
G90
T1
X0100Y0200
M30
";
        let d = parse(drill); // must return, not panic
        assert_eq!(d.holes.len(), 1);
    }

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

    #[test]
    fn metric_honors_preceding_file_format() {
        // A `;FILE_FORMAT=4:4` before METRIC must win: METRIC must not clobber
        // it back to the 3:3 default (the INCH handler already guarded this).
        // X12345678 under 4:4 -> 1234.5678 mm; under a forced 3:3 it would be
        // 12345.678.
        let drill = "\
M48
;FILE_FORMAT=4:4
METRIC
T1C0.350
%
G90
T1
X12345678Y00010000
M30
";
        let d = parse(drill);
        assert_eq!(d.holes.len(), 1);
        assert!(
            (d.holes[0].x - 1234.5678).abs() < 1e-4,
            "x was {} (expected 4:4 scaling, not the 3:3 default)",
            d.holes[0].x
        );
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
    fn kicad_g85_slot_carries_both_endpoints() {
        // KiCad's canned slot: start coords, `G85`, end coords, on one line.
        // Keying on the first X and Y (the plain modal reader) sees only the
        // start and records a round hole, which loses the whole slot wall.
        let d = parse(
            "\
M48
; #@! TF.FileFunction,Plated,1,2,PTH
FMAT,2
METRIC
T1C0.600
%
G90
G05
T1
X3.0Y4.0G85X9.0Y4.0
M30
",
        );
        assert_eq!(d.holes.len(), 1);
        let h = &d.holes[0];
        assert!((h.x - 3.0).abs() < 1e-9 && (h.y - 4.0).abs() < 1e-9);
        assert_eq!(
            h.to.map(|(x, y)| ((x * 1e6) as i64, (y * 1e6) as i64)),
            Some((9_000_000, 4_000_000))
        );
        assert!(
            (h.diameter - 0.6).abs() < 1e-9,
            "the routed width is the tool"
        );
    }

    #[test]
    fn g85_slot_in_an_inch_file_scales_both_ends() {
        // The unit factor must reach the SECOND coordinate pair too. Applying
        // it only to the start puts the far end 25x too close to the origin,
        // which turns a 3 mm slot into a wall sweeping most of the board.
        let d = parse(
            "\
M48
FMAT,2
INCH,TZ
T4C0.0236
%
G90
G05
T4
X5.0945Y-5.3064G85X5.0945Y-5.4667
M30
",
        );
        assert_eq!(d.holes.len(), 1);
        let h = &d.holes[0];
        assert!((h.x - 5.0945 * 25.4).abs() < 1e-6, "start x was {}", h.x);
        let (tx, ty) = h.to.expect("a slot end");
        assert!((tx - 5.0945 * 25.4).abs() < 1e-6, "end x was {tx}");
        assert!((ty - (-5.4667) * 25.4).abs() < 1e-6, "end y was {ty}");
        assert!((h.diameter - 0.0236 * 25.4).abs() < 1e-6);
        // The slot is ~0.4 mm long, not the ~130 mm an unscaled end implies.
        let len = ((tx - h.x).powi(2) + (ty - h.y).powi(2)).sqrt();
        assert!(len < 5.0, "slot length {len} mm is not a real slot");
    }

    #[test]
    fn a_routed_slot_becomes_one_segment_per_cut_and_no_hit_per_rapid() {
        // Rout mode: G00 positions with the cutter up, M15 plunges, each G01
        // cuts, M16 retracts. The rapids must NOT become drilled hits, and the
        // two cuts must come back as two connected segments.
        let d = parse(
            "\
M48
FMAT,2
METRIC
T3C0.800
%
G90
T3
G00X2.0Y2.0
M15
G01X8.0Y2.0
G01X8.0Y6.0
M16
G00X20.0Y20.0
M30
",
        );
        assert_eq!(d.holes.len(), 2, "two cuts, and neither rapid is a hit");
        let a = &d.holes[0];
        assert!((a.x - 2.0).abs() < 1e-9 && (a.y - 2.0).abs() < 1e-9);
        assert_eq!(a.to, Some((8.0, 2.0)));
        let b = &d.holes[1];
        assert!((b.x - 8.0).abs() < 1e-9 && (b.y - 2.0).abs() < 1e-9);
        assert_eq!(b.to, Some((8.0, 6.0)));
        assert!((a.diameter - 0.8).abs() < 1e-9);
    }

    #[test]
    fn a_cut_after_retract_is_not_a_slot() {
        // A G01 with the cutter UP moves it, it does not cut. Treating every
        // G01 as a cut paints a plated wall across the board.
        let d = parse(
            "\
M48
FMAT,2
METRIC
T3C0.800
%
G90
T3
G00X2.0Y2.0
M15
G01X8.0Y2.0
M16
G01X40.0Y40.0
M30
",
        );
        assert_eq!(d.holes.len(), 1);
        assert_eq!(d.holes[0].to, Some((8.0, 2.0)));
    }

    #[test]
    fn a_file_without_m15_keeps_its_old_reading_of_g01() {
        // No `M15` anywhere, so this file is not in rout mode and its `G01`
        // lines are positioned hits, the reading every existing job relies on.
        // The fixture carries real `G01` coordinate lines, so a reader that
        // treated G01 as a cut regardless of mode would fail here rather than
        // slip through on a fixture that never exercises the path.
        let d = parse(
            "\
M48
FMAT,2
METRIC
T1C0.300
%
G90
T1
X1.0Y1.0
G01X2.0Y1.0
G01X3.0Y1.0
M30
",
        );
        assert_eq!(d.holes.len(), 3, "three positioned hits, no slots");
        assert!(
            d.holes.iter().all(|h| h.to.is_none()),
            "no cut may be inferred without an M15"
        );
    }

    #[test]
    fn a_g_code_is_read_as_a_number_not_a_text_prefix() {
        // `G5` must not swallow `G50`, and an arc must be an arc whichever axis
        // the exporter writes first, so the code is parsed rather than prefix
        // matched.
        assert_eq!(leading_g_code("G0"), Some(0));
        assert_eq!(leading_g_code("G00X1.0"), Some(0));
        assert_eq!(leading_g_code("G000"), Some(0));
        assert_eq!(leading_g_code("G01X1.0Y2.0"), Some(1));
        assert_eq!(leading_g_code("G2Y5.0X1.0I0J1"), Some(2));
        assert_eq!(leading_g_code("G3Y5.0"), Some(3));
        assert_eq!(leading_g_code("G05"), Some(5));
        assert_eq!(leading_g_code("G50"), Some(50));
        assert_eq!(leading_g_code("M30"), None);
        assert_eq!(leading_g_code("GX1.0"), None);
    }

    #[test]
    fn a_y_first_arc_line_is_still_an_arc() {
        // `G3Y..X..` is the same arc as `G3X..Y..`. Prefix matching on `G3X`
        // made the axis order decide whether a plated wall existed at all.
        let d = parse(
            "\
M48
FMAT,2
METRIC
T1C0.800
%
G90
T1
G00X5.0Y0.0
M15
G3Y5.0X0.0I-5.0J0.0
M16
M30
",
        );
        assert!(
            d.holes.len() >= 4 && d.holes.iter().all(|h| h.to.is_some()),
            "a Y-first arc must tessellate into slot segments, got {:?}",
            d.holes
        );
    }

    #[test]
    fn x2_layer_pair_is_read_and_a_malformed_one_is_not_invented() {
        let span = |t: &str| parse(t).span;
        assert_eq!(
            span("M48\n; #@! TF.FileFunction,Plated,1,2,PTH\nMETRIC\n%\nM30\n"),
            DeclaredSpan::Pair(LayerPair { from: 1, to: 2 })
        );
        assert_eq!(
            span("M48\n; #@! TF.FileFunction,Plated,1,4,PTH\nMETRIC\n%\nM30\n"),
            DeclaredSpan::Pair(LayerPair { from: 1, to: 4 })
        );
        assert_eq!(
            span(KICAD_PTH),
            DeclaredSpan::Pair(LayerPair { from: 1, to: 2 })
        );

        // A file that tried to state a pair and produced nonsense is NOT the
        // same as a file that stated nothing. Collapsing the two lets the
        // caller's "silence means through-hole on a simple job" rule swallow a
        // broken declaration, which is how a phantom short gets in.
        assert_eq!(
            span("M48\n; #@! TF.FileFunction,Plated,4,1,PTH\nMETRIC\n%\nM30\n"),
            DeclaredSpan::Unreadable
        );
        assert_eq!(
            span("M48\n; #@! TF.FileFunction,Plated,2,2,PTH\nMETRIC\n%\nM30\n"),
            DeclaredSpan::Unreadable
        );
        assert_eq!(
            span("M48\n; #@! TF.FileFunction,Plated,0,3,PTH\nMETRIC\n%\nM30\n"),
            DeclaredSpan::Unreadable
        );

        // An attribute form that carries no pair field, and a file with no
        // attribute at all, both declare nothing.
        assert_eq!(
            span("M48\n; #@! TF.FileFunction,Plated,PTH\nMETRIC\n%\nM30\n"),
            DeclaredSpan::Absent
        );
        assert_eq!(
            span("M48\nMETRIC\nT1C0.3\n%\nT1\nX1.0Y1.0\nM30\n"),
            DeclaredSpan::Absent
        );
    }

    #[test]
    fn tz_pads_on_the_left_and_lz_pads_on_the_right() {
        // Excellon's zero words are the opposite way round from Gerber's `%FSL`.
        // `TZ` = trailing zeros KEPT, so leading ones are suppressed and the
        // token is right-justified; `LZ` = leading zeros kept, trailing
        // suppressed, so it is left-justified. Getting this backwards moves a
        // coordinate by orders of magnitude, silently.
        //
        // The check is pinned to a real board: the LumenPnP vacuum interposer
        // ships `INCH,TZ` with `X47047`, and its Edge.Cuts outline runs from
        // x = 119.5 mm to x = 128.5 mm. Only the right-justified reading
        // (4.7047 inch = 119.50 mm) lands on that board at all; the other gives
        // 47.047 inch = 1195 mm, ten times the whole board.
        let d = parse(
            "\
M48
INCH,TZ
T1C0.0197
%
G90
G05
T1
X47047Y-28425
M30
",
        );
        assert_eq!(d.holes.len(), 1);
        assert!(
            (d.holes[0].x - 4.7047 * 25.4).abs() < 1e-3,
            "TZ must right-justify: got {} mm, the board's left edge is 119.5 mm",
            d.holes[0].x
        );
        // The same digits under LZ are left-justified instead.
        let d = parse(
            "\
M48
INCH,LZ
T1C0.0197
%
G90
G05
T1
X47047Y28425
M30
",
        );
        assert!(
            (d.holes[0].x - 47.047 * 25.4).abs() < 1e-2,
            "LZ must left-justify: got {} mm",
            d.holes[0].x
        );
    }

    #[test]
    fn a_g85_slot_with_implicit_decimals_scales_both_ends_the_same_way() {
        // The inch G85 test above uses explicit decimal points, which would
        // pass even if the second coordinate pair skipped the zero-suppression
        // reader entirely. This one has no decimal points at all, so the far
        // end only lands in the right place if it went through the same
        // int/dec split and the same justification as the near end.
        let d = parse(
            "\
M48
INCH,TZ
T1C0.0236
%
G90
G05
T1
X0050000Y0020000G85X0070000Y0020000
M30
",
        );
        assert_eq!(d.holes.len(), 1);
        let h = &d.holes[0];
        // 2:4 inch, right-justified: 0050000 -> 005.0000 -> 5.0 inch.
        assert!((h.x - 5.0 * 25.4).abs() < 1e-3, "start x was {}", h.x);
        let (tx, ty) = h.to.expect("a slot end");
        assert!((tx - 7.0 * 25.4).abs() < 1e-3, "end x was {tx}");
        assert!((ty - 2.0 * 25.4).abs() < 1e-3, "end y was {ty}");
    }

    #[test]
    fn a_routed_arc_is_tessellated_and_never_becomes_a_hole_at_its_endpoint() {
        // A quarter-circle cut of radius 5 from (5,0) to (0,5) about the
        // origin. The chord between those points passes 1.46 mm inside the true
        // arc, so a chord-only reading would miss any copper the arc bulges
        // towards. Every returned segment must sit on the radius.
        let d = parse(
            "\
M48
FMAT,2
METRIC
T1C0.800
%
G90
T1
G00X5.0Y0.0
M15
G03X0.0Y5.0I-5.0J0.0
M16
M30
",
        );
        // A quarter turn at 16 steps per full circle is exactly 4 segments.
        // Asserting the COUNT is what catches a direction swap: read clockwise,
        // the same two endpoints describe the 270-degree way round, which is 12
        // segments that still all sit on the radius and still start and end in
        // the right places.
        assert_eq!(
            d.holes.len(),
            4,
            "a quarter arc is 4 steps; {} means the sweep went the long way",
            d.holes.len()
        );
        // Every segment stays in the first quadrant, which the long way round
        // does not.
        for h in &d.holes {
            let (tx, ty) = h.to.expect("an arc segment is a slot");
            assert!(
                h.x >= -1e-9 && h.y >= -1e-9 && tx >= -1e-9 && ty >= -1e-9,
                "segment ({}, {}) -> ({tx}, {ty}) left the first quadrant",
                h.x,
                h.y
            );
        }
        for h in &d.holes {
            let (tx, ty) = h.to.expect("an arc segment is a slot, not a hole");
            for (x, y) in [(h.x, h.y), (tx, ty)] {
                assert!(
                    (x.hypot(y) - 5.0).abs() < 1e-6,
                    "point ({x}, {y}) is off the arc radius"
                );
            }
        }
        // The chain runs start to end without a gap.
        let first = &d.holes[0];
        assert!((first.x - 5.0).abs() < 1e-9 && first.y.abs() < 1e-9);
        let last = d.holes.last().unwrap().to.unwrap();
        assert!(last.0.abs() < 1e-9 && (last.1 - 5.0).abs() < 1e-9);
    }

    #[test]
    fn a_rout_arc_without_a_center_yields_no_geometry_rather_than_a_phantom_hole() {
        // No I/J: there is no arc to build. Falling through to the plain
        // coordinate reader would plant a round plated hit at the endpoint that
        // the file never described, which is copper we invented.
        let d = parse(
            "\
M48
FMAT,2
METRIC
T1C0.800
%
G90
T1
G00X5.0Y0.0
M15
G03X0.0Y5.0
M16
M30
",
        );
        assert!(
            d.holes.is_empty(),
            "an unbuildable arc must produce nothing, got {:?}",
            d.holes
        );
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
