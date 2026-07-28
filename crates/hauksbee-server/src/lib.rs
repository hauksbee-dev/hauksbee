//! The hauksbee web server: a WebSocket that streams live simulation frames to
//! the frontend and routes user controls back to the engine, plus the HTTP router
//! that serves the React bundle, the analysis API, and (when a board is preloaded)
//! that board's own file for the geometry viewer. [`Server`] owns the sim loop;
//! the free functions assemble the drop-zone and unified app routers.

pub mod engine;
pub mod frontdoor;
pub mod protocol;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path as UrlPath, State};
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use engine::Engine;
use frontdoor::{CheckRunner, DepInstaller, DepsStatus, FirmwareAnalyzer, LiveLauncher};
use protocol::{ClientMessage, ServerMessage, Status};
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
        deps: Option<(DepsStatus, DepInstaller)>,
        launch: Option<LiveLauncher>,
        startup_json: String,
    ) -> Router {
        unified_router(
            Some(self.hub.clone()),
            static_dir,
            board_file,
            Some(analyze),
            check,
            deps,
            launch,
            startup_json,
        )
    }

    /// Serve the unified app router (WebSocket sim included). Used by
    /// `hauksbee run --serve`, where a board is preloaded.
    #[allow(clippy::too_many_arguments)]
    pub async fn serve_app(
        &self,
        addr: &str,
        static_dir: Option<&Path>,
        board_file: Option<(String, String)>,
        analyze: FirmwareAnalyzer,
        check: Option<CheckRunner>,
        deps: Option<(DepsStatus, DepInstaller)>,
        launch: Option<LiveLauncher>,
        startup_json: String,
    ) -> anyhow::Result<()> {
        let listener = bind_with_fallback(addr).await?;
        let router = self.app_router(
            static_dir,
            board_file,
            analyze,
            check,
            deps,
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
    #[allow(clippy::too_many_arguments)]
    pub async fn serve_app_on(
        &self,
        listener: tokio::net::TcpListener,
        static_dir: Option<&Path>,
        board_file: Option<(String, String)>,
        analyze: FirmwareAnalyzer,
        check: Option<CheckRunner>,
        deps: Option<(DepsStatus, DepInstaller)>,
        launch: Option<LiveLauncher>,
        startup_json: String,
    ) -> anyhow::Result<()> {
        let router = self.app_router(
            static_dir,
            board_file,
            analyze,
            check,
            deps,
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
#[allow(clippy::too_many_arguments)]
fn unified_router(
    hub: Option<Arc<LiveHub>>,
    static_dir: Option<&Path>,
    board_file: Option<(String, String)>,
    analyze: Option<FirmwareAnalyzer>,
    check: Option<CheckRunner>,
    deps: Option<(DepsStatus, DepInstaller)>,
    launch: Option<LiveLauncher>,
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
    }
    if let (Some(hub), Some(launch)) = (&hub, launch) {
        router = router.merge(frontdoor::live_routes(hub.clone(), launch));
    }
    if let Some(analyze) = analyze {
        router = router.merge(frontdoor::api_routes(analyze));
    }
    // The web checks panel's backend (`POST /api/check`): present whenever the
    // embedding binary supplied a runner (the hauksbee-ci shell-out).
    if let Some(check) = check {
        router = router.merge(frontdoor::check_route(check));
    }
    // The dependency panel's backend (`GET /api/deps`, `POST
    // /api/deps/install/{id}`): present whenever the embedding binary supplied
    // the engine's probe + installer hooks.
    if let Some((status, install)) = deps {
        router = router.merge(frontdoor::deps_routes(status, install));
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
    deps: Option<(DepsStatus, DepInstaller)>,
    startup_json: String,
) -> anyhow::Result<()> {
    let (listener, _bound) = bind_frontdoor(addr).await?;
    serve_frontdoor_on(
        listener,
        static_dir,
        analyze,
        check,
        deps,
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
    deps: Option<(DepsStatus, DepInstaller)>,
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
        check,
        deps,
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
        Err(e) => Err(e.into()),
    }
}

async fn ws_handler(ws: WebSocketUpgrade, State(hub): State<Arc<LiveHub>>) -> impl IntoResponse {
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

/// GET `/boards/{name}`: the CURRENT live session's own board file, for the
/// geometry viewer. 404 for anything but the launched board's exact name.
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
        None => (StatusCode::NOT_FOUND, "no such live board").into_response(),
    }
}

async fn handle_socket(mut socket: WebSocket, shared: Arc<Shared>) {
    let info = shared.board_info_json.lock().await.clone();
    if socket.send(Message::Text(info.into())).await.is_err() {
        return;
    }
    let mut rx = shared.tx.subscribe();
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

/// The engine's BoardInfo with the session's wire name applied (see the
/// fallback rationale in [`LiveHub::launch`]).
fn named_board_info(engine: &dyn Engine, wire_name: &str) -> protocol::BoardInfo {
    let mut info = engine.board_info();
    if info.name.trim().is_empty() {
        info.name = wire_name.to_string();
    }
    info
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
    // Last simulation time seen from the engine, so the Status broadcast after
    // a command reports the real clock instead of resetting the UI to 0.0.
    let mut sim_time = 0.0f64;
    let frame_dt = 1.0 / FRAME_RATE_HZ;
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs_f64(frame_dt));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let broadcast_msg = |msg: &ServerMessage| {
        if tx.receiver_count() > 0 {
            let _ = tx.send(serde_json::to_string(msg).unwrap());
        }
    };

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                if running {
                    let frame = engine.step(frame_dt * speed);
                    sim_time = frame.t;
                    broadcast_msg(&ServerMessage::SimFrame(frame));
                    broadcast_msg(&ServerMessage::Status(Status {
                        running, sim_time, options: engine.controls(),
                    }));
                }
            }
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { return };
                match cmd {
                    ClientMessage::Play => running = true,
                    ClientMessage::Pause => running = false,
                    ClientMessage::Step { dt } => {
                        // Clamp the client-supplied step like SetSpeed clamps
                        // factor: an unbounded dt (e.g. `1e9`) is ~1e13 chunks
                        // that the single sim_loop task runs synchronously,
                        // wedging every client until restart. A manual step is
                        // milliseconds; 1 s is already a generous ceiling.
                        let step_dt = if dt > 0.0 { dt.min(1.0) } else { frame_dt };
                        let frame = engine.step(step_dt);
                        sim_time = frame.t;
                        broadcast_msg(&ServerMessage::SimFrame(frame));
                    }
                    ClientMessage::Reset => {
                        engine.reset();
                        running = false;
                        sim_time = 0.0;
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
                    ClientMessage::LoadBoard { .. }
                    | ClientMessage::AddProbe { .. }
                    | ClientMessage::RemoveProbe { .. } => {
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
                    options: engine.controls(),
                }));
            }
        }
    }
}
