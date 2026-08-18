//! The hauksbee web server: a WebSocket that streams live simulation frames to
//! the frontend and routes user controls back to the engine, plus the HTTP router
//! that serves the React bundle, the analysis API, and (when a board is preloaded)
//! that board's own file for the geometry viewer. [`Server`] owns the sim loop;
//! the free functions assemble the drop-zone and unified app routers.

pub mod engine;
pub mod frontdoor;
pub mod protocol;
pub mod rate;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path as UrlPath, State};
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use engine::Engine;
use frontdoor::{
    CheckRunner, FirmwareAnalyzer, LiveLauncher, SchematicAnalyzer, SchematicCheckRunner,
    SchematicLiveLauncher, ToolHooks,
};
use protocol::{ClientMessage, ServerMessage, SessionBacklog, SimFrame, Status};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, Mutex};
use tower_http::compression::CompressionLayer;

const FRAME_RATE_HZ: f64 = 30.0;

struct Shared {
    tx: broadcast::Sender<String>,
    cmd: mpsc::Sender<ClientMessage>,
    board_info_json: Mutex<String>,
    /// Flipped to true when this session is replaced. Every per-socket task
    /// watches it and closes its socket: the broadcast channel alone cannot
    /// signal this (the socket task itself keeps `Shared`, and with it a live
    /// sender, alive), and a socket left open kept painting the DEAD session's
    /// last frames as if they were current.
    replaced: tokio::sync::watch::Sender<bool>,
    /// Server-held session history (accumulated faults, active probes),
    /// replayed to every new subscriber so a mid-session reload rejoins with
    /// the fault log intact. The broadcast channel alone cannot provide this:
    /// a fault is drained into exactly one frame, so a client that was not
    /// connected at that moment would never see it again. std Mutex on
    /// purpose: every critical section is a short in-memory mutation.
    backlog: std::sync::Mutex<SessionBacklog>,
}

impl Shared {
    /// Fold a frame's freshly-raised faults into the session history: first
    /// occurrence per (component, kind) keeps its timestamp, matching how the
    /// frontend's fault log accumulates, so a rejoin restores the same list a
    /// never-disconnected client would show.
    fn record_faults(&self, frame: &SimFrame) {
        if frame.faults.is_empty() {
            return;
        }
        let mut backlog = self.backlog.lock().expect("backlog lock");
        for f in &frame.faults {
            if !backlog
                .faults
                .iter()
                .any(|e| e.component == f.component && e.kind == f.kind)
            {
                backlog.faults.push(f.clone());
            }
        }
    }
}

/// One running live-sim session: the sim loop's shared state, the loop task
/// itself (so a replacement can stop it), and the session's identity for the
/// status endpoint and the board-file route.
struct LiveSession {
    shared: Arc<Shared>,
    task: tokio::task::JoinHandle<()>,
    board_name: String,
    /// (file name, KiCad layout text) served at `/boards/<file name>` so the
    /// frontend's geometry viewer can render the launched board. None for
    /// formats with no client-drawable text (Altium, gerber zips).
    board_file: Option<(String, String)>,
    /// Anything the session must keep alive for its whole run, e.g. the staged
    /// firmware temp file the emulated MCU reloads from on reset. Dropped when
    /// the session is replaced.
    _keepalive: Option<Box<dyn std::any::Any + Send>>,
}

/// The one live-sim slot behind `/ws`. `hauksbee serve` starts it empty (the
/// drop zone launches a session on demand via `/api/live/launch`);
/// `run --serve` preloads it with the CLI board. A second launch replaces the
/// current session: the old sim task is aborted after its subscribers get a
/// final "session replaced" error frame.
pub struct LiveHub {
    session: std::sync::Mutex<Option<LiveSession>>,
}

impl LiveHub {
    // LiveHub::new returns Arc<LiveHub>, so implementing Default would be the wrong trait.
    #[allow(clippy::new_without_default)]
    pub fn new() -> Arc<LiveHub> {
        Arc::new(LiveHub {
            session: std::sync::Mutex::new(None),
        })
    }

    /// Install a new live session, replacing any current one. Must be called
    /// from within a tokio runtime (the sim loop is spawned here). Returns
    /// true when an existing session was replaced.
    pub fn launch(
        &self,
        engine: Box<dyn Engine>,
        board_name: String,
        board_file: Option<(String, String)>,
        keepalive: Option<Box<dyn std::any::Any + Send>>,
    ) -> bool {
        let (tx, _) = broadcast::channel::<String>(256);
        let (cmd_tx, cmd_rx) = mpsc::channel::<ClientMessage>(64);
        // The session's identity on the wire. Many boards carry no board name
        // in their layout file, so the engine's BoardInfo.name comes back
        // empty; the frontend binds the sim surface's identity to this name
        // (the wrong-board banner, the "another session is live" chip), and an
        // empty identity reads as no identity at all. Fall back to the launch
        // file name, which is always known.
        let wire_name = {
            let engine_name = engine.board_info().name;
            if engine_name.trim().is_empty() {
                board_name.clone()
            } else {
                engine_name
            }
        };
        let board_info = serde_json::to_string(&ServerMessage::BoardInfo(named_board_info(
            &*engine, &wire_name,
        )))
        .expect("board info serializes");
        let (replaced_tx, _) = tokio::sync::watch::channel(false);
        let shared = Arc::new(Shared {
            tx: tx.clone(),
            cmd: cmd_tx,
            board_info_json: Mutex::new(board_info),
            replaced: replaced_tx,
            backlog: std::sync::Mutex::new(SessionBacklog::default()),
        });
        let task = tokio::spawn(sim_loop(engine, wire_name, tx, cmd_rx, shared.clone()));
        let old = self
            .session
            .lock()
            .expect("live hub lock")
            .replace(LiveSession {
                shared,
                task,
                board_name,
                board_file,
                _keepalive: keepalive,
            });
        match old {
            Some(old) => {
                // Tell any client still on the old socket why its stream ends,
                // then stop the old loop for real: an aborted task drops its
                // engine (and any external emulator it holds) instead of
                // stepping a board nobody can see anymore.
                let bye = ServerMessage::Error {
                    message: "live session replaced: a new board was launched".to_string(),
                };
                let _ = old
                    .shared
                    .tx
                    .send(serde_json::to_string(&bye).expect("error serializes"));
                // Close every socket still attached to the old session: the
                // frontends reconnect and land on the NEW session's stream.
                let _ = old.shared.replaced.send(true);
                old.task.abort();
                true
            }
            None => false,
        }
    }

    fn shared(&self) -> Option<Arc<Shared>> {
        self.session
            .lock()
            .expect("live hub lock")
            .as_ref()
            .map(|s| s.shared.clone())
    }

    /// Name of the currently live board, if a session is running.
    pub fn active_board(&self) -> Option<String> {
        self.session
            .lock()
            .expect("live hub lock")
            .as_ref()
            .map(|s| s.board_name.clone())
    }

    /// The launched board's own layout text, when the current session serves
    /// `name`. Backs the dynamic `/boards/{name}` route.
    fn board_file(&self, name: &str) -> Option<String> {
        self.session
            .lock()
            .expect("live hub lock")
            .as_ref()
            .and_then(|s| s.board_file.as_ref())
            .filter(|(n, _)| n == name)
            .map(|(_, contents)| contents.clone())
    }
}

pub struct Server {
    hub: Arc<LiveHub>,
}

impl Server {
    /// Spawn the simulation loop around `engine` and return the server.
    pub fn new(engine: Box<dyn Engine>) -> Server {
        Server::new_named(engine, None)
    }

    /// [`Server::new`] with an explicit session name for `/api/live/status`
    /// and the frontend's session-identity surfaces. Callers that know the
    /// board's FILE name should pass it: many layout files carry no board
    /// name, so the engine-derived fallback is often empty.
    pub fn new_named(engine: Box<dyn Engine>, name: Option<String>) -> Server {
        let hub = LiveHub::new();
        let name = name
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| engine.board_info().name.clone());
        hub.launch(engine, name, None, None);
        Server { hub }
    }

    /// The live-session hub this server preloaded, for wiring the launch API.
    pub fn hub(&self) -> Arc<LiveHub> {
        self.hub.clone()
    }

    pub fn router(&self, static_dir: Option<&std::path::Path>) -> Router {
        self.router_with_board(static_dir, None)
    }

    /// Build the router, optionally serving the loaded board's own file at a
    /// fixed URL path so the frontend's geometry renderer can fetch it. The
    /// static `dist/` only carries the demo boards, so without this any user
    /// board would 404 in the 2D/3D view; serving the actual file here makes
    /// `hauksbee run <any board>` show its real geometry.
    pub fn router_with_board(
        &self,
        static_dir: Option<&std::path::Path>,
        board_file: Option<(String, String)>,
    ) -> Router {
        let mut router = Router::new()
            .route("/ws", get(ws_handler))
            .with_state(self.hub.clone());
        if let Some((url_path, contents)) = board_file {
            router = router.route(
                &url_path,
                get(move || async move {
                    (
                        [(
                            axum::http::header::CONTENT_TYPE,
                            "text/plain; charset=utf-8",
                        )],
                        contents,
                    )
                }),
            );
        }
        // The resume probe, answered here too. The frontend asks
        // `/boards/{name}` on every session resume whether the server still
        // holds the uploaded bytes; this router had no such route, so the answer
        // came from the static fallback as a 404, which is the browser console
        // error on every resume that the contract exists to avoid. An exact
        // `board_file` route registered above still wins for the board that is
        // actually loaded. See `live_board_handler` for the contract.
        router = router.route("/boards/{name}", get(no_board_handler));
        if let Some(dir) = static_dir {
            router = router.fallback_service(tower_http::services::ServeDir::new(dir));
        }
        // Gzip responses on the fly. The frontend's .glb board models are ~14 MB
        // uncompressed but ~1.9 MB gzipped, and every browser sends
        // `Accept-Encoding: gzip`; the WebSocket upgrade (a bodyless 101) passes
        // through untouched.
        router
            .layer(axum::middleware::map_response(no_cache_html))
            .layer(CompressionLayer::new())
    }

    pub async fn serve(
        &self,
        addr: &str,
        static_dir: Option<&std::path::Path>,
    ) -> anyhow::Result<()> {
        self.serve_with_board(addr, static_dir, None).await
    }

    pub async fn serve_with_board(
        &self,
        addr: &str,
        static_dir: Option<&std::path::Path>,
        board_file: Option<(String, String)>,
    ) -> anyhow::Result<()> {
        // Fall back to a nearby / OS-assigned port if the requested one is busy,
        // rather than dying with a bare "Address already in use (os error 48)".
        let listener = match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                let mut chosen = None;
                if let Ok(mut sa) = addr.parse::<std::net::SocketAddr>() {
                    let base = sa.port();
                    for p in (base + 1)..=(base + 20) {
                        sa.set_port(p);
                        if let Ok(l) = tokio::net::TcpListener::bind(sa).await {
                            eprintln!("  (port {base} was busy; using {p} instead)");
                            chosen = Some(l);
                            break;
                        }
                    }
                    if chosen.is_none() {
                        sa.set_port(0);
                        chosen = tokio::net::TcpListener::bind(sa).await.ok();
                    }
                }
                chosen.ok_or(e)?
            }
            Err(e) => return Err(e.into()),
        };
        let bound = listener.local_addr()?;
        eprintln!("hauksbee-server listening on http://{bound}");
        axum::serve(listener, self.router_with_board(static_dir, board_file)).await?;
        Ok(())
    }

    /// The unified web app router: one server path, and so one web experience,
    /// serving the static React bundle (`static_dir`) with no server-rendered
    /// HTML alternative, the analysis API the React landing calls
    /// (`analyze` -> `/api/analyze` + `/api/analyze-with-firmware`), the
    /// `/api/startup` hint the app reads to choose its landing state, the live
    /// WebSocket sim (`/ws`), and, when a board was preloaded (`run --serve`),
    /// that board's own file so the viewer renders its real geometry.
    ///
    /// `startup_json` is the JSON the frontend fetches from `/api/startup`:
    /// `{"preloaded":false}` for `serve` (lands on the drop zone) or
    /// `{"preloaded":true,"board_name":...,"report":<WebReport>}` for
    /// `run --serve` (lands on that board's report, "run it" expands to the sim).
    pub fn app_router(
        &self,
        static_dir: Option<&Path>,
        board_file: Option<(String, String)>,
        analyze: FirmwareAnalyzer,
        check: Option<CheckRunner>,
        tools: Option<ToolHooks>,
        launch: Option<LiveLauncher>,
        startup_json: String,
    ) -> Router {
        unified_router(
            Some(self.hub.clone()),
            static_dir,
            board_file,
            Some(analyze),
            None,
            check,
            None,
            tools,
            launch,
            None,
            startup_json,
        )
    }

    /// Serve the unified app router (WebSocket sim included). Used by
    /// `hauksbee run --serve`, where a board is preloaded.
    pub async fn serve_app(
        &self,
        addr: &str,
        static_dir: Option<&Path>,
        board_file: Option<(String, String)>,
        analyze: FirmwareAnalyzer,
        check: Option<CheckRunner>,
        tools: Option<ToolHooks>,
        launch: Option<LiveLauncher>,
        startup_json: String,
    ) -> anyhow::Result<()> {
        let listener = bind_with_fallback(addr).await?;
        let router = self.app_router(
            static_dir,
            board_file,
            analyze,
            check,
            tools,
            launch,
            startup_json,
        );
        axum::serve(listener, router).await?;
        Ok(())
    }

    /// Serve the unified app router on a listener already produced by
    /// [`bind_frontdoor`], so the caller can print the *actually bound* URL
    /// before the server takes over the thread (the requested port may have
    /// been busy and replaced by a fallback).
    pub async fn serve_app_on(
        &self,
        listener: tokio::net::TcpListener,
        static_dir: Option<&Path>,
        board_file: Option<(String, String)>,
        analyze: FirmwareAnalyzer,
        check: Option<CheckRunner>,
        tools: Option<ToolHooks>,
        launch: Option<LiveLauncher>,
        startup_json: String,
    ) -> anyhow::Result<()> {
        let router = self.app_router(
            static_dir,
            board_file,
            analyze,
            check,
            tools,
            launch,
            startup_json,
        );
        axum::serve(listener, router).await?;
        Ok(())
    }

    /// Schematic-aware counterpart to [`Self::serve_app_on`]. Kept separate so
    /// existing embedders retain their public callback signatures.
    pub async fn serve_app_on_with_schematic(
        &self,
        listener: tokio::net::TcpListener,
        static_dir: Option<&Path>,
        board_file: Option<(String, String)>,
        analyze: SchematicAnalyzer,
        check: Option<SchematicCheckRunner>,
        tools: Option<ToolHooks>,
        launch: Option<SchematicLiveLauncher>,
        startup_json: String,
    ) -> anyhow::Result<()> {
        let router = unified_router(
            Some(self.hub.clone()),
            static_dir,
            board_file,
            None,
            Some(analyze),
            None,
            check,
            tools,
            None,
            launch,
            startup_json,
        );
        axum::serve(listener, router).await?;
        Ok(())
    }
}

/// Assemble the unified router from its optional parts. `hub` is the live-sim
/// slot behind `/ws`: preloaded for `run --serve`, empty for `serve` until the
/// user launches an uploaded board. `launch` mounts the `/api/live/*` routes
/// that fill (or replace) the hub's session server-side; a deployment without
/// the callback keeps the CLI-hint fallback in the frontend.
fn unified_router(
    hub: Option<Arc<LiveHub>>,
    static_dir: Option<&Path>,
    board_file: Option<(String, String)>,
    analyze: Option<FirmwareAnalyzer>,
    schematic_analyze: Option<SchematicAnalyzer>,
    check: Option<CheckRunner>,
    schematic_check: Option<SchematicCheckRunner>,
    tools: Option<ToolHooks>,
    launch: Option<LiveLauncher>,
    schematic_launch: Option<SchematicLiveLauncher>,
    startup_json: String,
) -> Router {
    let mut router = Router::new();
    if let Some(hub) = &hub {
        router = router.merge(
            Router::new()
                .route("/ws", get(ws_handler))
                // The launched board's own file, so the geometry viewer can
                // fetch what was just uploaded (the static dist/ only carries
                // demo boards). A `run --serve` preloaded board's static route
                // (exact path, below) takes priority over this parameterised
                // one, keeping its historical behaviour byte-identical.
                .route("/boards/{name}", get(live_board_handler))
                .with_state(hub.clone()),
        );
    } else {
        // No live hub, and the probe still has to be answered. The frontend asks
        // this route on every session resume whether the server still holds the
        // uploaded bytes, and with no hub there was no route at all, so the
        // static fallback answered 404: the exact browser console error the
        // contract above exists to avoid, on the most ordinary configuration
        // there is. "No board here" is an answer, not an error.
        router = router.merge(Router::new().route("/boards/{name}", get(no_board_handler)));
    }
    if let (Some(hub), Some(launch)) = (&hub, launch) {
        router = router.merge(frontdoor::live_routes(hub.clone(), launch));
    }
    if let (Some(hub), Some(launch)) = (&hub, schematic_launch) {
        router = router.merge(frontdoor::live_routes_with_schematic(hub.clone(), launch));
    }
    if let Some(analyze) = analyze {
        router = router.merge(frontdoor::api_routes(analyze));
    }
    if let Some(analyze) = schematic_analyze {
        router = router.merge(frontdoor::api_routes_with_schematic(analyze));
    }
    // The web checks panel's backend (`POST /api/check`): present whenever the
    // embedding binary supplied a runner (the hauksbee-ci shell-out).
    if let Some(check) = check {
        router = router.merge(frontdoor::check_route(check));
    }
    if let Some(check) = schematic_check {
        router = router.merge(frontdoor::check_route_with_schematic(check));
    }
    // The dependency panel's backend (`GET /api/deps`, `POST
    // /api/deps/install/{id}`) and the datasheet-extraction backend
    // (`/api/models/*`): present whenever the embedding binary supplied the
    // engine's hooks. Mounted together because they are one panel's worth of
    // capability, and because a UI that can see codex in the dependency list
    // but cannot reach the extract route would offer a button that 404s.
    if let Some(tools) = tools {
        router = router.merge(frontdoor::deps_routes(tools.deps_status, tools.install));
        router = router.merge(frontdoor::datasheet_routes(tools.datasheet));
    }
    // `/api/startup`: the frontend reads this once on load to decide whether to
    // show the drop zone (serve) or a preloaded board's report (run --serve).
    router = router.route(
        "/api/startup",
        get(move || {
            let body = startup_json.clone();
            async move { ([(header::CONTENT_TYPE, "application/json")], body) }
        }),
    );
    if let Some((url_path, contents)) = board_file {
        router = router.route(
            &url_path,
            get(move || async move {
                (
                    [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                    contents,
                )
            }),
        );
    }
    if let Some(dir) = static_dir {
        router = router.fallback_service(tower_http::services::ServeDir::new(dir));
    }
    // Gzip on the fly: the frontend's .glb board models are ~14 MB uncompressed,
    // ~1.9 MB gzipped. The `/ws` upgrade (a bodyless 101) passes through.
    router
        .layer(axum::middleware::map_response(no_cache_html))
        .layer(CompressionLayer::new())
}

/// Mark HTML responses `Cache-Control: no-cache` so a browser always
/// revalidates `index.html` against the served `dist/`. Vite's hashed asset
/// names make everything else safely cacheable, but the entry HTML is served
/// under a stable name, a cached copy keeps pointing at old asset hashes and
/// resurrects "already fixed" bugs after a rebuild. `no-cache` still allows a
/// 304 when the file is unchanged (ServeDir handles conditional requests).
async fn no_cache_html(mut res: axum::response::Response) -> axum::response::Response {
    let is_html = res
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|ct| ct.starts_with("text/html"))
        .unwrap_or(false);
    if is_html {
        res.headers_mut().insert(
            header::CACHE_CONTROL,
            axum::http::HeaderValue::from_static("no-cache"),
        );
    }
    res
}

/// Serve the drop-zone-only front door (no preloaded board, no live engine):
/// the static React bundle, the analysis API, and `/api/startup` reporting
/// `preloaded:false`. Used by `hauksbee serve`. The React landing lands on the
/// drop zone; the WebSocket sim is not mounted here (there is no board to run
/// until the user brings one via `run --serve`).
pub async fn serve_frontdoor(
    addr: &str,
    static_dir: Option<&Path>,
    analyze: FirmwareAnalyzer,
    check: Option<CheckRunner>,
    tools: Option<ToolHooks>,
    startup_json: String,
) -> anyhow::Result<()> {
    let (listener, _bound) = bind_frontdoor(addr).await?;
    serve_frontdoor_on(
        listener,
        static_dir,
        analyze,
        check,
        tools,
        None,
        startup_json,
    )
    .await
}

/// Bind the front-door address (applying the busy-port fallback) and return the
/// listener together with the address that was *actually* bound. The requested
/// port and the bound port differ whenever the requested one was in use, so a
/// caller must print a URL from this returned address, not from the requested
/// `addr`, or it advertises a stale port.
pub async fn bind_frontdoor(
    addr: &str,
) -> anyhow::Result<(tokio::net::TcpListener, std::net::SocketAddr)> {
    let listener = bind_with_fallback(addr).await?;
    let bound = listener.local_addr()?;
    Ok((listener, bound))
}

/// Serve the drop-zone front door on a listener already produced by
/// [`bind_frontdoor`], so the caller can print the real bound URL before the
/// server takes over the thread.
pub async fn serve_frontdoor_on(
    listener: tokio::net::TcpListener,
    static_dir: Option<&Path>,
    analyze: FirmwareAnalyzer,
    check: Option<CheckRunner>,
    tools: Option<ToolHooks>,
    launch: Option<LiveLauncher>,
    startup_json: String,
) -> anyhow::Result<()> {
    // The hub starts empty: `/ws` answers 409 until a board is launched. It is
    // mounted even without a launcher so the route surface stays stable.
    let hub = LiveHub::new();
    let router = unified_router(
        Some(hub),
        static_dir,
        None,
        Some(analyze),
        None,
        check,
        None,
        tools,
        launch,
        None,
        startup_json,
    );
    axum::serve(listener, router).await?;
    Ok(())
}

/// Schematic-aware standalone front door. Existing callers keep using
/// [`serve_frontdoor_on`]; the shipped engine uses this path so an uploaded
/// Eagle `.sch` reaches both report analysis and live launch.
pub async fn serve_frontdoor_on_with_schematic(
    listener: tokio::net::TcpListener,
    static_dir: Option<&Path>,
    analyze: SchematicAnalyzer,
    check: Option<SchematicCheckRunner>,
    tools: Option<ToolHooks>,
    launch: Option<SchematicLiveLauncher>,
    startup_json: String,
) -> anyhow::Result<()> {
    let hub = LiveHub::new();
    let router = unified_router(
        Some(hub),
        static_dir,
        None,
        None,
        Some(analyze),
        None,
        check,
        tools,
        None,
        launch,
        startup_json,
    );
    axum::serve(listener, router).await?;
    Ok(())
}

/// Bind `addr`, but if its port is busy fall back to the next few ports and
/// finally an OS-assigned free port, so a server launch never dies with a bare
/// "Address already in use". The caller prints the actual bound address.
async fn bind_with_fallback(addr: &str) -> anyhow::Result<tokio::net::TcpListener> {
    use std::net::SocketAddr;
    match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => Ok(l),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            if let Ok(mut sa) = addr.parse::<SocketAddr>() {
                let base = sa.port();
                for p in (base + 1)..=(base + 20) {
                    sa.set_port(p);
                    if let Ok(l) = tokio::net::TcpListener::bind(sa).await {
                        eprintln!("  (port {base} was busy; using {p} instead)");
                        return Ok(l);
                    }
                }
                sa.set_port(0);
                if let Ok(l) = tokio::net::TcpListener::bind(sa).await {
                    return Ok(l);
                }
            }
            Err(e.into())
        }
        // A privileged port (< 1024) fails with PermissionDenied, which is not
        // the busy-port case the automatic fallback exists for; falling back
        // would silently serve somewhere the user did not ask for. Name the
        // port and the reason instead.
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            let port = addr
                .parse::<std::net::SocketAddr>()
                .map(|sa| sa.port())
                .unwrap_or(0);
            if port != 0 && port < 1024 {
                Err(anyhow::anyhow!(
                    "cannot bind port {port}: ports below 1024 need root/administrator \
                     privileges. Pick a port of 1024 or above (e.g. --port 3001). The \
                     automatic next-free-port fallback only applies to BUSY ports, not \
                     privileged ones."
                ))
            } else {
                Err(e.into())
            }
        }
        Err(e) => Err(e.into()),
    }
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    headers: axum::http::HeaderMap,
    State(hub): State<Arc<LiveHub>>,
) -> impl IntoResponse {
    // The same-origin policy does not cover WebSockets. Any page in any tab can
    // open ws://127.0.0.1:<port>/ws, and the port is guessable because the
    // fallback ladder is a twenty-port range. Without this check that page
    // receives the BoardInfo frame on connect, which is the netlist of whatever
    // proprietary board the user is simulating, then every SimFrame after it,
    // and can send Reset / Play / SetPowerSupply / Serial back down the same
    // socket. The HTTP endpoints are guarded by `reject_cross_site`, but that
    // reads `Sec-Fetch-Site`, which browsers do NOT send on a WS handshake, so
    // this needs its own check against `Origin`.
    if let Err(why) = origin_is_ours(&headers) {
        return (StatusCode::FORBIDDEN, why).into_response();
    }

    // No session yet (a `serve` front door before any live launch): refuse the
    // upgrade with a clear status instead of accepting a socket that would
    // never speak. The frontend only opens `/ws` after a successful launch, so
    // hitting this means a stale tab or a race; its reconnect loop recovers.
    let Some(shared) = hub.shared() else {
        return (StatusCode::CONFLICT, "no live sim session is running").into_response();
    };
    ws.on_upgrade(move |socket| handle_socket(socket, shared))
        .into_response()
}

/// Whether a WebSocket handshake came from our own page.
///
/// A browser always sends `Origin` on a WS handshake and cannot forge it from
/// page script, so it is the reliable signal here. Only loopback origins pass:
/// the server binds 127.0.0.1 only, so our own page is always served from one,
/// and any other origin is by definition somebody else's site.
///
/// A missing `Origin` is allowed, because non-browser clients (a test, a CLI
/// tool, `websocat`) do not send one and they are not the threat: this defends
/// against a hostile PAGE, which cannot omit it.
fn origin_is_ours(headers: &axum::http::HeaderMap) -> Result<(), &'static str> {
    let Some(origin) = headers.get(axum::http::header::ORIGIN) else {
        return Ok(());
    };
    let Ok(origin) = origin.to_str() else {
        return Err("cross-origin websocket refused: unreadable Origin header");
    };
    let host = origin
        .split("://")
        .nth(1)
        .unwrap_or(origin)
        .split('/')
        .next()
        .unwrap_or("");
    // Strip the port: any loopback port is our own server or another local
    // tool, and the alternative is guessing which port we ended up on after
    // the fallback ladder.
    let hostname = host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host);
    if matches!(hostname, "127.0.0.1" | "localhost" | "[::1]" | "::1") {
        Ok(())
    } else {
        Err(
            "cross-origin websocket refused: this socket carries the board you are \
             simulating and accepts control messages, so it answers only the hauksbee \
             page itself",
        )
    }
}

/// GET `/boards/{name}`: the CURRENT live session's own board file, for the
/// geometry viewer.
///
/// Contract: a hit is `200 text/plain` carrying the board text. A miss is
/// `200 application/json` carrying `{"available":false}`, NOT a 404: the
/// frontend speculatively probes this route on every session resume to ask
/// whether the server still holds the uploaded bytes, and a 404 answer to
/// that legitimate question shows up as a browser console error on every
/// resume. The two cases are distinguished by Content-Type (board text is
/// never JSON).
async fn live_board_handler(
    State(hub): State<Arc<LiveHub>>,
    UrlPath(name): UrlPath<String>,
) -> axum::response::Response {
    match hub.board_file(&name) {
        Some(contents) => (
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            contents,
        )
            .into_response(),
        None => (
            [(header::CONTENT_TYPE, "application/json")],
            "{\"available\":false}",
        )
            .into_response(),
    }
}

/// GET `/boards/{name}` with no live session: the same `200 application/json`
/// `{"available":false}` miss answer [`live_board_handler`] gives, so the resume
/// probe gets the same shape whether or not a session exists.
async fn no_board_handler() -> axum::response::Response {
    (
        [(header::CONTENT_TYPE, "application/json")],
        "{\"available\":false}",
    )
        .into_response()
}

async fn handle_socket(mut socket: WebSocket, shared: Arc<Shared>) {
    let info = shared.board_info_json.lock().await.clone();
    if socket.send(Message::Text(info.into())).await.is_err() {
        return;
    }
    // Subscribe BEFORE snapshotting the backlog. The other order has a hole:
    // a terminal failure landing between the snapshot and the subscribe would
    // be in neither (the snapshot predates it, the broadcast preceded the
    // subscription), and this socket would never learn the session died. With
    // the subscription first, any such event is in the snapshot, in the
    // buffered broadcast, or both (a duplicate Error is idempotent).
    let mut rx = shared.tx.subscribe();
    // Replay the session's server-held history right after the identity frame:
    // faults are drained into exactly one broadcast frame each, so without
    // this a client that reloads mid-session would show an empty fault log
    // over a sim that kept running (and faulting) the whole time.
    let backlog = shared.backlog.lock().expect("backlog lock").clone();
    let backlog_json =
        serde_json::to_string(&ServerMessage::Backlog(backlog)).expect("backlog serializes");
    if socket
        .send(Message::Text(backlog_json.into()))
        .await
        .is_err()
    {
        return;
    }
    let mut replaced_rx = shared.replaced.subscribe();
    // A socket attached to an already-replaced session closes immediately.
    if *replaced_rx.borrow() {
        return;
    }
    loop {
        tokio::select! {
            // The session was replaced: close this socket so the client's
            // reconnect loop attaches to the NEW session instead of painting
            // the dead one's last frames forever.
            changed = replaced_rx.changed() => {
                if changed.is_err() || *replaced_rx.borrow() {
                    return;
                }
            }
            broadcasted = rx.recv() => {
                match broadcasted {
                    Ok(json) => {
                        if socket.send(Message::Text(json.into())).await.is_err() {
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
            incoming = socket.recv() => {
                let Some(Ok(msg)) = incoming else { return };
                if let Message::Text(text) = msg {
                    match serde_json::from_str::<ClientMessage>(&text) {
                        Ok(cmd) => {
                            let _ = shared.cmd.send(cmd).await;
                        }
                        Err(e) => {
                            let err = ServerMessage::Error { message: e.to_string() };
                            let _ = socket
                                .send(Message::Text(
                                    serde_json::to_string(&err).unwrap().into(),
                                ))
                                .await;
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod rate_honesty_tests {
    //! The sim loop's rate accounting, tested against an artificially slow
    //! engine: the streamed `realtime_factor` must report what the loop
    //! DELIVERED, not the requested multiplier, and must track a known step
    //! cost within tolerance.

    use super::*;
    use crate::protocol::{BoardInfo, SimFrame, SolverControls};

    /// An engine whose step burns `cost` wall seconds per sim second, so the
    /// sustainable rate is exactly 1/cost and the loop's honesty is checkable
    /// against a known ceiling.
    struct SlowEngine {
        sim_time: f64,
        cost: f64,
        controls: SolverControls,
        /// Steps actually taken, observable from the test after the engine
        /// moves into the loop; proves a "responsiveness while grinding"
        /// window really contained a grinding step.
        stepped: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Engine for SlowEngine {
        fn board_info(&self) -> BoardInfo {
            BoardInfo {
                name: "slow".into(),
                board_url: String::new(),
                num_components: 0,
                num_nets: 0,
                nets: Vec::new(),
                component_kinds: Default::default(),
                mcus: Vec::new(),
                power_supplies: Default::default(),
                peripherals: Default::default(),
                input_sources: Default::default(),
                shorts: None,
            }
        }
        fn step(&mut self, dt: f64) -> SimFrame {
            std::thread::sleep(std::time::Duration::from_secs_f64(dt * self.cost));
            self.sim_time += dt;
            self.stepped
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            SimFrame {
                t: self.sim_time,
                ..Default::default()
            }
        }
        fn reset(&mut self) {
            self.sim_time = 0.0;
        }
        fn set_controls(&mut self, controls: SolverControls) {
            self.controls = controls;
        }
        fn controls(&self) -> SolverControls {
            self.controls.clone()
        }
        fn serial(&mut self, _mcu: &str, _data: &[u8]) {}
        fn set_input(&mut self, _source: &str, _value: f64) {}
    }

    /// Run a sim loop over `engine` for `secs` of wall time at requested
    /// speed 1.0 and return the last streamed frame.
    async fn last_frame_after(engine: SlowEngine, secs: f64) -> SimFrame {
        let (tx, mut rx) = broadcast::channel::<String>(1024);
        let (cmd_tx, cmd_rx) = mpsc::channel::<ClientMessage>(8);
        let shared = Arc::new(Shared {
            tx: tx.clone(),
            cmd: cmd_tx.clone(),
            board_info_json: Mutex::new(String::new()),
            replaced: tokio::sync::watch::channel(false).0,
            backlog: std::sync::Mutex::new(SessionBacklog::default()),
        });
        let task = tokio::spawn(sim_loop(
            Box::new(engine),
            "slow".into(),
            tx,
            cmd_rx,
            shared,
        ));
        cmd_tx.send(ClientMessage::Play).await.unwrap();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs_f64(secs);
        let mut last: Option<SimFrame> = None;
        while tokio::time::Instant::now() < deadline {
            let timeout = tokio::time::sleep_until(deadline);
            tokio::select! {
                _ = timeout => break,
                msg = rx.recv() => {
                    match msg {
                        Ok(json) => {
                            if let Ok(ServerMessage::SimFrame(f)) =
                                serde_json::from_str::<ServerMessage>(&json)
                            {
                                last = Some(f);
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        }
        task.abort();
        last.expect("the loop streamed at least one frame")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn slow_engine_reports_achieved_below_requested() {
        // Cost 5 wall s per sim s: sustainable 0.2x. A 1.0x request must come
        // back with achieved well under 1.0 and the honest cap flagged.
        let frame = last_frame_after(
            SlowEngine {
                sim_time: 0.0,
                cost: 5.0,
                controls: SolverControls::default(),
                stepped: Default::default(),
            },
            2.0,
        )
        .await;
        assert_eq!(frame.requested_factor, 1.0);
        assert!(frame.rate_limited, "the 1.0x request exceeds the ceiling");
        assert!(
            frame.realtime_factor < 0.5,
            "achieved {} must not approach the requested 1.0",
            frame.realtime_factor
        );
        // Tracks the known ceiling (0.2x, paced to 0.18x) within a loose CI
        // tolerance: scheduler jitter and sleep overshoot only push it DOWN.
        assert!(
            (0.03..=0.30).contains(&frame.realtime_factor),
            "achieved {} should track the ~0.2x ceiling",
            frame.realtime_factor
        );
    }

    /// Wedge regression (the lily58 corpus hang, defect 2): one session whose
    /// step grinds for a second at a time must not stall the rest of the
    /// runtime. The CURRENT-THREAD flavor is deliberate: it is the
    /// deterministic stand-in for the production failure, where the runtime
    /// worker stuck inside `engine.step` was the one holding tokio's I/O +
    /// timer driver, so every page load and websocket on the whole server
    /// timed out. With the step inlined on the runtime (the old code), this
    /// test's timers freeze for the full step and the asserted latency blows
    /// past the bound (measured pre-fix: ~1 s); with the step on the blocking
    /// pool they keep firing on time.
    #[tokio::test]
    async fn a_grinding_step_does_not_stall_the_runtime() {
        let (tx, _rx) = broadcast::channel::<String>(1024);
        let (cmd_tx, cmd_rx) = mpsc::channel::<ClientMessage>(8);
        let shared = Arc::new(Shared {
            tx: tx.clone(),
            cmd: cmd_tx.clone(),
            board_info_json: Mutex::new(String::new()),
            replaced: tokio::sync::watch::channel(false).0,
            backlog: std::sync::Mutex::new(SessionBacklog::default()),
        });
        // Each ~33 ms frame step burns ~1 s of wall time (cost 30x): the
        // shape of a solver grinding its retry ladder.
        let stepped: Arc<std::sync::atomic::AtomicUsize> = Default::default();
        let task = tokio::spawn(sim_loop(
            Box::new(SlowEngine {
                sim_time: 0.0,
                cost: 30.0,
                controls: SolverControls::default(),
                stepped: stepped.clone(),
            }),
            "grind".into(),
            tx,
            cmd_rx,
            shared,
        ));
        cmd_tx.send(ClientMessage::Play).await.unwrap();
        // While the engine grinds, short timers on the same runtime must keep
        // firing on schedule. Track the worst observed latency of a 20 ms
        // sleep over ~1.5 s of wall time (covering at least one full step).
        let mut worst = std::time::Duration::ZERO;
        let t_end = std::time::Instant::now() + std::time::Duration::from_millis(1500);
        while std::time::Instant::now() < t_end {
            let t0 = std::time::Instant::now();
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            worst = worst.max(t0.elapsed());
        }
        task.abort();
        // The latency bound is only evidence if a grinding step actually ran
        // inside the window; a loop that never stepped would pass vacuously.
        assert!(
            stepped.load(std::sync::atomic::Ordering::Relaxed) >= 1,
            "the engine never stepped: this run measured nothing"
        );
        assert!(
            worst < std::time::Duration::from_millis(400),
            "a 20 ms timer stalled {worst:?} while the engine stepped: the sim loop is \
             blocking the runtime again"
        );
    }

    /// An engine whose analog solve dies permanently after the first step,
    /// reporting it the way [`Engine::analog_failure`] contracts.
    struct DeadSolveEngine {
        sim_time: f64,
        steps: u32,
        controls: SolverControls,
        /// Flipped by `reset()`: the engine is healthy afterwards, so the
        /// test can prove Reset actually reopens the session (the latch
        /// lifecycle), not just that the failure fired once.
        healed: bool,
    }

    impl Engine for DeadSolveEngine {
        fn board_info(&self) -> BoardInfo {
            BoardInfo {
                name: "dead".into(),
                board_url: String::new(),
                num_components: 0,
                num_nets: 0,
                nets: Vec::new(),
                component_kinds: Default::default(),
                mcus: Vec::new(),
                power_supplies: Default::default(),
                peripherals: Default::default(),
                input_sources: Default::default(),
                shorts: None,
            }
        }
        fn step(&mut self, dt: f64) -> SimFrame {
            self.steps += 1;
            self.sim_time += dt;
            SimFrame {
                t: self.sim_time,
                ..Default::default()
            }
        }
        fn reset(&mut self) {
            self.steps = 0;
            self.sim_time = 0.0;
            self.healed = true;
        }
        fn set_controls(&mut self, controls: SolverControls) {
            self.controls = controls;
        }
        fn controls(&self) -> SolverControls {
            self.controls.clone()
        }
        fn serial(&mut self, _mcu: &str, _data: &[u8]) {}
        fn set_input(&mut self, _source: &str, _value: f64) {}
        fn analog_failure(&self) -> Option<String> {
            (!self.healed && self.steps >= 1)
                .then(|| "Newton failed at t=0 even at dt_min".to_string())
        }
    }

    /// Session-lifecycle regression (defect 2's second half): a live session
    /// whose analog solve is irrecoverably failing must stop stepping and put
    /// the reason on the wire, instead of grinding the dead solve forever
    /// while clients watch a frozen clock. Two-sided: without the
    /// `analog_failure` check in the sim loop, no Error arrives and frames
    /// keep flowing, so both assertions fail.
    #[tokio::test]
    async fn a_dead_analog_solve_stops_the_session_with_the_reason() {
        let (tx, mut rx) = broadcast::channel::<String>(1024);
        let (cmd_tx, cmd_rx) = mpsc::channel::<ClientMessage>(8);
        let shared = Arc::new(Shared {
            tx: tx.clone(),
            cmd: cmd_tx.clone(),
            board_info_json: Mutex::new(String::new()),
            replaced: tokio::sync::watch::channel(false).0,
            backlog: std::sync::Mutex::new(SessionBacklog::default()),
        });
        let task = tokio::spawn(sim_loop(
            Box::new(DeadSolveEngine {
                sim_time: 0.0,
                steps: 0,
                controls: SolverControls::default(),
                healed: false,
            }),
            "dead".into(),
            tx,
            cmd_rx,
            shared.clone(),
        ));
        cmd_tx.send(ClientMessage::Play).await.unwrap();
        // Expect the honest stop: an Error naming the failure, then silence
        // on the frame stream (running was forced false).
        let mut saw_error = false;
        let mut frames_after_error = 0u32;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            let timeout = tokio::time::sleep_until(deadline);
            tokio::select! {
                _ = timeout => break,
                msg = rx.recv() => match msg {
                    Ok(json) => match serde_json::from_str::<ServerMessage>(&json) {
                        Ok(ServerMessage::Error { message }) => {
                            assert!(
                                message.contains("live simulation stopped")
                                    && message.contains("Newton failed at t=0"),
                                "the stop must carry the engine's own reason, got: {message}"
                            );
                            saw_error = true;
                        }
                        Ok(ServerMessage::SimFrame(_)) if saw_error => frames_after_error += 1,
                        _ => {}
                    },
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                },
            }
        }
        assert!(
            saw_error,
            "a permanently failing analog solve must put an Error on the wire"
        );
        assert_eq!(
            frames_after_error, 0,
            "the session must stop stepping once the solve is declared dead"
        );
        // Late-subscriber honesty: the reason is also in the backlog, so a
        // client that connects (or reloads) AFTER the failure still learns
        // why the sim is stopped; the broadcast alone is dropped when nobody
        // is subscribed at that instant.
        let fatal = shared.backlog.lock().expect("backlog lock").fatal.clone();
        assert!(
            fatal.is_some_and(|f| f.contains("Newton failed at t=0")),
            "the terminal failure must be recorded for replay to late subscribers"
        );

        // The fatal latch: Play on a dead session must NOT step again (it
        // re-broadcasts the reason instead of re-grinding the dead solve).
        cmd_tx.send(ClientMessage::Play).await.unwrap();
        let mut errors_after_replay = 0u32;
        let mut frames_after_replay = 0u32;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(500);
        while tokio::time::Instant::now() < deadline {
            let timeout = tokio::time::sleep_until(deadline);
            tokio::select! {
                _ = timeout => break,
                msg = rx.recv() => match msg {
                    Ok(json) => match serde_json::from_str::<ServerMessage>(&json) {
                        Ok(ServerMessage::Error { .. }) => errors_after_replay += 1,
                        Ok(ServerMessage::SimFrame(_)) => frames_after_replay += 1,
                        _ => {}
                    },
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                },
            }
        }
        assert!(
            errors_after_replay >= 1,
            "Play on a fatal session must re-broadcast the reason \
             (frames_after_replay={frames_after_replay})"
        );
        assert_eq!(
            frames_after_replay, 0,
            "Play on a fatal session must not step the dead solve again"
        );

        // Reset clears the latch (the engine heals in this fixture): the
        // refreshed backlog is broadcast as the acknowledgement that lifts
        // the banner, and Play flows frames again.
        cmd_tx.send(ClientMessage::Reset).await.unwrap();
        cmd_tx.send(ClientMessage::Play).await.unwrap();
        let mut saw_cleared_backlog = false;
        let mut frames_after_reset = 0u32;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            let timeout = tokio::time::sleep_until(deadline);
            tokio::select! {
                _ = timeout => break,
                msg = rx.recv() => match msg {
                    Ok(json) => match serde_json::from_str::<ServerMessage>(&json) {
                        Ok(ServerMessage::Backlog(b)) => {
                            if b.fatal.is_none() {
                                saw_cleared_backlog = true;
                            }
                        }
                        Ok(ServerMessage::SimFrame(_)) => {
                            frames_after_reset += 1;
                            if frames_after_reset >= 2 && saw_cleared_backlog {
                                break;
                            }
                        }
                        _ => {}
                    },
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                },
            }
        }
        assert!(
            saw_cleared_backlog,
            "Reset must broadcast the refreshed backlog (the banner-lift acknowledgement)"
        );
        assert!(
            frames_after_reset >= 1,
            "after Reset heals the engine, Play must step again"
        );
        assert!(
            shared.backlog.lock().expect("backlog lock").fatal.is_none(),
            "Reset must clear the recorded terminal failure"
        );
        task.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn fast_engine_is_not_capped_and_reports_near_requested() {
        // Near-zero cost: the loop holds the requested 1.0x, and the reported
        // achieved rate must sit near it (generous lower bound for CI noise).
        let frame = last_frame_after(
            SlowEngine {
                sim_time: 0.0,
                cost: 0.01,
                controls: SolverControls::default(),
                stepped: Default::default(),
            },
            1.5,
        )
        .await;
        assert_eq!(frame.requested_factor, 1.0);
        assert!(!frame.rate_limited);
        assert!(
            (0.5..=1.1).contains(&frame.realtime_factor),
            "achieved {} should be near the requested 1.0",
            frame.realtime_factor
        );
    }
}

/// The engine's BoardInfo with the session's wire name applied (see the
/// fallback rationale in [`LiveHub::launch`]).
fn named_board_info(engine: &dyn Engine, wire_name: &str) -> protocol::BoardInfo {
    let mut info = engine.board_info();
    if info.name.trim().is_empty() {
        info.name = wire_name.to_string();
    }
    info
}

/// One engine step, run on the blocking pool.
///
/// `Engine::step` is CPU-bound solver work of UNBOUNDED duration: a board
/// whose analog solve is struggling grinds the retry ladder for minutes per
/// step. Run inline on a runtime worker, such a step doesn't just occupy one
/// worker: the worker that currently holds tokio's I/O + timer driver never
/// parks, so no thread polls the reactor and the WHOLE server stops answering
/// (measured on the lily58 corpus board: every page load and websocket timed
/// out while 7 of 8 workers sat idle). The blocking pool is where this work
/// belongs; it also puts an await point under the step, so a session
/// replacement's `task.abort()` ENDS THE SESSION mid-step (sockets close, the
/// new session takes over) instead of waiting the step out. The orphaned
/// blocking task itself cannot be interrupted: it runs its current step to
/// completion in the background and drops the engine when it returns, so a
/// replaced diverging solve costs at most one in-flight step of background
/// CPU, exactly the bound the old inline form had, without its wedge.
///
/// Returns `None` when the step panicked (the engine is gone); the caller
/// must end the session honestly.
async fn step_on_blocking_pool(
    engine: Box<dyn Engine>,
    dt: f64,
) -> Option<(Box<dyn Engine>, SimFrame)> {
    tokio::task::spawn_blocking(move || {
        let mut engine = engine;
        let frame = engine.step(dt);
        (engine, frame)
    })
    .await
    .ok()
}

/// The honest end of a session whose engine panicked mid-step.
const ENGINE_PANIC_REASON: &str = "the simulation engine crashed while stepping; this session \
     has ended (relaunch the board to start a new one)";

/// Terminal-failure bookkeeping: put the reason on the wire for anyone
/// listening NOW, and into the session backlog for anyone who connects (or
/// reloads) LATER; the broadcast alone is dropped when no receiver is
/// subscribed at that instant, which is exactly when a page reload is in
/// flight.
fn end_session_fatally(shared: &Shared, broadcast: &impl Fn(&ServerMessage), reason: &str) {
    shared.backlog.lock().expect("backlog lock").fatal = Some(reason.to_string());
    broadcast(&ServerMessage::Error {
        message: reason.to_string(),
    });
}

async fn sim_loop(
    mut engine: Box<dyn Engine>,
    wire_name: String,
    tx: broadcast::Sender<String>,
    mut cmd_rx: mpsc::Receiver<ClientMessage>,
    shared: Arc<Shared>,
) {
    let mut running = false;
    let mut speed = 1.0f64;
    // Latched once the analog solve is declared irrecoverable. While set,
    // Play/Step refuse (re-broadcasting the reason) instead of launching
    // another unbounded solve of a circuit already known to be dead; Reset
    // clears it together with the engine's own failure streak.
    let mut fatal: Option<String> = None;
    // Last simulation time seen from the engine, so the Status broadcast after
    // a command reports the real clock instead of resetting the UI to 0.0.
    let mut sim_time = 0.0f64;
    let frame_dt = 1.0 / FRAME_RATE_HZ;
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs_f64(frame_dt));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Honest rate accounting: what the loop DELIVERS (rolling window) and the
    // sustainable ceiling it paces to, kept strictly apart from `speed` (what
    // the user ASKED for). Cleared on pause/reset so idle wall time never
    // counts against the achieved rate. The wall axis is this loop's start.
    let mut meter = crate::rate::RateMeter::new();
    let loop_started = std::time::Instant::now();

    let broadcast_msg = |msg: &ServerMessage| {
        if tx.receiver_count() > 0 {
            let _ = tx.send(serde_json::to_string(msg).unwrap());
        }
    };

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                if running {
                    // Pace at the measured sustainable ceiling when the
                    // requested factor exceeds it: one honest small step per
                    // tick keeps frames flowing and commands responsive, where
                    // an oversized step would block this loop for its full
                    // solve time and still not deliver the requested rate.
                    let (paced, rate_limited) = meter.paced_factor(speed);
                    // Never shrink a step below the engine's own floor: for an
                    // external emulator the per-step round-trip costs the same
                    // whatever the step buys, so a smaller step buys less at
                    // the same price and the pacer's own measurement then
                    // reports a still-worse cost. That feedback drove the
                    // ESP32 live sim to the pacer's minimum factor and 0.0008x
                    // realtime. Below the floor the loop simply takes longer
                    // than one tick per step, which is honest: frames arrive
                    // slower, and each one carries real simulated time.
                    let step_dt = (frame_dt * paced).max(engine.min_step_dt());
                    let step_started = std::time::Instant::now();
                    // On the blocking pool, never inline: an inline step of
                    // unbounded solve time wedges the runtime's I/O driver and
                    // with it the whole server (see `step_on_blocking_pool`).
                    let Some((stepped, mut frame)) =
                        step_on_blocking_pool(engine, step_dt).await
                    else {
                        end_session_fatally(&shared, &broadcast_msg, ENGINE_PANIC_REASON);
                        return;
                    };
                    engine = stepped;
                    // Account the DELIVERED sim time, not the requested step:
                    // a live step that ended early at the dead-solve abort
                    // streak delivered a fraction of `step_dt`, and crediting
                    // the request would overstate the terminal frame's rate.
                    let delivered_dt = (frame.t - sim_time).max(0.0);
                    meter.record(
                        loop_started.elapsed().as_secs_f64(),
                        frame.t,
                        step_started.elapsed().as_secs_f64(),
                        delivered_dt,
                    );
                    // The wire carries BOTH numbers: the measured achieved
                    // rate (clamped to the paced factor: tick pacing bounds
                    // delivery, and the clamp also covers the first fraction
                    // of a second before the window can measure) and the
                    // user's requested factor, so no UI has to conflate them.
                    frame.realtime_factor =
                        meter.achieved().unwrap_or(f64::INFINITY).min(paced);
                    frame.requested_factor = speed;
                    frame.rate_limited = rate_limited;
                    sim_time = frame.t;
                    // Record BEFORE broadcasting (and regardless of receiver
                    // count): a fault raised while nobody is connected must
                    // still be in the backlog a later subscriber replays.
                    shared.record_faults(&frame);
                    broadcast_msg(&ServerMessage::SimFrame(frame));
                    // An irrecoverably failing analog solve must STOP this
                    // session with the reason on the wire. Left running, it
                    // grinds the dead solve at 100% CPU forever while the
                    // client watches a clock that never advances; that is the
                    // dishonest outcome. Pause (Reset clears the streak, a
                    // relaunch replaces the session), and say why.
                    if let Some(reason) = engine.analog_failure() {
                        running = false;
                        meter.clear();
                        let message = format!(
                            "live simulation stopped: {reason}. Press reset to \
                             retry, or fix the board/model and relaunch."
                        );
                        fatal = Some(message.clone());
                        end_session_fatally(&shared, &broadcast_msg, &message);
                    }
                    broadcast_msg(&ServerMessage::Status(Status {
                        running, sim_time, requested_factor: speed,
                        options: engine.controls(),
                    }));
                }
            }
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { return };
                match cmd {
                    // Play and Pause both restart the rate window: wall time
                    // spent paused must never count against the achieved rate,
                    // and stale pre-pause samples must not shape the first
                    // post-resume ceiling.
                    ClientMessage::Play => {
                        if let Some(reason) = &fatal {
                            broadcast_msg(&ServerMessage::Error {
                                message: reason.clone(),
                            });
                        } else {
                            running = true;
                            meter.clear();
                        }
                    }
                    ClientMessage::Pause => {
                        running = false;
                        meter.clear();
                    }
                    ClientMessage::Step { dt } if fatal.is_none() => {
                        // Clamp the client-supplied step like SetSpeed clamps
                        // factor: an unbounded dt (e.g. `1e9`) is ~1e13 chunks
                        // that the single sim_loop task runs synchronously,
                        // wedging every client until restart. A manual step is
                        // milliseconds; 1 s is already a generous ceiling.
                        let step_dt = if dt > 0.0 { dt.min(1.0) } else { frame_dt };
                        let step_started = std::time::Instant::now();
                        // Blocking pool for the same reason as the ticker's
                        // step: solve time is unbounded and must never sit on
                        // a runtime worker (see `step_on_blocking_pool`).
                        let Some((stepped, mut frame)) =
                            step_on_blocking_pool(engine, step_dt).await
                        else {
                            end_session_fatally(&shared, &broadcast_msg, ENGINE_PANIC_REASON);
                            return;
                        };
                        engine = stepped;
                        // A manual step has no continuous rate to report; the
                        // honest per-step number is what THIS step delivered
                        // (sim seconds per wall second of the solve), which is
                        // NOT the requested dt when the step ended early at
                        // the dead-solve abort streak. It does not feed the
                        // pacing meter: the sim is paused.
                        let step_wall = step_started.elapsed().as_secs_f64();
                        let delivered_dt = (frame.t - sim_time).max(0.0);
                        frame.realtime_factor = if step_wall > 0.0 {
                            delivered_dt / step_wall
                        } else {
                            0.0
                        };
                        frame.requested_factor = speed;
                        sim_time = frame.t;
                        shared.record_faults(&frame);
                        broadcast_msg(&ServerMessage::SimFrame(frame));
                        // Same honesty as the ticker path: a manual step of a
                        // dead solve reports why nothing will ever advance.
                        if let Some(reason) = engine.analog_failure() {
                            running = false;
                            meter.clear();
                            let message = format!(
                                "live simulation stopped: {reason}. Press reset to \
                                 retry, or fix the board/model and relaunch."
                            );
                            fatal = Some(message.clone());
                            end_session_fatally(&shared, &broadcast_msg, &message);
                        }
                    }
                    ClientMessage::Step { .. } => {
                        // Fatal latch: stepping a solve already declared dead
                        // just re-grinds it; repeat the reason instead.
                        if let Some(reason) = &fatal {
                            broadcast_msg(&ServerMessage::Error {
                                message: reason.clone(),
                            });
                        }
                    }
                    ClientMessage::Reset => {
                        engine.reset();
                        running = false;
                        sim_time = 0.0;
                        meter.clear();
                        // The engine's failure streak was cleared by reset();
                        // clear the session's latch with it so Play works.
                        fatal = None;
                        // A reset starts the fault story over server-side too,
                        // or the next subscriber would replay pre-reset faults
                        // as if they belonged to the fresh run. Same for the
                        // terminal-failure marker: `engine.reset()` cleared the
                        // abort streak, so the failure story starts over.
                        let cleared = {
                            let mut backlog = shared.backlog.lock().expect("backlog lock");
                            backlog.faults.clear();
                            backlog.fatal = None;
                            backlog.clone()
                        };
                        // Broadcast the refreshed (empty) backlog: every
                        // connected client lifts its failure banner on THIS
                        // acknowledgement, not on optimistically having sent
                        // a Reset a dead loop may never process.
                        broadcast_msg(&ServerMessage::Backlog(cleared));
                    }
                    ClientMessage::SetSpeed { factor } => {
                        speed = factor.clamp(0.001, 1000.0);
                    }
                    ClientMessage::SetControls(c) => engine.set_controls(c),
                    ClientMessage::Serial { mcu, data } => engine.serial(&mcu, &data),
                    ClientMessage::SetInput { source, value } => {
                        // Route to a bound input source first; if nothing
                        // matched, fall back to a peripheral of that id so a
                        // frontend slider wired to a peripheral works as-is.
                        engine.set_input(&source, value);
                        engine.set_peripheral(&source, value);
                    }
                    ClientMessage::SetPowerSupply { net, supply } => {
                        engine.set_power_supply(&net, supply);
                    }
                    ClientMessage::SetPeripheral { id, value } => {
                        engine.set_peripheral(&id, value);
                    }
                    ClientMessage::AttachPeripheral(spec) => {
                        let id = spec.id.clone();
                        match engine.attach_peripheral(spec) {
                        Ok(()) => {
                            // The control list is part of the live session
                            // contract. Refresh it immediately so the newly
                            // attached button/stimulus appears in the rail and
                            // can be manipulated without a reconnect.
                            broadcast_msg(&ServerMessage::BoardInfo(engine.board_info()));
                            broadcast_msg(&ServerMessage::ActionResult {
                                action: "attach_peripheral".into(),
                                id: id.clone(),
                                request_id: None,
                                ok: true,
                                message: format!("Attached {id} to the live circuit."),
                            });
                        }
                        Err(message) => broadcast_msg(&ServerMessage::ActionResult {
                            action: "attach_peripheral".into(),
                            id,
                            request_id: None,
                            ok: false,
                            message,
                        }),
                    }
                    }
                    ClientMessage::AttachRegisterMap(spec) => {
                        let id = spec.id.clone();
                        let request_id = spec.request_id;
                        match engine.attach_register_map(spec) {
                            Ok(()) => {
                                broadcast_msg(&ServerMessage::BoardInfo(engine.board_info()));
                                broadcast_msg(&ServerMessage::ActionResult {
                                    action: "attach_register_map".into(),
                                    id: id.clone(),
                                    request_id,
                                    ok: true,
                                    message: format!(
                                        "Attached exact register-map bytes for {id} to the live co-simulation."
                                    ),
                                });
                            }
                            Err(message) => broadcast_msg(&ServerMessage::ActionResult {
                                action: "attach_register_map".into(),
                                id,
                                request_id,
                                ok: false,
                                message,
                            }),
                        }
                    }
                    // Probe DATA is client-derived from the frame stream, but
                    // the active probe SET is session state: holding it here
                    // lets a rejoining client restore its probes from the
                    // backlog instead of losing them on reload.
                    ClientMessage::AddProbe { net } => {
                        let mut backlog = shared.backlog.lock().expect("backlog lock");
                        // Bounded because `net` is an arbitrary client string and
                        // the whole list is replayed to every new subscriber, so an
                        // uncapped list is both unbounded memory and unbounded work
                        // per reconnect. A board with more nets than this does not
                        // exist, so the cap only ever bites on a client sending
                        // rubbish.
                        const MAX_PROBES: usize = 512;
                        if backlog.probes.len() >= MAX_PROBES {
                            continue;
                        }
                        if net.len() > 256 {
                            continue;
                        }
                        if !backlog.probes.contains(&net) {
                            backlog.probes.push(net);
                        }
                    }
                    ClientMessage::RemoveProbe { net } => {
                        shared
                            .backlog
                            .lock()
                            .expect("backlog lock")
                            .probes
                            .retain(|p| p != &net);
                    }
                    ClientMessage::LoadBoard { .. } => {
                        // Wired up with the full engine integration.
                    }
                }
                let info = serde_json::to_string(
                    &ServerMessage::BoardInfo(named_board_info(&*engine, &wire_name)),
                ).unwrap();
                *shared.board_info_json.lock().await = info;
                broadcast_msg(&ServerMessage::Status(Status {
                    running,
                    sim_time,
                    requested_factor: speed,
                    options: engine.controls(),
                }));
            }
        }
    }
}
