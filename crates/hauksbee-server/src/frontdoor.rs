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
//! callback (`Analyzer` / `FirmwareAnalyzer` / `SchematicAnalyzer`) so the server
//! crate stays free of any dependency on the engine/extract crates (which depend
//! on *this* crate); the `hauksbee` binary wires the engine's `analyze_json` in.
//! Every route has ONE handler, the schematic-aware one; the narrower legacy
//! callback shapes are adapted onto it at mount time so the two can never drift.
//! The routes are merged into the unified server router (see [`crate::Server`])
//! so `serve` and `run --serve` both expose them alongside the WebSocket sim and
//! the static React bundle.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Multipart, Path as UrlPath, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};

pub use hauksbee_frontdoor_api::frontdoor::{
    Analyzer, CheckRunner, DatasheetChecker, DatasheetExtractor, DatasheetHooks, DatasheetJob,
    DatasheetReady, DatasheetSaver, DepInstaller, DepsStatus, DesignAnalyzer, DesignCheckRunner,
    DesignLiveLauncher, DesignUpload, FirmwareAnalyzer, LiveLaunch, LiveLauncher, ModelDrafter,
    NamedUpload, SchematicAnalyzer, SchematicCheckRunner, SchematicLiveLauncher, ToolHooks,
};

/// A JSON (or plain-text) body with its content-type header: the shape every
/// non-streaming handler answers with.
type JsonResponse = (StatusCode, [(header::HeaderName, &'static str); 1], String);

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
fn reject_cross_site(headers: &HeaderMap) -> Option<JsonResponse> {
    let site = headers.get("sec-fetch-site")?.to_str().ok()?;
    // Browser-origin request: only our own page (same-origin) or a direct
    // address-bar navigation (none) may reach these endpoints.
    (site != "same-origin" && site != "none").then(|| {
        (
            StatusCode::FORBIDDEN,
            [(header::CONTENT_TYPE, "application/json")],
            "{\"ok\":false,\"error\":\"cross-site request refused: the hauksbee analysis \
             endpoints accept requests only from the hauksbee page itself\"}"
                .to_string(),
        )
    })
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
async fn name_upload_limit_413(resp: Response) -> Response {
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

/// Bind a multipart router's state behind the shared upload guard: one body
/// size limit and one 413 namer, so every upload endpoint answers an over-size
/// request the same way.
fn with_upload_guard<S: Send + Sync + 'static>(router: Router<Arc<S>>, state: S) -> Router {
    router
        .layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES))
        .layer(axum::middleware::map_response(name_upload_limit_413))
        .with_state(Arc::new(state))
}

/// Build the board-only analysis routes (`/api/analyze`). No server-rendered
/// page: the React bundle owns `/`. Kept for tests and any board-only caller;
/// production wires the firmware-aware [`api_routes`] into the unified server.
pub fn router(analyze: Analyzer) -> Router {
    api_routes_with_schematic(Arc::new(move |name, board, _, _| analyze(name, board)))
}

struct DesignState {
    analyze: DesignAnalyzer,
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
    api_routes_with_schematic(with_schematic_analyzer(analyze))
}

/// Complete browser analysis contract. Supplemental design/manufacturing files
/// use the same multipart endpoint as firmware and schematic companions.
pub fn api_routes_with_design(analyze: DesignAnalyzer) -> Router {
    let state = Arc::new(DesignState { analyze });
    Router::new()
        .route("/api/analyze", post(analyze_handler_design_raw))
        .route("/api/analyze-with-firmware", post(analyze_design_handler))
        .layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES))
        .layer(axum::middleware::map_response(name_upload_limit_413))
        .with_state(state)
}

/// Back-compat alias for [`api_routes`] (the firmware-aware analysis routes).
/// No server-rendered page: the React bundle owns `/`.
pub fn router_with_firmware(analyze: FirmwareAnalyzer) -> Router {
    api_routes(analyze)
}

/// A [`FirmwareAnalyzer`] as the schematic-aware shape, ignoring the schematic.
pub(crate) fn with_schematic_analyzer(analyze: FirmwareAnalyzer) -> SchematicAnalyzer {
    Arc::new(move |name, board, firmware, _| analyze(name, board, firmware))
}

struct CheckState {
    check: CheckCallback,
}

/// Which shape of check callback a route was mounted with. The narrowest
/// (`CheckRunner`) is adapted onto the schematic shape at mount time, so there
/// are only two here and one handler behind all three constructors.
#[derive(Clone)]
enum CheckCallback {
    Schematic(SchematicCheckRunner),
    Design(DesignCheckRunner),
}

/// A [`CheckRunner`] as the schematic-aware shape, ignoring the schematic.
pub(crate) fn with_schematic_check(check: CheckRunner) -> SchematicCheckRunner {
    Arc::new(move |name, board, firmware, _, spec| check(name, board, firmware, spec))
}

/// A [`LiveLauncher`] as the schematic-aware shape, ignoring the schematic.
pub(crate) fn with_schematic_launcher(launch: LiveLauncher) -> SchematicLiveLauncher {
    Arc::new(move |name, board, firmware, _| launch(name, board, firmware))
}

/// Analysis routes which additionally accept a `schematic` multipart part.
/// The raw board endpoint remains available and supplies no companions.
pub fn api_routes_with_schematic(analyze: SchematicAnalyzer) -> Router {
    with_upload_guard(
        Router::new()
            .route("/api/analyze", post(analyze_handler))
            .route("/api/analyze-with-firmware", post(analyze_upload_handler)),
        analyze,
    )
}

/// The web checks route (`POST /api/check`, multipart: `board` + optional
/// `firmware` + `spec`). The spec part is the TOML body the browser's builder
/// composed, everything except the file paths, which the engine injects from
/// the uploaded parts. Merged into the unified router next to the analysis
/// routes.
pub fn check_route(check: CheckRunner) -> Router {
    check_route_with_schematic(with_schematic_check(check))
}

/// Schematic-aware checks route used by the shipped standalone app.
pub fn check_route_with_schematic(check: SchematicCheckRunner) -> Router {
    check_route_for(CheckCallback::Schematic(check))
}

/// Checks route which takes the whole design upload (board plus every
/// companion the desktop app posts) rather than the board and its firmware.
pub fn check_route_with_design(check: DesignCheckRunner) -> Router {
    check_route_for(CheckCallback::Design(check))
}

fn check_route_for(check: CheckCallback) -> Router {
    with_upload_guard(
        Router::new().route("/api/check", post(check_handler)),
        CheckState { check },
    )
}

struct LiveState {
    hub: Arc<crate::LiveHub>,
    launch: SchematicLiveLauncher,
}

struct DesignLiveState {
    hub: Arc<crate::LiveHub>,
    launch: DesignLiveLauncher,
}

/// The live-launch API: `POST /api/live/launch` (multipart `board` + optional
/// `firmware`, same parts as `/api/analyze-with-firmware`) boots a live sim
/// session for the upload and swaps it into the hub behind `/ws`;
/// `GET /api/live/status` reports whether (and which) a session is running so
/// the UI can confirm before replacing it. The launch runs an uploaded board
/// (and firmware) through the engine, so it carries the same
/// `reject_cross_site` guard as the other mutating endpoints.
pub fn live_routes(hub: Arc<crate::LiveHub>, launch: LiveLauncher) -> Router {
    live_routes_with_schematic(hub, with_schematic_launcher(launch))
}

/// Live-launch routes which additionally accept a `schematic` multipart part.
pub fn live_routes_with_schematic(
    hub: Arc<crate::LiveHub>,
    launch: SchematicLiveLauncher,
) -> Router {
    with_upload_guard(
        Router::new()
            .route("/api/live/launch", post(live_launch_handler))
            .route("/api/live/status", get(live_status_handler)),
        LiveState { hub, launch },
    )
}

/// `GET /api/live/status`: whether a live session is running, and on which
/// board, so the UI can confirm before replacing it.
async fn live_status_handler(State(state): State<Arc<LiveState>>) -> JsonResponse {
    live_status(&state.hub)
}

/// Live-launch routes for the full design upload: the board plus every
/// companion the desktop app can post (firmware, schematic, netlist).
pub fn live_routes_with_design(hub: Arc<crate::LiveHub>, launch: DesignLiveLauncher) -> Router {
    with_upload_guard(
        Router::new()
            .route("/api/live/launch", post(live_launch_design_handler))
            .route("/api/live/status", get(live_status_design_handler)),
        DesignLiveState { hub, launch },
    )
}

async fn live_status_design_handler(State(state): State<Arc<DesignLiveState>>) -> JsonResponse {
    live_status(&state.hub)
}

async fn live_launch_design_handler(
    State(state): State<Arc<DesignLiveState>>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> JsonResponse {
    let parts = match guarded_upload(&headers, &mut multipart).await {
        Ok(parts) => parts,
        Err(response) => return response,
    };
    let upload = match parts.into_design_upload() {
        Ok(upload) => upload,
        Err(message) => return json_error(message),
    };
    let launch = state.launch.clone();
    match run_blocking("the live launch task", move || (launch)(upload)).await {
        Ok(live) => installed_live(&state.hub, live),
        Err(response) => response,
    }
}

/// Whether a live session is running, and on which board.
fn live_status(hub: &crate::LiveHub) -> JsonResponse {
    json_body(
        StatusCode::OK,
        match hub.active_board() {
            Some(name) => serde_json::json!({ "active": true, "board_name": name }),
            None => serde_json::json!({ "active": false }),
        },
    )
}

/// Install a freshly built session as THE live session, reporting whether it
/// displaced one that was already running.
fn installed_live(hub: &crate::LiveHub, live: LiveLaunch) -> JsonResponse {
    let board_name = live.board_name.clone();
    let replaced = hub.launch(
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
) -> JsonResponse {
    let mut parts = match guarded_upload(&headers, &mut multipart).await {
        Ok(parts) => parts,
        Err(response) => return response,
    };
    let Some(board_bytes) = parts.board_bytes.take() else {
        return json_error(NO_BOARD_PART);
    };
    let launch = state.launch.clone();
    let built = run_blocking("the live launch task", move || {
        (launch)(
            &parts.board_name,
            &board_bytes,
            parts.firmware(),
            parts.schematic(),
        )
    })
    .await;
    match built {
        Ok(live) => installed_live(&state.hub, live),
        Err(response) => response,
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
    Router::new()
        .route("/api/deps", get(deps_status_handler))
        .route("/api/deps/install/{id}", post(deps_install_handler))
        .with_state(Arc::new(DepsState { status, install }))
}

/// GET `/api/deps`: relay the engine's dependency JSON. Discovery is bounded
/// and cached by the engine; it still runs on the blocking pool so a resolver
/// cannot stall the async runtime. Authentication/version checks are lazy and
/// belong to the operation that needs them.
async fn deps_status_handler(State(state): State<Arc<DepsState>>) -> JsonResponse {
    let status = state.status.clone();
    json_ok(
        tokio::task::spawn_blocking(move || (status)())
            .await
            .unwrap_or_else(|_| "{\"deps\":[]}".to_string()),
    )
}

/// One SSE frame: `event: <kind>` with the text as (possibly multi-line) data.
fn sse_event(kind: &str, text: &str) -> String {
    let mut s = format!("event: {kind}\n");
    let mut lines = text.lines().peekable();
    if lines.peek().is_none() {
        s.push_str("data:\n");
    }
    for line in lines {
        s.push_str(&format!("data: {line}\n"));
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
) -> Response {
    if let Some(resp) = reject_cross_site(&headers) {
        return resp.into_response();
    }
    let install = state.install.clone();
    stream_job("done", move |sink| {
        (install)(&id, sink).map(|()| "ok".to_string())
    })
}

/// Largest datasheet accepted (32 MiB). A datasheet is tens of pages of vector
/// art; the largest real ones (a 600-page MCU reference manual) are under 30 MiB.
/// The board limit would be absurd here, and the extraction copies the file and
/// renders its pages, so an enormous upload costs disk and CPU before anything
/// has been checked.
const MAX_DATASHEET_BYTES: usize = 32 * 1024 * 1024;

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
    Router::new()
        .route("/api/models/extract/ready", get(datasheet_ready_handler))
        .route("/api/models/extract", post(datasheet_extract_handler))
        .route("/api/models/save", post(datasheet_save_handler))
        .route("/api/models/check", post(datasheet_check_handler))
        .route("/api/models/draft", post(model_draft_handler))
        .route("/api/sensor-specs", get(sensor_catalog_handler))
        .layer(DefaultBodyLimit::max(MAX_DATASHEET_BYTES))
        .with_state(Arc::new(hooks))
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

/// The string under `key` in a JSON request body, empty when absent.
fn json_str_field(value: &serde_json::Value, key: &str) -> String {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// POST `/api/models/draft`: prepare the selected component for local editing.
/// An executable partial model is copied into a board-narrowed extension;
/// an unresolved component gets the same conservative evidence-first scaffold
/// as `models prepare`. It is a local deterministic preview: no files are
/// written and no network or LLM is used.
async fn model_draft_handler(
    State(hooks): State<Arc<DatasheetHooks>>,
    headers: HeaderMap,
    body: Bytes,
) -> JsonResponse {
    if let Some(resp) = reject_cross_site(&headers) {
        return resp;
    }
    let request = String::from_utf8_lossy(&body).into_owned();
    let draft = hooks.draft.clone();
    match run_blocking("the model draft task", move || (draft)(&request)).await {
        Ok(toml) => json_body(
            StatusCode::OK,
            serde_json::json!({ "ok": true, "toml": toml, "read_only": true }),
        ),
        Err(response) => response,
    }
}

/// GET `/api/models/extract/ready`: relay the engine's readiness JSON. The
/// probe asks codex for its own login state (a subprocess), so it runs on the
/// blocking pool.
async fn datasheet_ready_handler(State(hooks): State<Arc<DatasheetHooks>>) -> JsonResponse {
    let ready = hooks.ready.clone();
    json_ok(
        tokio::task::spawn_blocking(move || (ready)())
            .await
            .unwrap_or_else(|_| {
                "{\"ready\":false,\"reason\":\"the readiness probe panicked; see the server log\"}"
                    .to_string()
            }),
    )
}

/// POST `/api/models/extract`: run one extraction and stream it as SSE, the
/// same framing the dependency installs use (`log` lines, then exactly one
/// `card` or `error`). Streaming rather than one long request because a codex
/// extraction runs for minutes: a silent request is indistinguishable from a
/// hang, and this one is spending the user's money while it looks dead.
///
/// The `card` payload is the model for review. Nothing has been written at that
/// point; `POST /api/models/save` is what keeps it. Unlike an install, there is
/// nothing worth finishing for a browser that went away (the card would have
/// nobody to review it), but the extraction still runs to the end because it
/// has already been paid for and cannot be recalled.
async fn datasheet_extract_handler(
    State(hooks): State<Arc<DatasheetHooks>>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Response {
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
    // An absent or blank kind is intentional: the shared datasheet extractor
    // identifies it from the first pages. The browser labels the picker
    // optional, so rejecting its default value here made the primary path fail.
    let Some(part) = parts.part.filter(|value| !value.trim().is_empty()) else {
        return sse_once(
            "error",
            "the extraction request needs a 'part' (the manufacturer part number)",
        );
    };
    let job = DatasheetJob {
        pdf_name: parts.datasheet_name,
        pdf,
        reference: parts.reference.unwrap_or_default(),
        part,
        kind: parts.kind.unwrap_or_default(),
        model: parts.model.unwrap_or_default(),
    };
    let extract = hooks.extract.clone();
    stream_job("card", move |sink| (extract)(job, sink))
}

#[cfg(test)]
mod datasheet_input_tests {
    use super::{name_upload_limit_413, sensor_catalog_json, sse_event, MAX_UPLOAD_BYTES};
    use axum::{body::Body, http::StatusCode, response::Response};

    #[test]
    fn bundled_sensor_catalog_is_local_exact_and_contains_valid_toml() {
        let catalog = sensor_catalog_json();
        assert_eq!(catalog["read_only"], true);
        assert_eq!(catalog["llm_required"], false);
        let entries = catalog["entries"].as_array().unwrap();
        assert!(entries.iter().any(|e| e["id"] == "lm75"), "{catalog}");
        for entry in entries {
            let spec = entry["spec_toml"].as_str().unwrap();
            assert!(
                spec.contains("[sensor]") && spec.contains("[sensor.protocol]"),
                "{} lacks a sensor/protocol table",
                entry["id"]
            );
        }
    }

    #[test]
    fn sse_frames_carry_every_line_and_an_empty_data_field_for_no_text() {
        assert_eq!(sse_event("log", "a\nb"), "event: log\ndata: a\ndata: b\n\n");
        assert_eq!(sse_event("done", ""), "event: done\ndata:\n\n");
    }

    #[tokio::test]
    async fn upload_limit_response_keeps_the_status_and_names_the_limit() {
        let stock = Response::builder()
            .status(StatusCode::PAYLOAD_TOO_LARGE)
            .body(Body::from("length limit exceeded"))
            .unwrap();
        let response = name_upload_limit_413(stock).await;
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        let mb = (MAX_UPLOAD_BYTES / (1024 * 1024)).to_string();
        assert!(String::from_utf8(body.to_vec()).unwrap().contains(&mb));
    }
}

/// POST `/api/models/save`: write an accepted model card into the user's model
/// library. Body is JSON `{part, kind, toml}`; `toml` is the text the user saw
/// and accepted (they may have edited it), so the engine re-validates it before
/// writing rather than assuming it is still the extractor's own output.
async fn datasheet_save_handler(
    State(hooks): State<Arc<DatasheetHooks>>,
    headers: HeaderMap,
    body: Bytes,
) -> JsonResponse {
    if let Some(resp) = reject_cross_site(&headers) {
        return resp;
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return json_error("the save request body is not JSON");
    };
    let (part, kind, toml) = (
        json_str_field(&value, "part"),
        json_str_field(&value, "kind"),
        json_str_field(&value, "toml"),
    );
    if part.trim().is_empty() || toml.trim().is_empty() {
        return json_error("the save request needs a non-empty 'part' and 'toml'");
    }
    let save = hooks.save.clone();
    match run_blocking("the save task", move || (save)(&part, &kind, &toml)).await {
        Ok(json) => json_ok(json),
        Err(response) => response,
    }
}

/// POST `/api/models/check`: validate a model without saving it.
///
/// The editor calls this while someone types. It runs the SAME checks the save
/// path runs, so a model cannot validate here and be refused there, which
/// would be worse than offering no editor at all. Two formats go through one
/// endpoint, because the editor is one box with a toggle and a second route
/// would only duplicate the cross-site guard and the blocking-pool handoff.
async fn datasheet_check_handler(
    State(hooks): State<Arc<DatasheetHooks>>,
    headers: HeaderMap,
    body: Bytes,
) -> JsonResponse {
    if let Some(resp) = reject_cross_site(&headers) {
        return resp;
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return json_error("the check request body is not JSON");
    };
    let toml = json_str_field(&value, "toml");
    let spice = json_str_field(&value, "format").eq_ignore_ascii_case("spice");
    let check = if spice {
        hooks.spice_check.clone()
    } else {
        hooks.check.clone()
    };
    match tokio::task::spawn_blocking(move || (check)(&toml)).await {
        Ok(Ok(summary)) => json_body(
            StatusCode::OK,
            serde_json::json!({ "ok": true, "summary": summary }),
        ),
        // 200 with ok:false, not a 4xx. A model in progress is not a failed
        // request, and an editor that logs a console error on every
        // keystroke while someone types is unusable.
        Ok(Err(msg)) => json_error(&msg),
        Err(_) => json_error("the check task panicked; see the server log"),
    }
}

/// Run a blocking engine callback on the blocking pool (extract + bind, a
/// child co-sim, a subprocess probe: none of it may sit on an async worker).
/// The callback's own refusal and a panic both come back as the `{ok:false,
/// error}` answer, so the UI shows a reason instead of a dead spinner.
async fn run_blocking<T: Send + 'static>(
    what: &'static str,
    f: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, JsonResponse> {
    match tokio::task::spawn_blocking(f).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(message)) => Err(json_error(&message)),
        Err(_) => Err(json_error(&format!("{what} panicked; see the server log"))),
    }
}

/// Run a long job on the blocking pool and stream its progress lines as SSE
/// `log` events, ending with exactly one `<done_kind>` frame carrying the
/// job's result, or one `error` frame carrying its message.
///
/// Progress goes through `try_send`, NOT `blocking_send`. A client that opens
/// the POST and then stops reading applies TCP backpressure, the channel fills,
/// and a blocking send would park this thread inside the very loop that
/// enforces the job's timeout: the child is never killed, the RAII slot is
/// never released, and every later job is refused for the life of the process.
/// Dropping a progress line for a peer that is not listening costs nothing
/// worth having; the job itself runs to completion regardless.
fn stream_job(
    done_kind: &'static str,
    job: impl FnOnce(&mut dyn FnMut(&str)) -> Result<String, String> + Send + 'static,
) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<String>(256);
    tokio::task::spawn_blocking(move || {
        let result = {
            let tx = tx.clone();
            let mut sink = move |line: &str| {
                let _ = tx.try_send(sse_event("log", line));
            };
            job(&mut sink)
        };
        let _ = match result {
            Ok(text) => tx.blocking_send(sse_event(done_kind, &text)),
            Err(e) => tx.blocking_send(sse_event("error", &e)),
        };
    });
    sse_response(rx)
}

/// An SSE response carrying exactly one frame, for a request rejected before
/// any work started. The client reads this endpoint as a stream, so a refusal
/// has to arrive in the stream's own language or it shows up as a parse failure
/// instead of the reason.
fn sse_once(kind: &str, text: &str) -> Response {
    sse_body(axum::body::Body::from(sse_event(kind, text)))
}

/// Wrap a worker's line channel as a `text/event-stream` body. Shared by the
/// dependency installs and datasheet extraction so the two streams cannot drift
/// in framing or headers (the compression layer exempts `text/event-stream`,
/// which is why lines reach the browser as they happen rather than sitting in a
/// gzip buffer).
fn sse_response(rx: tokio::sync::mpsc::Receiver<String>) -> Response {
    use tokio_stream::StreamExt as _;
    let stream = tokio_stream::wrappers::ReceiverStream::new(rx)
        .map(|s| Ok::<Bytes, std::convert::Infallible>(Bytes::from(s)));
    sse_body(axum::body::Body::from_stream(stream))
}

fn sse_body(body: axum::body::Body) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(body)
        .expect("static headers build")
}

/// A `200 OK` carrying an already-serialized JSON document.
fn json_ok(json: String) -> JsonResponse {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        json,
    )
}

/// The board's original filename, which the page sends in `X-Board-Filename`.
/// Falls back to `"board"`: the analyzer sniffs the format from the bytes, so a
/// missing header costs only the name in the report.
fn board_filename(headers: &HeaderMap) -> &str {
    headers
        .get("x-board-filename")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("board")
}

/// A JSON body response with the standard content-type header. Every error is
/// built through `serde_json` so backslashes / control chars in a message can
/// never produce invalid JSON (B9).
fn json_body(status: StatusCode, value: serde_json::Value) -> JsonResponse {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        value.to_string(),
    )
}

fn json_error(msg: &str) -> JsonResponse {
    json_body(
        StatusCode::OK,
        serde_json::json!({ "ok": false, "error": msg }),
    )
}

const NO_BOARD_PART: &str = "no board file in the upload (expected a 'board' or 'file' part)";
const NEEDS_BOARD_AND_SPEC: &str = "the check request needs a 'board' part and a 'spec' part";

/// The parts every upload endpoint accepts. `board`/`file` name the PCB (a
/// caller reaching for either should just work, since the browser form uses a
/// `file` input id while the raw path is conceptually "the board"), `firmware`
/// and `schematic` are optional and an empty part means "none selected", and
/// `spec` carries the checks TOML. Unknown parts are ignored so a future field
/// cannot break an older server.
#[derive(Default)]
struct UploadedParts {
    board_name: String,
    board_bytes: Option<Vec<u8>>,
    fw_name: String,
    fw_bytes: Option<Vec<u8>>,
    schematic_name: String,
    schematic_bytes: Option<Vec<u8>>,
    bom_name: String,
    bom_bytes: Option<Vec<u8>>,
    placement_name: String,
    placement_bytes: Option<Vec<u8>>,
    variant_name: String,
    variant_bytes: Option<Vec<u8>>,
    asbuilt_name: String,
    asbuilt_bytes: Option<Vec<u8>>,
    models: Vec<NamedUpload>,
    spec: Option<String>,
    /// The datasheet PDF and what to extract from it (`/api/models/extract`).
    datasheet_name: String,
    datasheet_bytes: Option<Vec<u8>>,
    part: Option<String>,
    kind: Option<String>,
    reference: Option<String>,
    model: Option<String>,
}

impl UploadedParts {
    /// The optional firmware part as the `(name, bytes)` the callbacks take.
    fn firmware(&self) -> Option<(&str, &[u8])> {
        self.fw_bytes
            .as_deref()
            .map(|bytes| (self.fw_name.as_str(), bytes))
    }

    /// The optional schematic part as the `(name, bytes)` the callbacks take.
    fn schematic(&self) -> Option<(&str, &[u8])> {
        self.schematic_bytes
            .as_deref()
            .map(|bytes| (self.schematic_name.as_str(), bytes))
    }

    /// Every companion the upload carried, as the one bundle the design-aware
    /// callbacks take.
    fn into_design_upload(self) -> Result<DesignUpload, &'static str> {
        let Some(board_bytes) = self.board_bytes else {
            return Err(NO_BOARD_PART);
        };
        let named =
            |name: String, bytes: Option<Vec<u8>>| bytes.map(|bytes| NamedUpload { name, bytes });
        Ok(DesignUpload {
            board: NamedUpload {
                name: self.board_name,
                bytes: board_bytes,
            },
            firmware: named(self.fw_name, self.fw_bytes),
            schematic: named(self.schematic_name, self.schematic_bytes),
            bom: named(self.bom_name, self.bom_bytes),
            placement: named(self.placement_name, self.placement_bytes),
            variant: named(self.variant_name, self.variant_bytes),
            asbuilt: named(self.asbuilt_name, self.asbuilt_bytes),
            models: self.models,
        })
    }
}

/// The cross-site guard and the multipart parse every JSON upload endpoint
/// starts with, with the refusal already in response form.
async fn guarded_upload(
    headers: &HeaderMap,
    multipart: &mut Multipart,
) -> Result<UploadedParts, JsonResponse> {
    if let Some(resp) = reject_cross_site(headers) {
        return Err(resp);
    }
    parse_upload(multipart)
        .await
        .map_err(|msg| json_error(&msg))
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
        bom_name: "bom.csv".to_string(),
        placement_name: "placement.csv".to_string(),
        variant_name: "variant.toml".to_string(),
        asbuilt_name: "asbuilt.toml".to_string(),
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
        // A file part: its name overrides the default, and an EMPTY body is
        // "none selected" for the optional ones.
        let (slot_name, slot_bytes, required) = match name.as_str() {
            "board" | "file" => (&mut parts.board_name, &mut parts.board_bytes, true),
            "firmware" => (&mut parts.fw_name, &mut parts.fw_bytes, false),
            "schematic" => (&mut parts.schematic_name, &mut parts.schematic_bytes, false),
            "datasheet" => (&mut parts.datasheet_name, &mut parts.datasheet_bytes, false),
            "bom" => (&mut parts.bom_name, &mut parts.bom_bytes, false),
            "placement" => (&mut parts.placement_name, &mut parts.placement_bytes, false),
            "variant" => (&mut parts.variant_name, &mut parts.variant_bytes, false),
            "asbuilt" => (&mut parts.asbuilt_name, &mut parts.asbuilt_bytes, false),
            // The only repeatable part: models accumulate instead of replacing
            // one another, so it carries no slot.
            "model_file" => {
                if !data.is_empty() {
                    parts.models.push(NamedUpload {
                        name: filename.unwrap_or_else(|| "model.toml".to_string()),
                        bytes: data.to_vec(),
                    });
                }
                continue;
            }
            // The text fields. Trimmed because a browser form happily posts a
            // trailing newline and a part number with one on the end matches
            // nothing; the spec TOML keeps its bytes.
            "spec" => {
                parts.spec = Some(String::from_utf8_lossy(&data).into_owned());
                continue;
            }
            "part" | "kind" | "model" | "reference" => {
                let text = Some(String::from_utf8_lossy(&data).trim().to_string());
                match name.as_str() {
                    "part" => parts.part = text,
                    "kind" => parts.kind = text,
                    "model" => parts.model = text,
                    _ => parts.reference = text,
                }
                continue;
            }
            _ => continue,
        };
        if let Some(f) = filename {
            *slot_name = f;
        }
        if required || !data.is_empty() {
            *slot_bytes = Some(data.to_vec());
        }
    }
    Ok(parts)
}

/// POST `/api/check`: relay the runner's ready JSON string (its own `{ok:...}`
/// shape) verbatim. It BLOCKS for the whole child co-sim (up to the runner's
/// own multi-minute timeout), so it runs on the blocking pool: called inline
/// it would pin an async worker thread per active check, and a handful of
/// concurrent checks could pin every worker and stall all the other routes.
async fn check_handler(
    State(state): State<Arc<CheckState>>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> JsonResponse {
    let mut parts = match guarded_upload(&headers, &mut multipart).await {
        Ok(parts) => parts,
        Err(response) => return response,
    };
    let Some(spec) = parts.spec.take() else {
        return json_error(NEEDS_BOARD_AND_SPEC);
    };
    let check = state.check.clone();
    match run_blocking("the check task", move || match check {
        CheckCallback::Design(check) => parts
            .into_design_upload()
            .map(|upload| check(upload, &spec))
            .map_err(str::to_string),
        CheckCallback::Schematic(check) => {
            let Some(board_bytes) = parts.board_bytes.take() else {
                return Err(NEEDS_BOARD_AND_SPEC.to_string());
            };
            Ok(check(
                &parts.board_name,
                &board_bytes,
                parts.firmware(),
                parts.schematic(),
                &spec,
            ))
        }
    })
    .await
    {
        Ok(json) => json_ok(json),
        Err(response) => response,
    }
}

/// POST `/api/analyze`: the raw board file as the request body, with the
/// original filename in the `X-Board-Filename` header (the page sets it).
/// Board files may be text (KiCad/Eagle/IPC), a zip (gerbers) or a binary
/// container (Altium .PcbDoc). The analyzer's extractor sniffs the format from
/// the RAW bytes; decoding here to a lossy-UTF8 string would corrupt the binary
/// formats before they are ever parsed. Returns the analysis JSON.
async fn analyze_handler(
    State(analyze): State<Arc<SchematicAnalyzer>>,
    headers: HeaderMap,
    body: Bytes,
) -> JsonResponse {
    if let Some(resp) = reject_cross_site(&headers) {
        return resp;
    }
    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if content_type.starts_with("multipart/form-data") {
        return json_body(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            serde_json::json!({
                "ok": false,
                "error": "/api/analyze takes the raw board file as the request body with the \
                          file name in the X-Board-Filename header; send multipart/form-data \
                          to /api/analyze-with-firmware instead",
            }),
        );
    }
    json_ok((analyze)(board_filename(&headers), &body, None, None))
}

/// POST `/api/analyze-with-firmware`: a `multipart/form-data` upload with a
/// `board` part (required) and optional `firmware` / `schematic` parts. Every
/// part is passed as raw `&[u8]`, NEVER lossy-decoded, which would corrupt an
/// ELF or a binary board; the analyzer's extractor sniffs binary-vs-text
/// itself. An absent (or empty) firmware part falls back to a board-only
/// analysis, the same contract `/api/analyze` offers.
async fn analyze_upload_handler(
    State(analyze): State<Arc<SchematicAnalyzer>>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> JsonResponse {
    let parts = match guarded_upload(&headers, &mut multipart).await {
        Ok(parts) => parts,
        Err(response) => return response,
    };
    let Some(board_bytes) = &parts.board_bytes else {
        return json_error(NO_BOARD_PART);
    };
    json_ok((analyze)(
        &parts.board_name,
        board_bytes,
        parts.firmware(),
        parts.schematic(),
    ))
}

async fn analyze_handler_design_raw(
    State(state): State<Arc<DesignState>>,
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
    let json = (state.analyze)(DesignUpload {
        board: NamedUpload {
            name: file_name,
            bytes: body.to_vec(),
        },
        firmware: None,
        schematic: None,
        bom: None,
        placement: None,
        variant: None,
        asbuilt: None,
        models: Vec::new(),
    });
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        json,
    )
}

async fn analyze_design_handler(
    State(state): State<Arc<DesignState>>,
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
    let upload = match parts.into_design_upload() {
        Ok(upload) => upload,
        Err(message) => return json_error(message),
    };
    let analyze = state.analyze.clone();
    let json = match tokio::task::spawn_blocking(move || (analyze)(upload)).await {
        Ok(json) => json,
        Err(_) => return json_error("the analysis task panicked; see the server log"),
    };
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        json,
    )
}
