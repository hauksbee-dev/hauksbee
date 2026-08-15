//! `hauksbee-ci check`: load-only spec validation, no simulation.
//!
//! The VS Code extension (and any editor tooling) needs "is this spec valid?"
//! answered in milliseconds; running a real co-simulation to find out times
//! out. This module parses and validates the spec, and, unless asked not to,
//! resolves and loads the referenced board file and validates every net /
//! component reference against it, all WITHOUT booting an emulator or solving
//! a single frame.
//!
//! Output is a list of [`Diagnostic`]s: an empty list is a valid spec. Each
//! diagnostic carries a stable machine `code`, a human `message`, a best-effort
//! `line`/`col` into the spec file (exact for TOML parse errors, which carry
//! spans; resolved by searching the spec text for the offending identifier for
//! validation errors; absent when not derivable), and a short suggested `fix`
//! where one is derivable (typically a did-you-mean).
//!
//! BOM, placement and assembly-variant artifacts are parsed and reconciled on
//! the same code path `run` uses; the `firmware` path is resolved and checked
//! to exist too. `check` therefore cannot pass a spec `run` will refuse at
//! startup. `--no-board` turns all on-disk artifact checks off together for an
//! editor loop where they may not be built or checked out yet.
//!
//! What `check` does NOT verify (these need a run, a model bind, or an
//! artifact that legitimately may not exist yet at edit time): whether the
//! firmware image actually LOADS on the target core, sensor-TOML contents,
//! scenario `part`/`profile` resolution against the model DB, tolerance patterns
//! actually matching a component, MCU/emulator resolution, and every behavioral
//! assertion.

use std::path::Path;

use serde::Serialize;

use crate::error::SpecError;
use crate::spec::Spec;

/// One `check` finding. Serialized as-is for `--json` (absent fields omitted).
#[derive(Debug, Clone, Serialize)]
pub struct Diagnostic {
    /// 1-based line in the spec file, when derivable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    /// 1-based column (byte offset within the line), when derivable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub col: Option<u32>,
    /// Stable machine code for the error kind. The taxonomy:
    /// `io` (spec file unreadable), `toml-parse` (TOML syntax / type errors),
    /// `unknown-field` (a key the spec vocabulary does not have),
    /// `missing-field` (a required key absent), `unknown-kind` (a closed-
    /// vocabulary token nothing matches: assertion kind, peripheral type,
    /// supply kind, waveform, ...), `unknown-id` (a reference to an undeclared
    /// scenario/peripheral/sensor id), `unknown-net` (a net not on the board),
    /// `unknown-ref` (a component reference not on the board), `bad-bound`
    /// (a numeric value outside its documented bounds), `conflicting-fields`
    /// (mutually exclusive keys set together), `board-load` (the board file
    /// failed to resolve/load), `firmware-missing` (the `firmware` path does not
    /// resolve to a readable image), `firmware-format` (the image is there but is
    /// not the format its extension claims), `invalid-spec` (anything else).
    pub code: &'static str,
    /// Human-readable description of the problem.
    pub message: String,
    /// Short suggested fix, when one is derivable (usually a did-you-mean).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

impl Diagnostic {
    /// Render for the terminal: `file:line:col: [code] message (fix)`.
    pub fn render_human(&self, file: &Path) -> String {
        let mut loc = file.display().to_string();
        if let Some(l) = self.line {
            loc.push_str(&format!(":{l}"));
            if let Some(c) = self.col {
                loc.push_str(&format!(":{c}"));
            }
        }
        let fix = self
            .fix
            .as_ref()
            .map(|f| format!(" ({f})"))
            .unwrap_or_default();
        format!("{loc}: [{}] {}{fix}", self.code, self.message)
    }
}

/// Options for [`check_spec`].
#[derive(Debug, Clone, Default)]
pub struct CheckOptions {
    /// Skip resolving the on-disk artifacts (the board file AND the firmware
    /// image): parse + structural validation only. For editor loops where the
    /// board is large or not checked out, or the firmware is not built yet.
    pub no_board: bool,
    /// Extra model directory, with the same highest-priority layer used by
    /// `run --models-dir`. Manufacturing identity reconciliation consumes the
    /// model library, so `check` must use the identical authority or it can
    /// accept a BOM/placement combination that `run` later refuses.
    pub models_dir: Option<std::path::PathBuf>,
}

/// Validate the spec at `path` without running anything. Empty vec = valid.
///
/// Phases, each contributing diagnostics:
/// 1. read the file (`io`),
/// 2. TOML parse + deserialize into [`Spec`] (`toml-parse` / `unknown-field`
///    / `missing-field`, with exact line/col from the parser's span),
/// 3. board-independent structural validation, ALL independent errors
///    collected ([`Spec::validate_all`]),
/// 4. unless `no_board`: resolve + load the board and reconcile its BOM,
///    placement and assembly variant (`board-load` / `invalid`), then validate
///    every referenced net (`unknown-net`) and component reference
///    (`unknown-ref`) against that exact assembled input,
/// 5. unless `no_board`: resolve the `firmware` path and check it exists and is
///    readable (`firmware-missing`), on the same code path `run` uses.
pub fn check_spec(path: &Path, opts: &CheckOptions) -> Vec<Diagnostic> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            return vec![Diagnostic {
                line: None,
                col: None,
                code: "io",
                message: format!("cannot read spec '{}': {e}", path.display()),
                fix: None,
            }];
        }
    };

    // Parse ourselves (rather than through Spec::load) so the toml error's
    // SPAN survives; Spec::load flattens it into a display string.
    let mut spec: Spec = match toml::from_str(&text) {
        Ok(s) => s,
        Err(e) => return vec![toml_diagnostic(&text, &e)],
    };
    spec.base_dir = crate::spec::base_dir_of(path);
    spec.normalize();

    let mut diags: Vec<Diagnostic> = Vec::new();
    for err in spec.validate_all() {
        push_spec_error(&mut diags, &text, &err);
    }

    if !opts.no_board {
        // The board phase only makes sense on a spec whose structure parsed;
        // structural errors above do not block it (they are independent), but
        // a failed board load ends the phase (net/ref checks need the board).
        let board_path = spec.board_path();
        match crate::runner::load_board(&board_path) {
            Err(e) => {
                let (line, col) = locate(&text, &spec.board.display().to_string())
                    .map(|(l, c)| (Some(l), Some(c)))
                    .unwrap_or((None, None));
                diags.push(Diagnostic {
                    line,
                    col,
                    code: "board-load",
                    message: e.to_string(),
                    fix: None,
                });
            }
            Ok(board) => {
                let extra: Vec<&Path> = opts.models_dir.as_deref().into_iter().collect();
                let lib = hauksbee_models::ModelLibrary::builtin_with_user_dirs(&extra);
                match crate::runner::prepare_assembly_inputs(&spec, board, &lib) {
                    Err(error) => push_spec_error(&mut diags, &text, &error),
                    Ok(prepared) => {
                        let known: Vec<String> =
                            prepared.board.nets.iter().map(|n| n.name.clone()).collect();
                        if let Err(e) = prepared.spec.check_nets(&known) {
                            push_spec_error(&mut diags, &text, &e);
                        }
                        let known_refs: Vec<String> = prepared
                            .board
                            .components
                            .iter()
                            .map(|c| c.reference.clone())
                            .collect();
                        for e in crate::runner::component_ref_errors(&prepared.spec, &known_refs) {
                            push_spec_error(&mut diags, &text, &e);
                        }
                    }
                }
            }
        }
        if let Some(d) = firmware_diagnostic(&spec, &text) {
            diags.push(d);
        }
    }
    // Report in FILE order. The phases above run structural-then-board, so an
    // unknown net on line 6 came out after a bad bound on line 12, and a reader
    // working down their spec had to jump back and forth. Diagnostics with no
    // line (an unreadable file, a message naming nothing that appears in the
    // text) cannot be placed, so they go last rather than at an arbitrary
    // line 0. The sort is stable, so two findings on one line keep the phase
    // order that produced them.
    diags.sort_by_key(|d| (d.line.unwrap_or(u32::MAX), d.col.unwrap_or(u32::MAX)));
    diags
}

/// Resolve the spec's `firmware` and confirm an image is actually there.
///
/// Same two steps `run` takes, in the same order, so the two verdicts cannot
/// disagree: resolve a PlatformIO project / build tree / zip down to the compiled
/// image (a bare `.elf`/`.hex` passes through), then check that image exists and
/// opens. Without this, `check` printed "OK" on a spec whose `firmware` pointed
/// nowhere, and `run` then exited 2 on the very same file: the editor-facing
/// validator disagreeing with the runner about whether a spec is valid.
fn firmware_diagnostic(spec: &Spec, text: &str) -> Option<Diagnostic> {
    let declared = spec.firmware.as_ref()?.display().to_string();
    let path = spec.firmware_path()?;
    let resolved = match hauksbee_engine::firmware_input::resolve_firmware_cli(&path) {
        Ok(Some(r)) => r.path,
        Ok(None) => path,
        Err(e) => return Some(firmware_diag(text, &declared, &e.to_string())),
    };
    match hauksbee_engine::validate_firmware_path(&resolved) {
        Ok(_) => {
            // The image exists; is it the format its extension claims? `run`
            // refuses a .hex renamed .elf, so `check` has to as well, or the
            // editor-facing validator says OK about a spec the runner rejects.
            crate::runner::firmware_format_mismatch(&resolved).map(|m| Diagnostic {
                code: "firmware-format",
                // The message already names both repairs; the missing-file fix
                // ("build the firmware first") would be wrong advice here.
                fix: None,
                ..firmware_diag(text, &declared, &m)
            })
        }
        Err(e) => Some(firmware_diag(text, &declared, &e.to_string())),
    }
}

/// A `firmware-missing` diagnostic pointed at the spec's `firmware` value, with
/// the same "(from the spec's `firmware = ...`)" trailer `run` prints.
fn firmware_diag(text: &str, declared: &str, message: &str) -> Diagnostic {
    let (line, col) = locate(text, declared)
        .map(|(l, c)| (Some(l), Some(c)))
        .unwrap_or((None, None));
    Diagnostic {
        line,
        col,
        code: "firmware-missing",
        message: format!("{message} (from the spec's `firmware = \"{declared}\"`)"),
        fix: Some(
            "build the firmware first, fix the path, or pass --no-board to validate \
             structure only"
                .to_string(),
        ),
    }
}

/// Turn a toml deserialization error into a diagnostic with exact line/col
/// (the toml crate reports a byte span into the source text).
fn toml_diagnostic(text: &str, e: &toml::de::Error) -> Diagnostic {
    let (line, col) = match e.span() {
        Some(span) => {
            let (l, c) = line_col(text, span.start);
            (Some(l), Some(c))
        }
        None => (None, None),
    };
    let message = e.message().to_string();
    // serde's deny_unknown_fields errors arrive through the TOML parser;
    // give them their own code (and a did-you-mean against the expected
    // list the message itself carries).
    let (code, fix) = if message.starts_with("unknown field") {
        ("unknown-field", unknown_field_fix(&message))
    } else if message.starts_with("missing field") {
        ("missing-field", None)
    } else {
        ("toml-parse", None)
    };
    Diagnostic {
        line,
        col,
        code,
        message,
        fix,
    }
}

/// Flatten a [`SpecError`] into diagnostics (an `UnknownNets` carries several
/// findings; a `Many` carries several errors).
fn push_spec_error(diags: &mut Vec<Diagnostic>, text: &str, err: &SpecError) {
    match err {
        SpecError::Many(errors) => {
            for e in errors {
                push_spec_error(diags, text, e);
            }
        }
        SpecError::UnknownNets(items) => {
            for (net, ctx, suggestions) in items {
                let (line, col) = locate(text, net)
                    .map(|(l, c)| (Some(l), Some(c)))
                    .unwrap_or((None, None));
                let fix = if suggestions.is_empty() {
                    None
                } else {
                    Some(format!("did you mean: {}?", suggestions.join(", ")))
                };
                diags.push(Diagnostic {
                    line,
                    col,
                    code: "unknown-net",
                    message: format!("'{net}' (referenced in {ctx}) is not a net on the board"),
                    fix,
                });
            }
        }
        SpecError::Io(m) => diags.push(Diagnostic {
            line: None,
            col: None,
            code: "io",
            message: m.clone(),
            fix: None,
        }),
        SpecError::Toml { message, .. } => diags.push(Diagnostic {
            line: None,
            col: None,
            code: "toml-parse",
            message: message.clone(),
            fix: None,
        }),
        SpecError::Invalid(m) => {
            // The did-you-mean lives in ONE place on a diagnostic: `fix`. It
            // arrives spliced into the validation message (that is the shape
            // `run`'s stderr wants, where there is no separate field for it),
            // and the renderer appends `fix` after the message, so leaving it in
            // both printed it twice: "... (did you mean 'voltage'?) (did you
            // mean 'voltage'?)". Split it out rather than dropping either half.
            let (message, fix) = split_did_you_mean(m);
            // Locate against the message with the suggestion removed: otherwise
            // an identifier that only appears in the suggestion could win the
            // line/col, pointing the editor at the wrong token.
            let (line, col) = locate_from_message(text, &message);
            diags.push(Diagnostic {
                line,
                col,
                code: classify_invalid(m),
                message,
                fix,
            });
        }
    }
}

/// Best-effort machine code for a validation message. The messages are the
/// crate's real error surface (tested word by word), so classifying on their
/// stable phrasing is deliberate: the taxonomy lives HERE, in one place,
/// rather than threaded through every error construction site.
fn classify_invalid(msg: &str) -> &'static str {
    if msg.contains("references unknown component") {
        "unknown-ref"
    } else if msg.contains("unknown assertion kind")
        || msg.contains("unknown type")
        || msg.contains("unknown kind")
        || msg.contains("unknown waveform")
        || msg.contains("unknown usb profile")
        || msg.contains("unknown chemistry")
        || msg.contains("unknown distribution")
        || msg.contains("unknown ensemble mode")
        || msg.contains("sweep must be")
    {
        "unknown-kind"
    } else if msg.contains("is scoped to scenario")
        || msg.contains("reads id")
        || msg.contains("is outside this spec's ensemble")
    {
        "unknown-id"
    } else if msg.contains("sets both")
        || msg.contains("mutually exclusive")
        || msg.contains("does not compose")
        || msg.contains("only meaningful with")
        || msg.contains("does not support")
    {
        "conflicting-fields"
    } else if msg.contains("needs") || msg.contains("has no [[assert]]") {
        "missing-field"
    } else if msg.contains("must be")
        || msg.contains("greater than max")
        || msg.contains("is a fraction")
        || msg.contains("tolerance is a fraction")
        || msg.contains("in (0, 100)")
        || msg.contains("0..1")
        || msg.contains("between 0.0 and 1.0")
    {
        "bad-bound"
    } else {
        "invalid-spec"
    }
}

/// Split a validation message into (message without its did-you-mean clause,
/// the clause as a `fix`). Returns the message unchanged and `None` when there
/// is no clause.
///
/// Both spellings the crate produces are handled: the parenthesised
/// `... 'voltag' (did you mean 'voltage'?)` of the closed-vocabulary errors, and
/// the trailing `...; did you mean: R1, R9?` of the net / reference suggesters.
/// The punctuation that introduced the clause goes with it, so what is left
/// reads as a finished sentence rather than trailing a stray `(` or `;`.
fn split_did_you_mean(msg: &str) -> (String, Option<String>) {
    let Some(start) = msg.find("did you mean") else {
        return (msg.to_string(), None);
    };
    // Every construction site ends the clause with a question mark.
    let end = msg[start..]
        .find('?')
        .map(|i| start + i + 1)
        .unwrap_or(msg.len());
    let clause = msg[start..end].to_string();
    let mut lead = start;
    let mut trail = end;
    if msg[..lead].ends_with(" (") {
        lead -= 2;
        if msg[trail..].starts_with(')') {
            trail += 1;
        }
    } else if msg[..lead].ends_with("; ") {
        lead -= 2;
    } else if msg[..lead].ends_with(' ') {
        lead -= 1;
    }
    let cleaned = format!("{}{}", &msg[..lead], &msg[trail..]);
    (cleaned.trim().to_string(), Some(clause))
}

/// For an `unknown field \`x\`, expected ...` message: suggest the nearest
/// expected field name, when one is within typo distance.
fn unknown_field_fix(msg: &str) -> Option<String> {
    // Message shape: unknown field `typo`, expected one of `a`, `b`, ...
    let mut ticks = msg.split('`');
    let _ = ticks.next()?; // "unknown field "
    let field = ticks.next()?;
    let expected: Vec<&str> = msg
        .split(", expected")
        .nth(1)?
        .split('`')
        .skip(1)
        .step_by(2)
        .collect();
    crate::error::did_you_mean(field, &expected).map(|s| format!("did you mean '{s}'?"))
}

/// Find the identifiers a validation message quotes ('...') and resolve the
/// first one that appears in the spec text to a line/col. Best effort: a name
/// that appears several times resolves to its first occurrence.
fn locate_from_message(text: &str, msg: &str) -> (Option<u32>, Option<u32>) {
    let mut parts = msg.split('\'');
    // Quoted identifiers are the odd-numbered fragments.
    let _ = parts.next();
    while let Some(ident) = parts.next() {
        let _ = parts.next(); // skip the fragment between quotes
        if ident.is_empty() {
            continue;
        }
        if let Some((l, c)) = locate(text, ident) {
            return (Some(l), Some(c));
        }
    }
    (None, None)
}

/// 1-based (line, col) of `ident`'s first occurrence in `text`, preferring a
/// TOML-string occurrence (`"ident"`) over a bare substring so an identifier
/// that also happens to appear in a comment or key resolves to its value.
fn locate(text: &str, ident: &str) -> Option<(u32, u32)> {
    if ident.is_empty() {
        return None;
    }
    let quoted = format!("\"{ident}\"");
    let byte = match text.find(&quoted) {
        Some(i) => i + 1, // point at the identifier, not its opening quote
        None => text.find(ident)?,
    };
    Some(line_col(text, byte))
}

/// 1-based (line, col) of a byte offset in `text`.
fn line_col(text: &str, byte: usize) -> (u32, u32) {
    let byte = byte.min(text.len());
    let before = &text[..byte];
    let line = before.bytes().filter(|&b| b == b'\n').count() as u32 + 1;
    let col = (byte - before.rfind('\n').map(|i| i + 1).unwrap_or(0)) as u32 + 1;
    (line, col)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_col_is_one_based_and_line_aware() {
        let text = "abc\ndef\nghi";
        assert_eq!(line_col(text, 0), (1, 1));
        assert_eq!(line_col(text, 4), (2, 1));
        assert_eq!(line_col(text, 6), (2, 3));
        assert_eq!(line_col(text, 8), (3, 1));
    }

    #[test]
    fn locate_prefers_the_quoted_occurrence() {
        // "LED" appears in a comment on line 1 and as a value on line 2; the
        // quoted (value) occurrence must win.
        let text = "# the LED net\nnet = \"LED\"\n";
        let (l, c) = locate(text, "LED").unwrap();
        assert_eq!((l, c), (2, 8));
    }

    #[test]
    fn the_did_you_mean_clause_moves_out_of_the_message_into_the_fix() {
        // M5: the message carried the suggestion AND the renderer appended `fix`,
        // so `check` printed the did-you-mean twice. `fix` is the single source.
        let msg = "unknown assertion kind 'voltag' (did you mean 'voltage'?) (expected ...)";
        let (message, fix) = split_did_you_mean(msg);
        assert_eq!(fix.as_deref(), Some("did you mean 'voltage'?"));
        assert_eq!(message, "unknown assertion kind 'voltag' (expected ...)");
        assert!(!message.contains("did you mean"));

        // The suggester's trailing-clause spelling, and its punctuation.
        let refs = "max_current assert references unknown component 'R98'; did you mean: R9?";
        let (message, fix) = split_did_you_mean(refs);
        assert_eq!(fix.as_deref(), Some("did you mean: R9?"));
        assert_eq!(
            message,
            "max_current assert references unknown component 'R98'"
        );

        let (message, fix) = split_did_you_mean("no clause here");
        assert_eq!(fix, None);
        assert_eq!(message, "no clause here");
    }

    #[test]
    fn the_rendered_line_carries_the_suggestion_exactly_once() {
        let d = Diagnostic {
            line: None,
            col: None,
            code: "unknown-kind",
            message: "unknown assertion kind 'voltag'".to_string(),
            fix: Some("did you mean 'voltage'?".to_string()),
        };
        let rendered = d.render_human(Path::new("ci/power-up.toml"));
        assert_eq!(rendered.matches("did you mean").count(), 1, "{rendered}");
    }

    #[test]
    fn classify_covers_the_real_message_shapes() {
        for (msg, code) in [
            ("unknown assertion kind 'voltag' (expected ...)", "unknown-kind"),
            ("peripheral 'X': unknown waveform 'square' (expected ...)", "unknown-kind"),
            ("voltage assertion needs a `net`", "missing-field"),
            (
                "toggle assertion on 'D13' sets both `freq_hz` and `min_toggles`; use one",
                "conflicting-fields",
            ),
            (
                "voltage assertion 'x': min (5) is greater than max (3), a window nothing can satisfy",
                "bad-bound",
            ),
            ("duration_ms must be a positive, finite number", "bad-bound"),
            (
                "max_current assert references unknown component 'R99'",
                "unknown-ref",
            ),
            (
                "rail_window assertion 'x' is scoped to scenario 'boot', but no [[scenario]] declares that id",
                "unknown-id",
            ),
        ] {
            assert_eq!(classify_invalid(msg), code, "message: {msg}");
        }
    }
}
