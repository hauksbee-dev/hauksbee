//! Websocket server streaming live simulation frames to the frontend and
//! routing user controls back to the engine.

pub mod engine;
pub mod protocol;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use engine::Engine;
use protocol::{ClientMessage, ServerMessage, Status};
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
        let board_info =
            serde_json::to_string(&ServerMessage::BoardInfo(engine.board_info()))
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
        let mut router = Router::new()
            .route("/ws", get(ws_handler))
            .with_state(self.shared.clone());
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
        let listener = tokio::net::TcpListener::bind(addr).await?;
        eprintln!("galvani-server listening on http://{addr}");
        axum::serve(listener, self.router(static_dir)).await?;
        Ok(())
    }
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(shared): State<Arc<Shared>>,
) -> impl IntoResponse {
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
    let mut ticker =
        tokio::time::interval(std::time::Duration::from_secs_f64(frame_dt));
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
                        engine.set_input(&source, value);
                    }
                    ClientMessage::SetPowerSupply { net, supply } => {
                        engine.set_power_supply(&net, supply);
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
