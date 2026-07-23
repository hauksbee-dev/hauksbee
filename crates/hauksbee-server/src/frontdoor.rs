//! The "drop your board, get a report" analysis API — the report backend the
//! React landing page calls.
//!
//! A non-CLI, non-engineer user runs `hauksbee serve`, opens the printed URL,
//! drops a board file onto the React drop zone, and gets back the plain-language
//! verdict, the full report, and a 2D map of where the parts sit — all rendered
//! by the React app (W6 §1). There is no server-rendered HTML page anymore: the
//! one web experience is the React app in `frontend/dist`, and this module is the
//! JSON API it fetches (`/api/analyze`, `/api/analyze-with-firmware`).
//!
//! This module is the thin HTTP layer only. The actual analysis is injected as a
//! callback (`Analyzer` / `FirmwareAnalyzer`) so the server crate stays free of
//! any dependency on the engine/extract crates (which depend on *this* crate);
//! the `hauksbee` binary wires the engine's `analyze_json` in. The routes are
//! merged into the unified server router (see [`crate::Server`]) so `serve` and
//! `run --serve` both expose them alongside the WebSocket sim and the static
//! React bundle.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Multipart, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::post;
use axum::Router;

/// Analyze an uploaded board: `(file_name, board_bytes) -> JSON report string`.
/// The board is passed as raw `&[u8]` (never lossy-decoded) so a binary format
/// (Altium `.PcbDoc`, an OLE2 container) survives intact; the analyzer's
/// extractor sniffs binary-vs-text itself. Boxed so the engine can supply its
/// `analyze_json` without the server crate depending on the engine.
pub type Analyzer = Arc<dyn Fn(&str, &[u8]) -> String + Send + Sync>;

/// Analyze a board AND an optional firmware: `(board_name, board_bytes,
/// Option<(firmware_name, firmware_bytes)>) -> JSON report string`. Board and
/// firmware are both passed as raw `&[u8]` (never lossy-decoded) so an uploaded
/// binary board or ELF stays intact. Parallel to [`Analyzer`] so the server
/// crate stays engine-free and the existing `/api/analyze` path + call sites
/// are untouched.
pub type FirmwareAnalyzer =
    Arc<dyn Fn(&str, &[u8], Option<(&str, &[u8])>) -> String + Send + Sync>;

/// Largest board upload accepted (256 MiB). Real flagship layouts blow past a
/// timid cap (the 3,443-component Tarski InputSystem .kicad_pcb is 44 MiB), and
/// the server is localhost-only, so the limit exists solely to stop a
/// pathological upload from exhausting memory. When it does trip, axum answers
/// with a plain-text 413 ("Failed to buffer the request body"), which is why
/// the frontend reads error bodies as text, not JSON.
const MAX_UPLOAD_BYTES: usize = 256 * 1024 * 1024;

struct FrontDoorState {
    analyze: Analyzer,
}

/// Build the board-only analysis routes (`/api/analyze`). No server-rendered
/// page: the React bundle owns `/`. Kept for tests and any board-only caller;
/// production wires the firmware-aware [`api_routes`] into the unified server.
pub fn router(analyze: Analyzer) -> Router {
    let state = Arc::new(FrontDoorState { analyze });
    Router::new()
        .route("/api/analyze", post(analyze_handler))
        .layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES))
        .with_state(state)
}

struct FirmwareState {
    analyze: FirmwareAnalyzer,
}

/// Build the analysis API routes the React landing page calls: board-only
/// analysis at `/api/analyze` and the firmware co-sim at
/// `/api/analyze-with-firmware` (multipart: `board` + optional `firmware`).
///
/// Both endpoints share the one [`FirmwareAnalyzer`]; the board-only path simply
/// passes `None` for the firmware. Returns a `Router<()>` so it merges cleanly
/// into the unified server router alongside the WebSocket sim and the static
/// React bundle (W6 §1: one server path).
pub fn api_routes(analyze: FirmwareAnalyzer) -> Router {
    let state = Arc::new(FirmwareState { analyze });
    Router::new()
        .route("/api/analyze", post(analyze_handler_fw))
        .route("/api/analyze-with-firmware", post(analyze_firmware_handler))
        .layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES))
        .with_state(state)
}

/// Back-compat alias for [`api_routes`] (the firmware-aware analysis routes).
/// No server-rendered page: the React bundle owns `/`.
pub fn router_with_firmware(analyze: FirmwareAnalyzer) -> Router {
    api_routes(analyze)
}

/// Accept the raw board file as the request body, with the original filename in
/// the `X-Board-Filename` header (the page sets it). Returns the analysis JSON.
async fn analyze_handler(
    State(state): State<Arc<FrontDoorState>>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let file_name = headers
        .get("x-board-filename")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("board")
        .to_string();

    // Board files may be text (KiCad/Eagle/IPC), a zip (gerbers) or a binary
    // container (Altium .PcbDoc). The analyzer's extractor sniffs the format
    // from the RAW bytes; decoding here (the old lossy-UTF8 view) corrupted the
    // binary formats before they were ever parsed.
    let json = (state.analyze)(&file_name, &body);

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        json,
    )
}

/// Board-only analysis for the firmware-aware router: same contract as
/// [`analyze_handler`] (raw body + `X-Board-Filename`) but routed through the
/// [`FirmwareAnalyzer`] with `None` firmware, so the single-file drop path is
/// unchanged when the firmware-aware router is mounted.
async fn analyze_handler_fw(
    State(state): State<Arc<FirmwareState>>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let file_name = headers
        .get("x-board-filename")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("board")
        .to_string();
    // Raw bytes, same as [`analyze_handler`]: a binary board must not be
    // lossy-decoded on its way to the analyzer.
    let json = (state.analyze)(&file_name, &body, None);
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        json,
    )
}

/// Accept a `multipart/form-data` upload with a `board` part (required) and a
/// `firmware` part (optional). Both parts are passed as raw `&[u8]` — NEVER
/// lossy-decoded, which would corrupt an ELF or a binary board (Altium
/// .PcbDoc); the analyzer's extractor sniffs binary-vs-text itself. Falls back
/// to a board-only analysis when no firmware part is present.
async fn analyze_firmware_handler(
    State(state): State<Arc<FirmwareState>>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    let mut board_name = "board".to_string();
    let mut board_bytes: Option<Vec<u8>> = None;
    let mut fw_name = String::new();
    let mut fw_bytes: Option<Vec<u8>> = None;

    loop {
        // Distinguish "no more parts" (Ok(None), the clean end of the stream)
        // from a malformed/truncated multipart body (Err). The old
        // `while let Ok(Some(..))` collapsed both into "stop looping", so a
        // corrupt upload fell through to the misleading "no board file in the
        // upload" error below instead of naming the real cause.
        let field = match multipart.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => {
                let json = format!(
                    "{{\"ok\":false,\"error\":\"malformed multipart upload: {}\"}}",
                    e.to_string().replace('"', "'")
                );
                return (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "application/json")],
                    json,
                );
            }
        };
        let part = field.name().unwrap_or("").to_string();
        let filename = field.file_name().map(|s| s.to_string());
        let data = match field.bytes().await {
            Ok(b) => b,
            Err(e) => {
                let json = format!(
                    "{{\"ok\":false,\"error\":\"failed to read upload part: {}\"}}",
                    e.to_string().replace('"', "'")
                );
                return (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "application/json")],
                    json,
                );
            }
        };
        match part.as_str() {
            // Accept "board" or "file" for the PCB part. The browser form uses a
            // `file` input id and the raw /api/analyze path is conceptually "the
            // file", so a caller who reaches for either name should just work
            // instead of hitting a confusing "expected a 'board' part" error.
            "board" | "file" => {
                if let Some(f) = filename {
                    board_name = f;
                }
                board_bytes = Some(data.to_vec());
            }
            "firmware" => {
                if let Some(f) = filename {
                    fw_name = f;
                }
                // Ignore an empty firmware part (e.g. the browser sent the field
                // with no file selected) so we cleanly fall back to board-only.
                if !data.is_empty() {
                    fw_bytes = Some(data.to_vec());
                }
            }
            _ => {}
        }
    }

    let board_bytes = match board_bytes {
        Some(b) => b,
        None => {
            let json =
                "{\"ok\":false,\"error\":\"no board file in the upload (expected a 'board' or 'file' part)\"}"
                    .to_string();
            return (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/json")],
                json,
            );
        }
    };
    let json = match &fw_bytes {
        Some(bytes) => (state.analyze)(&board_name, &board_bytes, Some((&fw_name, bytes))),
        None => (state.analyze)(&board_name, &board_bytes, None),
    };

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        json,
    )
}
