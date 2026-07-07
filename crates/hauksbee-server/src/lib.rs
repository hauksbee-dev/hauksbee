//! Websocket server streaming live simulation frames to the frontend and
//! routing user controls back to the engine.

pub mod engine;
pub mod frontdoor;
pub mod protocol;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::header;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use engine::Engine;
use frontdoor::FirmwareAnalyzer;
use protocol::{ClientMessage, ServerMessage, Status};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, Mutex};

const FRAME_RATE_HZ: f64 = 30.0;

struct Shared {
    tx: broadcast::Sender<String>,
    cmd: mpsc::Sender<ClientMessage>,
    board_info_json: Mutex<String>,
}

pub struct Server {
    shared: Arc<Shared>,
}

impl Server {
    /// Spawn the simulation loop around `engine` and return the server.
    pub fn new(engine: Box<dyn Engine>) -> Server {
        let (tx, _) = broadcast::channel::<String>(256);
        let (cmd_tx, cmd_rx) = mpsc::channel::<ClientMessage>(64);
        let board_info = serde_json::to_string(&ServerMessage::BoardInfo(engine.board_info()))
            .expect("board info serializes");
        let shared = Arc::new(Shared {
            tx: tx.clone(),
            cmd: cmd_tx,
            board_info_json: Mutex::new(board_info),
        });
        tokio::spawn(sim_loop(engine, tx, cmd_rx, shared.clone()));
        Server { shared }
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
            .with_state(self.shared.clone());
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
        router
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

    /// The unified web app router (W6 §1): one server path serving the static
    /// React bundle (`static_dir`), the analysis API the React landing calls
    /// (`analyze` -> `/api/analyze` + `/api/analyze-with-firmware`), the
    /// `/api/startup` hint the app reads to choose its landing state, the live
    /// WebSocket sim (`/ws`), and — when a board was preloaded (`run --serve`) —
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
        startup_json: String,
    ) -> Router {
        unified_router(
            Some(self.shared.clone()),
            static_dir,
            board_file,
            Some(analyze),
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
        startup_json: String,
    ) -> anyhow::Result<()> {
        let listener = bind_with_fallback(addr).await?;
        let router = self.app_router(static_dir, board_file, analyze, startup_json);
        axum::serve(listener, router).await?;
        Ok(())
    }
}

/// Assemble the unified router from its optional parts. `shared` is present only
/// when a live engine is available (`run --serve`); `serve` (drop-zone-only, no
/// preloaded board) passes `None` and simply gets no `/ws` route — the React
/// landing never opens the socket until the user presses "run it".
fn unified_router(
    shared: Option<Arc<Shared>>,
    static_dir: Option<&Path>,
    board_file: Option<(String, String)>,
    analyze: Option<FirmwareAnalyzer>,
    startup_json: String,
) -> Router {
    let mut router = Router::new();
    if let Some(shared) = shared {
        router = router.merge(Router::new().route("/ws", get(ws_handler)).with_state(shared));
    }
    if let Some(analyze) = analyze {
        router = router.merge(frontdoor::api_routes(analyze));
    }
    // `/api/startup`: the frontend reads this once on load to decide whether to
    // show the drop zone (serve) or a preloaded board's report (run --serve).
    router = router.route(
        "/api/startup",
        get(move || {
            let body = startup_json.clone();
            async move {
                (
                    [(header::CONTENT_TYPE, "application/json")],
                    body,
                )
            }
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
    router
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
    startup_json: String,
) -> anyhow::Result<()> {
    let listener = bind_with_fallback(addr).await?;
    let router = unified_router(None, static_dir, None, Some(analyze), startup_json);
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

async fn ws_handler(ws: WebSocketUpgrade, State(shared): State<Arc<Shared>>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, shared))
}

async fn handle_socket(mut socket: WebSocket, shared: Arc<Shared>) {
    let info = shared.board_info_json.lock().await.clone();
    if socket.send(Message::Text(info.into())).await.is_err() {
        return;
    }
    let mut rx = shared.tx.subscribe();
    loop {
        tokio::select! {
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

async fn sim_loop(
    mut engine: Box<dyn Engine>,
    tx: broadcast::Sender<String>,
    mut cmd_rx: mpsc::Receiver<ClientMessage>,
    shared: Arc<Shared>,
) {
    let mut running = false;
    let mut speed = 1.0f64;
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
                    let t = frame.t;
                    broadcast_msg(&ServerMessage::SimFrame(frame));
                    broadcast_msg(&ServerMessage::Status(Status {
                        running, sim_time: t, options: engine.controls(),
                    }));
                }
            }
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { return };
                match cmd {
                    ClientMessage::Play => running = true,
                    ClientMessage::Pause => running = false,
                    ClientMessage::Step { dt } => {
                        let frame = engine.step(if dt > 0.0 { dt } else { frame_dt });
                        broadcast_msg(&ServerMessage::SimFrame(frame));
                    }
                    ClientMessage::Reset => {
                        engine.reset();
                        running = false;
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
                    &ServerMessage::BoardInfo(engine.board_info()),
                ).unwrap();
                *shared.board_info_json.lock().await = info;
                broadcast_msg(&ServerMessage::Status(Status {
                    running,
                    sim_time: 0.0,
                    options: engine.controls(),
                }));
            }
        }
    }
}
