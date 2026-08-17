//! The "drop your board, get a report" analysis API; the report backend the
//! React landing page calls.
//!
//! A non-CLI, non-engineer user runs `hauksbee serve`, opens the printed URL,
//! drops a board file onto the React drop zone, and gets back the plain-language
//! verdict, the full report, and a 2D map of where the parts sit, all rendered
//! by the React app. There is one web experience: a single server path serving
//! the React bundle in `frontend/dist`, with no server-rendered HTML
//! alternative. This module is the JSON API that bundle fetches
//! (`/api/analyze`, `/api/analyze-with-firmware`).
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
use axum::extract::{DefaultBodyLimit, Multipart, Path as UrlPath, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};

pub use hauksbee_frontdoor_api::frontdoor::{
    Analyzer, CheckRunner, DatasheetChecker, DatasheetExtractor, DatasheetHooks, DatasheetJob,
    DatasheetReady, DatasheetSaver, DepInstaller, DepsStatus, FirmwareAnalyzer, LiveLaunch,
    LiveLauncher, ModelDrafter, SchematicAnalyzer, SchematicCheckRunner, SchematicLiveLauncher,
    ToolHooks,
};

/// Reject a request that a *website* in the user's browser made cross-origin to
/// our loopback server. The analysis/check endpoints can run an uploaded
/// PlatformIO project (`pio run` executes arbitrary `extra_scripts`), so a
/// drive-by `FormData` POST from any open tab would be code execution under the
/// user's account. `Sec-Fetch-Site` is set by the browser and CANNOT be forged
/// by page JS: our own same-origin page sends `same-origin`; a hostile
/// cross-site fetch sends `cross-site`/`same-site`. A non-browser client (curl,
/// the CLI, a test) sends no such header, so tooling is unaffected. This is the
/// standard private-network-access defense for a localhost server.
///
/// Returns `Some(response)` when the request must be refused, `None` to proceed.
fn reject_cross_site(
    headers: &HeaderMap,
) -> Option<(StatusCode, [(header::HeaderName, &'static str); 1], String)> {
    if let Some(site) = headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()) {
        // Browser-origin request: only our own page (same-origin) or a
        // direct address-bar navigation (none) may reach these endpoints.
        if site != "same-origin" && site != "none" {
            return Some((
                StatusCode::FORBIDDEN,
                [(header::CONTENT_TYPE, "application/json")],
                "{\"ok\":false,\"error\":\"cross-site request refused: the hauksbee analysis \
                 endpoints accept requests only from the hauksbee page itself\"}"
                    .to_string(),
            ));
        }
    }
    None
}

/// Largest board upload accepted (256 MiB). Real flagship layouts blow past a
/// timid cap (the 3,443-component Tarski InputSystem .kicad_pcb is 44 MiB), and
/// the server is localhost-only, so the limit exists solely to stop a
/// pathological upload from exhausting memory. When it does trip, axum answers
/// with a plain-text 413 ("Failed to buffer the request body"), which is why
/// the frontend reads error bodies as text, not JSON.
const MAX_UPLOAD_BYTES: usize = 256 * 1024 * 1024;

/// Rewrite axum's stock 413 ("length limit exceeded") so the body NAMES the
/// limit; the frontend shows error bodies verbatim, and a message that says
/// what the cap is lets it tell the user something actionable.
async fn name_upload_limit_413(resp: axum::response::Response) -> axum::response::Response {
    if resp.status() == StatusCode::PAYLOAD_TOO_LARGE {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "upload too large: this server accepts at most {} MB per request",
                MAX_UPLOAD_BYTES / (1024 * 1024)
            ),
        )
            .into_response();
    }
    resp
}

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
        .layer(axum::middleware::map_response(name_upload_limit_413))
        .with_state(state)
}

struct FirmwareState {
    analyze: FirmwareAnalyzer,
}

struct SchematicState {
    analyze: SchematicAnalyzer,
}

/// Build the analysis API routes the React landing page calls: board-only
/// analysis at `/api/analyze` and the firmware co-sim at
/// `/api/analyze-with-firmware` (multipart: `board` + optional `firmware`).
///
/// Both endpoints share the one [`FirmwareAnalyzer`]; the board-only path simply
/// passes `None` for the firmware. Returns a `Router<()>` so it merges cleanly
/// into the unified server router alongside the WebSocket sim and the static
/// React bundle, keeping the whole web experience on one server path.
pub fn api_routes(analyze: FirmwareAnalyzer) -> Router {
    let state = Arc::new(FirmwareState { analyze });
    Router::new()
        .route("/api/analyze", post(analyze_handler_fw))
        .route("/api/analyze-with-firmware", post(analyze_firmware_handler))
        .layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES))
        .layer(axum::middleware::map_response(name_upload_limit_413))
        .with_state(state)
}

/// Analysis routes which additionally accept a `schematic` multipart part.
/// The raw board endpoint remains available and supplies no companions.
pub fn api_routes_with_schematic(analyze: SchematicAnalyzer) -> Router {
    let state = Arc::new(SchematicState { analyze });
    Router::new()
        .route("/api/analyze", post(analyze_handler_schematic))
        .route(
            "/api/analyze-with-firmware",
            post(analyze_schematic_handler),
        )
        .layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES))
        .layer(axum::middleware::map_response(name_upload_limit_413))
        .with_state(state)
}

/// Back-compat alias for [`api_routes`] (the firmware-aware analysis routes).
/// No server-rendered page: the React bundle owns `/`.
pub fn router_with_firmware(analyze: FirmwareAnalyzer) -> Router {
    api_routes(analyze)
}

enum CheckCallback {
    Legacy(CheckRunner),
    Schematic(SchematicCheckRunner),
}

struct CheckState {
    check: CheckCallback,
}

/// The web checks route (`POST /api/check`, multipart: `board` + optional
/// `firmware` + `spec`). The spec part is the TOML body the browser's builder
/// composed, everything except the file paths, which the engine injects from
/// the uploaded parts. Merged into the unified router next to the analysis
/// routes.
pub fn check_route(check: CheckRunner) -> Router {
    let state = Arc::new(CheckState {
        check: CheckCallback::Legacy(check),
    });
    Router::new()
        .route("/api/check", post(check_handler))
        .layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES))
        .layer(axum::middleware::map_response(name_upload_limit_413))
        .with_state(state)
}

/// Schematic-aware checks route used by the shipped standalone app.
pub fn check_route_with_schematic(check: SchematicCheckRunner) -> Router {
    let state = Arc::new(CheckState {
        check: CheckCallback::Schematic(check),
    });
    Router::new()
        .route("/api/check", post(check_handler))
        .layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES))
        .layer(axum::middleware::map_response(name_upload_limit_413))
        .with_state(state)
}

struct LiveState {
    hub: Arc<crate::LiveHub>,
    launch: LiveLauncher,
}

struct SchematicLiveState {
    hub: Arc<crate::LiveHub>,
    launch: SchematicLiveLauncher,
}

/// The live-launch API: `POST /api/live/launch` (multipart `board` + optional
/// `firmware`, same parts as `/api/analyze-with-firmware`) boots a live sim
/// session for the upload and swaps it into the hub behind `/ws`;
/// `GET /api/live/status` reports whether (and which) a session is running so
/// the UI can confirm before replacing it. The launch runs an uploaded board
/// (and firmware) through the engine, so it carries the same
/// `reject_cross_site` guard as the other mutating endpoints.
pub fn live_routes(hub: Arc<crate::LiveHub>, launch: LiveLauncher) -> Router {
    let state = Arc::new(LiveState { hub, launch });
    Router::new()
        .route("/api/live/launch", post(live_launch_handler))
        .route("/api/live/status", get(live_status_handler))
        .layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES))
        .layer(axum::middleware::map_response(name_upload_limit_413))
        .with_state(state)
}

/// Live-launch routes which additionally accept a `schematic` multipart part.
pub fn live_routes_with_schematic(
    hub: Arc<crate::LiveHub>,
    launch: SchematicLiveLauncher,
) -> Router {
    let state = Arc::new(SchematicLiveState { hub, launch });
    Router::new()
        .route("/api/live/launch", post(live_launch_schematic_handler))
        .route("/api/live/status", get(live_status_schematic_handler))
        .layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES))
        .layer(axum::middleware::map_response(name_upload_limit_413))
        .with_state(state)
}

async fn live_status_schematic_handler(
    State(state): State<Arc<SchematicLiveState>>,
) -> impl IntoResponse {
    let body = match state.hub.active_board() {
        Some(name) => serde_json::json!({ "active": true, "board_name": name }),
        None => serde_json::json!({ "active": false }),
    };
    json_body(StatusCode::OK, body)
}

async fn live_launch_schematic_handler(
    State(state): State<Arc<SchematicLiveState>>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> impl IntoResponse {
    if let Some(response) = reject_cross_site(&headers) {
        return response;
    }
    let parts = match parse_upload(&mut multipart).await {
        Ok(parts) => parts,
        Err(message) => return json_error(&message),
    };
    let Some(board_bytes) = parts.board_bytes else {
        return json_error("no board file in the upload (expected a 'board' or 'file' part)");
    };
    let launch = state.launch.clone();
    let built = tokio::task::spawn_blocking(move || {
        let firmware = parts
            .fw_bytes
            .as_deref()
            .map(|bytes| (parts.fw_name.as_str(), bytes));
        let schematic = parts
            .schematic_bytes
            .as_deref()
            .map(|bytes| (parts.schematic_name.as_str(), bytes));
        (launch)(&parts.board_name, &board_bytes, firmware, schematic)
    })
    .await;
    match built {
        Ok(Ok(live)) => {
            let board_name = live.board_name.clone();
            let replaced = state.hub.launch(
                live.engine,
                live.board_name,
                live.board_file,
                live.keepalive,
            );
            json_body(
                StatusCode::OK,
                serde_json::json!({ "ok": true, "board_name": board_name, "replaced": replaced }),
            )
        }
        Ok(Err(message)) => json_error(&message),
        Err(_) => json_error("the live launch task panicked; see the server log"),
    }
}

async fn live_status_handler(State(state): State<Arc<LiveState>>) -> impl IntoResponse {
    let body = match state.hub.active_board() {
        Some(name) => serde_json::json!({ "active": true, "board_name": name }),
        None => serde_json::json!({ "active": false }),
    };
    json_body(StatusCode::OK, body)
}

/// POST `/api/live/launch`: build the engine for the uploaded board (blocking
/// pool; extract + bind + firmware load are CPU work) and install it as THE
/// live session. `{ok:true, board_name, replaced}` on success; a refusal (no
/// processor for the firmware, unloadable firmware, unreadable board) comes
/// back as `{ok:false, error}` with the engine's own message, so the report
/// stays up and the UI shows the reason instead of a dead spinner.
async fn live_launch_handler(
    State(state): State<Arc<LiveState>>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> impl IntoResponse {
    if let Some(resp) = reject_cross_site(&headers) {
        return resp;
    }
    let parts = match parse_upload(&mut multipart).await {
        Ok(p) => p,
        Err(msg) => return json_error(&msg),
    };
    let Some(board_bytes) = parts.board_bytes else {
        return json_error("no board file in the upload (expected a 'board' or 'file' part)");
    };
    let (board_name, fw_name, fw_bytes) = (parts.board_name, parts.fw_name, parts.fw_bytes);
    let launch = state.launch.clone();
    let built = tokio::task::spawn_blocking(move || match &fw_bytes {
        Some(bytes) => (launch)(&board_name, &board_bytes, Some((&fw_name, bytes))),
        None => (launch)(&board_name, &board_bytes, None),
    })
    .await;
    match built {
        Ok(Ok(live)) => {
            let board_name = live.board_name.clone();
            let replaced = state.hub.launch(
                live.engine,
                live.board_name,
                live.board_file,
                live.keepalive,
            );
            json_body(
                StatusCode::OK,
                serde_json::json!({ "ok": true, "board_name": board_name, "replaced": replaced }),
            )
        }
        Ok(Err(msg)) => json_error(&msg),
        Err(_) => json_error("the live launch task panicked; see the server log"),
    }
}

struct DepsState {
    status: DepsStatus,
    install: DepInstaller,
}

/// The dependency panel's backend: `GET /api/deps` (status via the engine's own
/// discovery) and `POST /api/deps/install/{id}` (run an install, streaming its
/// progress as Server-Sent Events). Merged into the unified router next to the
/// analysis routes. The install route executes an installer, so it carries the
/// same `reject_cross_site` guard as every other mutating endpoint: a hostile
/// page in another tab must not be able to trigger a download.
pub fn deps_routes(status: DepsStatus, install: DepInstaller) -> Router {
    let state = Arc::new(DepsState { status, install });
    Router::new()
        .route("/api/deps", get(deps_status_handler))
        .route("/api/deps/install/{id}", post(deps_install_handler))
        .with_state(state)
}

/// GET `/api/deps`: relay the engine's dependency JSON. The probe shells a few
/// `--version` checks, so it runs on the blocking pool rather than stalling the
/// async runtime.
async fn deps_status_handler(State(state): State<Arc<DepsState>>) -> impl IntoResponse {
    let status = state.status.clone();
    let json = tokio::task::spawn_blocking(move || (status)())
        .await
        .unwrap_or_else(|_| "{\"deps\":[]}".to_string());
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        json,
    )
}

/// One SSE frame: `event: <kind>` with the text as (possibly multi-line) data.
fn sse_event(kind: &str, text: &str) -> String {
    let mut s = format!("event: {kind}\n");
    let mut any = false;
    for line in text.lines() {
        s.push_str("data: ");
        s.push_str(line);
        s.push('\n');
        any = true;
    }
    if !any {
        s.push_str("data:\n");
    }
    s.push('\n');
    s
}

/// POST `/api/deps/install/{id}`: start the install and stream its progress as
/// SSE (`log` events line by line, then exactly one `done` or `error`). A
/// streaming response, because these are multi-minute downloads: a request that
/// blocks silently until the end is indistinguishable from a hang. SSE framing
/// is used (rather than bare chunked text) because the compression layer
/// exempts `text/event-stream`, so lines reach the browser as they happen
/// instead of sitting in a gzip buffer.
///
/// The installer keeps running if the browser disconnects mid-stream: an
/// interrupted half-install would be worse than a completed one the user
/// stopped watching.
async fn deps_install_handler(
    State(state): State<Arc<DepsState>>,
    UrlPath(id): UrlPath<String>,
    headers: HeaderMap,
) -> axum::response::Response {
    if let Some(resp) = reject_cross_site(&headers) {
        return resp.into_response();
    }
    let (tx, rx) = tokio::sync::mpsc::channel::<String>(256);
    let install = state.install.clone();
    tokio::task::spawn_blocking(move || {
        let result = {
            let tx = tx.clone();
            let mut sink = move |line: &str| {
                // A send failure means the browser went away; the install
                // continues (see the handler doc), we just stop relaying.
                // try_send, NOT blocking_send. A client that opens this POST
                // and then stops reading applies TCP backpressure, the channel
                // fills, and a blocking send parks this thread inside the very
                // loop that enforces the timeout: the child is never killed,
                // the RAII slot is never released, and every later install is
                // refused for the life of the process. Dropping a progress line
                // for a peer that is not listening costs nothing worth having.
                let _ = tx.try_send(sse_event("log", line));
            };
            (install)(&id, &mut sink)
        };
        let _ = match result {
            Ok(()) => tx.blocking_send(sse_event("done", "ok")),
            Err(e) => tx.blocking_send(sse_event("error", &e)),
        };
    });
    sse_response(rx)
}

/// Largest datasheet accepted (32 MiB). A datasheet is tens of pages of vector
/// art; the largest real ones (a 600-page MCU reference manual) are under 30 MiB.
/// The board limit would be absurd here, and the extraction copies the file and
/// renders its pages, so an enormous upload costs disk and CPU before anything
/// has been checked.
const MAX_DATASHEET_BYTES: usize = 32 * 1024 * 1024;

struct DatasheetState {
    hooks: DatasheetHooks,
}

/// The datasheet-extraction API:
/// `GET /api/models/extract/ready` (can an extraction run on this machine, and
/// what does it cost), `POST /api/models/extract` (multipart `datasheet` +
/// `part` + `kind` + `reference`; streams progress as SSE and ends with the
/// reviewable model card), and `POST /api/models/save` (JSON `{part, kind,
/// toml}`; writes an ACCEPTED card into the user's model library).
///
/// The split is the consent contract, not a convenience: extraction sends the
/// datasheet off this machine, so it happens only on an explicit request, and
/// its result is never written anywhere until a second explicit request says to
/// keep it. Both mutating routes carry `reject_cross_site`; the extract route
/// spends the user's LLM credit and the save route writes to their model
/// library, so neither may be triggered by a page in another tab.
pub fn datasheet_routes(hooks: DatasheetHooks) -> Router {
    let state = Arc::new(DatasheetState { hooks });
    Router::new()
        .route("/api/models/extract/ready", get(datasheet_ready_handler))
        .route("/api/models/extract", post(datasheet_extract_handler))
        .route("/api/models/save", post(datasheet_save_handler))
        .route("/api/models/check", post(datasheet_check_handler))
        .route("/api/models/draft", post(model_draft_handler))
        .route("/api/sensor-specs", get(sensor_catalog_handler))
        .layer(DefaultBodyLimit::max(MAX_DATASHEET_BYTES))
        .with_state(state)
}

/// Checked-in, product-bundled register behavior for the no-LLM browser path.
/// The response carries the exact same TOML bytes accepted by models lint,
/// live attachment and CI scenarios; choosing an item never writes a model.
///
/// The specs are embedded from THIS crate's `assets/sensor-specs/` mirror,
/// because `cargo package` ships only files under the crate directory. The
/// AUTHORITATIVE copies live at repo-root `testdata/sensor-specs/` (a dozen
/// engine tests load them by path); `tests/packaged_asset_sync.rs` fails when
/// the mirror drifts, and `scripts/sync-crate-assets.sh` refreshes it.
fn sensor_catalog_json() -> serde_json::Value {
    serde_json::json!({
        "schema_version": 1,
        "read_only": true,
        "llm_required": false,
        "entries": [
            {"id":"lm75","name":"LM75 temperature subset","bus":"i2c","scope":"temperature register at 0x48; no timing/OS/thermal dynamics","spec_toml":include_str!("../assets/sensor-specs/lm75.toml")},
            {"id":"bma423_chip_id","name":"BMA423 chip identity","bus":"i2c","scope":"CHIP_ID at 0x18 for SDO low; no acceleration/FIFO/interrupts","spec_toml":include_str!("../assets/sensor-specs/bma423_chip_id.toml")},
            {"id":"bme280","name":"BME280 environmental sensor","bus":"i2c","scope":"declarative environmental register subset","spec_toml":include_str!("../assets/sensor-specs/bme280.toml")},
            {"id":"mpu6050","name":"MPU6050 motion sensor","bus":"i2c","scope":"declarative motion register subset","spec_toml":include_str!("../assets/sensor-specs/mpu6050.toml")},
            {"id":"ads1115","name":"ADS1115 ADC","bus":"i2c","scope":"mux/PGA/config/conversion subset; conversion timing omitted","spec_toml":include_str!("../assets/sensor-specs/ads1115.toml")},
            {"id":"ina219","name":"INA219 current monitor","bus":"i2c","scope":"declarative shunt/bus register subset","spec_toml":include_str!("../assets/sensor-specs/ina219.toml")},
            {"id":"mcp4728","name":"MCP4728 quad DAC","bus":"i2c","scope":"declarative DAC register/output subset","spec_toml":include_str!("../assets/sensor-specs/mcp4728.toml")},
            {"id":"icm42605","name":"ICM-42605 motion sensor","bus":"spi","scope":"declarative SPI register subset","spec_toml":include_str!("../assets/sensor-specs/icm42605.toml")}
        ]
    })
}

async fn sensor_catalog_handler() -> Json<serde_json::Value> {
    Json(sensor_catalog_json())
}

/// POST `/api/models/draft`: prepare the selected component for local editing.
/// An executable partial model is copied into a board-narrowed extension;
/// an unresolved component gets the same conservative evidence-first scaffold
/// as `models prepare`. It is a local deterministic preview: no files are
/// written and no network or LLM is used.
async fn model_draft_handler(
    State(state): State<Arc<DatasheetState>>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    if let Some(resp) = reject_cross_site(&headers) {
        return resp;
    }
    let request = String::from_utf8_lossy(&body).into_owned();
    let draft = state.hooks.draft.clone();
    match tokio::task::spawn_blocking(move || (draft)(&request)).await {
        Ok(Ok(toml)) => json_body(
            StatusCode::OK,
            serde_json::json!({ "ok": true, "toml": toml, "read_only": true }),
        ),
        Ok(Err(message)) => json_error(&message),
        Err(_) => json_error("the model draft task panicked; see the server log"),
    }
}

/// GET `/api/models/extract/ready`: relay the engine's readiness JSON. The
/// probe asks codex for its own login state (a subprocess), so it runs on the
/// blocking pool.
async fn datasheet_ready_handler(State(state): State<Arc<DatasheetState>>) -> impl IntoResponse {
    let ready = state.hooks.ready.clone();
    let json = tokio::task::spawn_blocking(move || (ready)())
        .await
        .unwrap_or_else(|_| {
            "{\"ready\":false,\"reason\":\"the readiness probe panicked; see the server log\"}"
                .to_string()
        });
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        json,
    )
}

/// POST `/api/models/extract`: run one extraction and stream it as SSE, the
/// same framing the dependency installs use (`log` lines, then exactly one
/// `card` or `error`). Streaming rather than one long request because a codex
/// extraction runs for minutes: a silent request is indistinguishable from a
/// hang, and this one is spending the user's money while it looks dead.
///
/// The `card` payload is the model for review. Nothing has been written at that
/// point; `POST /api/models/save` is what keeps it.
async fn datasheet_extract_handler(
    State(state): State<Arc<DatasheetState>>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> axum::response::Response {
    if let Some(resp) = reject_cross_site(&headers) {
        return resp.into_response();
    }
    let parts = match parse_upload(&mut multipart).await {
        Ok(p) => p,
        Err(msg) => return sse_once("error", &msg),
    };
    let Some(pdf) = parts.datasheet_bytes else {
        return sse_once(
            "error",
            "no datasheet in the upload (expected a 'datasheet' part)",
        );
    };
    let (part, kind) = match extraction_identity(parts.part, parts.kind) {
        Ok(identity) => identity,
        Err(message) => return sse_once("error", message),
    };
    let job = DatasheetJob {
        pdf_name: parts.datasheet_name,
        pdf,
        reference: parts.reference.unwrap_or_default(),
        part,
        kind,
        model: parts.model.unwrap_or_default(),
    };

    let (tx, rx) = tokio::sync::mpsc::channel::<String>(256);
    let extract = state.hooks.extract.clone();
    tokio::task::spawn_blocking(move || {
        let result = {
            let tx = tx.clone();
            let mut sink = move |line: &str| {
                // A send failure means the browser went away. Unlike an
                // install, there is nothing worth finishing for: the card would
                // have nobody to review it. The extraction still runs to the
                // end because it has already been paid for and cannot be
                // recalled, we just stop relaying.
                // try_send, NOT blocking_send. A client that opens this POST
                // and then stops reading applies TCP backpressure, the channel
                // fills, and a blocking send parks this thread inside the very
                // loop that enforces the timeout: the child is never killed,
                // the RAII slot is never released, and every later install is
                // refused for the life of the process. Dropping a progress line
                // for a peer that is not listening costs nothing worth having.
                let _ = tx.try_send(sse_event("log", line));
            };
            (extract)(job, &mut sink)
        };
        let _ = match result {
            Ok(card_json) => tx.blocking_send(sse_event("card", &card_json)),
            Err(e) => tx.blocking_send(sse_event("error", &e)),
        };
    });
    sse_response(rx)
}

fn extraction_identity(
    part: Option<String>,
    kind: Option<String>,
) -> Result<(String, String), &'static str> {
    let part = part
        .filter(|value| !value.trim().is_empty())
        .ok_or("the extraction request needs a 'part' (the manufacturer part number)")?;
    // An absent or blank kind is intentional: the shared datasheet extractor
    // identifies it from the first pages. The browser labels the picker
    // optional, so rejecting its default value here made the primary path fail.
    Ok((part, kind.unwrap_or_default()))
}

#[cfg(test)]
mod datasheet_input_tests {
    use super::{
        extraction_identity, name_upload_limit_413, sensor_catalog_json, MAX_UPLOAD_BYTES,
    };
    use axum::{body::Body, http::StatusCode, response::Response};

    #[test]
    fn blank_kind_reaches_the_shared_identifier() {
        assert_eq!(
            extraction_identity(Some("TP4054".into()), Some(String::new())).unwrap(),
            ("TP4054".to_string(), String::new())
        );
    }

    #[test]
    fn missing_part_is_still_refused() {
        assert!(extraction_identity(None, Some("vreg".into())).is_err());
    }

    #[test]
    fn bundled_sensor_catalog_is_local_exact_and_contains_valid_toml() {
        let catalog = sensor_catalog_json();
        assert_eq!(catalog["read_only"], true);
        assert_eq!(catalog["llm_required"], false);
        let entries = catalog["entries"].as_array().unwrap();
        let ids = entries
            .iter()
            .map(|entry| entry["id"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            [
                "lm75",
                "bma423_chip_id",
                "bme280",
                "mpu6050",
                "ads1115",
                "ina219",
                "mcp4728",
                "icm42605"
            ]
        );
        for entry in entries {
            let spec = entry["spec_toml"].as_str().unwrap();
            assert!(spec.contains("[sensor]"), "{} lacks [sensor]", entry["id"]);
            assert!(
                spec.contains("[sensor.protocol]"),
                "{} lacks protocol",
                entry["id"]
            );
        }
    }

    #[tokio::test]
    async fn upload_limit_response_names_the_exact_limit() {
        let stock = Response::builder()
            .status(StatusCode::PAYLOAD_TOO_LARGE)
            .body(Body::from("length limit exceeded"))
            .unwrap();
        let response = name_upload_limit_413(stock).await;
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert_eq!(
            text,
            format!(
                "upload too large: this server accepts at most {} MB per request",
                MAX_UPLOAD_BYTES / (1024 * 1024)
            )
        );
    }
}

/// POST `/api/models/save`: write an accepted model card into the user's model
/// library. Body is JSON `{part, kind, toml}`; `toml` is the text the user saw
/// and accepted (they may have edited it), so the engine re-validates it before
/// writing rather than assuming it is still the extractor's own output.
async fn datasheet_save_handler(
    State(state): State<Arc<DatasheetState>>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    if let Some(resp) = reject_cross_site(&headers) {
        return resp;
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return json_error("the save request body is not JSON");
    };
    let field = |k: &str| {
        value
            .get(k)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let (part, kind, toml) = (field("part"), field("kind"), field("toml"));
    if part.trim().is_empty() || toml.trim().is_empty() {
        return json_error("the save request needs a non-empty 'part' and 'toml'");
    }
    let save = state.hooks.save.clone();
    match tokio::task::spawn_blocking(move || (save)(&part, &kind, &toml)).await {
        Ok(Ok(json)) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            json,
        ),
        Ok(Err(msg)) => json_error(&msg),
        Err(_) => json_error("the save task panicked; see the server log"),
    }
}

/// POST `/api/models/check`: validate a model without saving it.
///
/// The editor calls this while someone types. It runs the SAME checks the save
/// path runs, so a model cannot validate here and be refused there, which
/// would be worse than offering no editor at all.
async fn datasheet_check_handler(
    State(state): State<Arc<DatasheetState>>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    if let Some(resp) = reject_cross_site(&headers) {
        return resp;
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return json_error("the check request body is not JSON");
    };
    let toml = value
        .get("toml")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string();
    // Two formats through one endpoint, because the editor is one box with a
    // toggle and a second route would only duplicate the cross-site guard and
    // the blocking-pool handoff.
    let spice = value
        .get("format")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|f| f.eq_ignore_ascii_case("spice"));
    let check = if spice {
        state.hooks.spice_check.clone()
    } else {
        state.hooks.check.clone()
    };
    match tokio::task::spawn_blocking(move || (check)(&toml)).await {
        Ok(Ok(summary)) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            serde_json::json!({ "ok": true, "summary": summary }).to_string(),
        ),
        Ok(Err(msg)) => (
            // 200 with ok:false, not a 4xx. A model in progress is not a failed
            // request, and an editor that logs a console error on every
            // keystroke while someone types is unusable.
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            serde_json::json!({ "ok": false, "error": msg }).to_string(),
        ),
        Err(_) => json_error("the check task panicked; see the server log"),
    }
}

/// An SSE response carrying exactly one frame, for a request rejected before
/// any work started. The client reads this endpoint as a stream, so a refusal
/// has to arrive in the stream's own language or it shows up as a parse failure
/// instead of the reason.
fn sse_once(kind: &str, text: &str) -> axum::response::Response {
    axum::response::Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(axum::body::Body::from(sse_event(kind, text)))
        .expect("static headers build")
}

/// Wrap a worker's line channel as a `text/event-stream` body. Shared by the
/// dependency installs and datasheet extraction so the two streams cannot drift
/// in framing or headers (the compression layer exempts `text/event-stream`,
/// which is why lines reach the browser as they happen rather than sitting in a
/// gzip buffer).
fn sse_response(rx: tokio::sync::mpsc::Receiver<String>) -> axum::response::Response {
    use tokio_stream::StreamExt as _;
    let stream = tokio_stream::wrappers::ReceiverStream::new(rx)
        .map(|s| Ok::<Bytes, std::convert::Infallible>(Bytes::from(s)));
    axum::response::Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(axum::body::Body::from_stream(stream))
        .expect("static headers build")
}

/// A JSON body response with the standard content-type header. Every error is
/// built through `serde_json` so backslashes / control chars in a message can
/// never produce invalid JSON (B9).
fn json_body(
    status: StatusCode,
    value: serde_json::Value,
) -> (StatusCode, [(header::HeaderName, &'static str); 1], String) {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        value.to_string(),
    )
}

fn json_error(msg: &str) -> (StatusCode, [(header::HeaderName, &'static str); 1], String) {
    json_body(
        StatusCode::OK,
        serde_json::json!({ "ok": false, "error": msg }),
    )
}

/// The parts every upload endpoint accepts. `board`/`file` name the PCB (a
/// caller reaching for either should just work, since the browser form uses a
/// `file` input id while the raw path is conceptually "the board"), `firmware`
/// is optional and an empty part means "none selected", and `spec` carries the
/// checks TOML. Unknown parts are ignored so a future field cannot break an
/// older server.
#[derive(Default)]
struct UploadedParts {
    board_name: String,
    board_bytes: Option<Vec<u8>>,
    fw_name: String,
    fw_bytes: Option<Vec<u8>>,
    schematic_name: String,
    schematic_bytes: Option<Vec<u8>>,
    spec: Option<String>,
    /// The datasheet PDF and what to extract from it (`/api/models/extract`).
    datasheet_name: String,
    datasheet_bytes: Option<Vec<u8>>,
    part: Option<String>,
    kind: Option<String>,
    reference: Option<String>,
    model: Option<String>,
}

/// Drain a multipart body into [`UploadedParts`], or return the user-facing
/// reason it could not be read.
///
/// One parser for every upload endpoint. A copy per handler drifts, and a
/// drifted copy that builds its error JSON by hand emits invalid JSON the
/// moment a message carries a backslash or a control character.
/// Distinguishing `Ok(None)` (the clean end of the stream) from
/// `Err` (a truncated or malformed body) also matters: collapsing them reports a
/// corrupt upload as "no board file in the upload", which sends the user
/// looking in the wrong place.
async fn parse_upload(multipart: &mut Multipart) -> Result<UploadedParts, String> {
    let mut parts = UploadedParts {
        board_name: "board".to_string(),
        datasheet_name: "datasheet.pdf".to_string(),
        ..Default::default()
    };
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => return Err(format!("malformed multipart upload: {e}")),
        };
        let name = field.name().unwrap_or("").to_string();
        let filename = field.file_name().map(|s| s.to_string());
        let data = match field.bytes().await {
            Ok(b) => b,
            Err(e) => return Err(format!("failed to read upload part: {e}")),
        };
        match name.as_str() {
            "board" | "file" => {
                if let Some(f) = filename {
                    parts.board_name = f;
                }
                parts.board_bytes = Some(data.to_vec());
            }
            "firmware" => {
                if let Some(f) = filename {
                    parts.fw_name = f;
                }
                if !data.is_empty() {
                    parts.fw_bytes = Some(data.to_vec());
                }
            }
            "schematic" => {
                if let Some(f) = filename {
                    parts.schematic_name = f;
                }
                if !data.is_empty() {
                    parts.schematic_bytes = Some(data.to_vec());
                }
            }
            "spec" => parts.spec = Some(String::from_utf8_lossy(&data).into_owned()),
            "datasheet" => {
                if let Some(f) = filename {
                    parts.datasheet_name = f;
                }
                if !data.is_empty() {
                    parts.datasheet_bytes = Some(data.to_vec());
                }
            }
            // The extraction's three text fields. Trimmed here because a
            // browser form happily posts a trailing newline and a part number
            // with one on the end matches nothing.
            "part" => parts.part = Some(String::from_utf8_lossy(&data).trim().to_string()),
            "kind" => parts.kind = Some(String::from_utf8_lossy(&data).trim().to_string()),
            "model" => parts.model = Some(String::from_utf8_lossy(&data).trim().to_string()),
            "reference" => {
                parts.reference = Some(String::from_utf8_lossy(&data).trim().to_string())
            }
            _ => {}
        }
    }
    Ok(parts)
}

async fn check_handler(
    State(state): State<Arc<CheckState>>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> impl IntoResponse {
    if let Some(resp) = reject_cross_site(&headers) {
        return resp;
    }
    let parts = match parse_upload(&mut multipart).await {
        Ok(p) => p,
        Err(msg) => return json_error(&msg),
    };
    let (Some(board_bytes), Some(spec)) = (parts.board_bytes, parts.spec) else {
        return json_error("the check request needs a 'board' part and a 'spec' part");
    };
    let (board_name, fw_name, fw_bytes) = (parts.board_name, parts.fw_name, parts.fw_bytes);
    let (schematic_name, schematic_bytes) = (parts.schematic_name, parts.schematic_bytes);

    // The runner returns a ready JSON string (its own {ok:...} shape); relay
    // it verbatim with the content-type header. It BLOCKS for the whole child
    // co-sim (up to the runner's own multi-minute timeout), so it must run on
    // the blocking pool: called inline it would pin an async worker thread per
    // active check, and a handful of concurrent checks could pin every worker
    // and stall all the other routes (the same reason datasheet_check_handler
    // uses spawn_blocking).
    let check = match &state.check {
        CheckCallback::Legacy(check) => CheckCallback::Legacy(check.clone()),
        CheckCallback::Schematic(check) => CheckCallback::Schematic(check.clone()),
    };
    let json = match tokio::task::spawn_blocking(move || {
        let firmware = fw_bytes.as_deref().map(|bytes| (fw_name.as_str(), bytes));
        let schematic = schematic_bytes
            .as_deref()
            .map(|bytes| (schematic_name.as_str(), bytes));
        match check {
            CheckCallback::Legacy(check) => check(&board_name, &board_bytes, firmware, &spec),
            CheckCallback::Schematic(check) => {
                check(&board_name, &board_bytes, firmware, schematic, &spec)
            }
        }
    })
    .await
    {
        Ok(json) => json,
        Err(_) => return json_error("the check task panicked; see the server log"),
    };
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        json,
    )
}

/// Accept the raw board file as the request body, with the original filename in
/// the `X-Board-Filename` header (the page sets it). Returns the analysis JSON.
async fn analyze_handler(
    State(state): State<Arc<FrontDoorState>>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    if let Some(resp) = reject_cross_site(&headers) {
        return resp;
    }
    let file_name = headers
        .get("x-board-filename")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("board")
        .to_string();

    // Board files may be text (KiCad/Eagle/IPC), a zip (gerbers) or a binary
    // container (Altium .PcbDoc). The analyzer's extractor sniffs the format
    // from the RAW bytes; decoding here to a lossy-UTF8 string would corrupt the
    // binary formats before they are ever parsed.
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
    if let Some(resp) = reject_cross_site(&headers) {
        return resp;
    }
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
/// `firmware` part (optional). Both parts are passed as raw `&[u8]`, NEVER
/// lossy-decoded, which would corrupt an ELF or a binary board (Altium
/// .PcbDoc); the analyzer's extractor sniffs binary-vs-text itself. Falls back
/// to a board-only analysis when no firmware part is present.
async fn analyze_firmware_handler(
    State(state): State<Arc<FirmwareState>>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> impl IntoResponse {
    if let Some(resp) = reject_cross_site(&headers) {
        return resp;
    }
    let parts = match parse_upload(&mut multipart).await {
        Ok(p) => p,
        Err(msg) => return json_error(&msg),
    };
    let (board_name, fw_name, fw_bytes) = (parts.board_name, parts.fw_name, parts.fw_bytes);
    let board_bytes = match parts.board_bytes {
        Some(b) => b,
        None => {
            return json_error("no board file in the upload (expected a 'board' or 'file' part)")
        }
    };

    // Firmware is optional: an absent (or empty) part falls back to a
    // board-only analysis, which is the same contract /api/analyze offers.
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

async fn analyze_handler_schematic(
    State(state): State<Arc<SchematicState>>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    if let Some(resp) = reject_cross_site(&headers) {
        return resp;
    }
    let file_name = headers
        .get("x-board-filename")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("board");
    let json = (state.analyze)(file_name, &body, None, None);
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        json,
    )
}

async fn analyze_schematic_handler(
    State(state): State<Arc<SchematicState>>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> impl IntoResponse {
    if let Some(resp) = reject_cross_site(&headers) {
        return resp;
    }
    let parts = match parse_upload(&mut multipart).await {
        Ok(parts) => parts,
        Err(message) => return json_error(&message),
    };
    let Some(board_bytes) = parts.board_bytes else {
        return json_error("no board file in the upload (expected a 'board' or 'file' part)");
    };
    let firmware = parts
        .fw_bytes
        .as_deref()
        .map(|bytes| (parts.fw_name.as_str(), bytes));
    let schematic = parts
        .schematic_bytes
        .as_deref()
        .map(|bytes| (parts.schematic_name.as_str(), bytes));
    let json = (state.analyze)(&parts.board_name, &board_bytes, firmware, schematic);
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        json,
    )
}
