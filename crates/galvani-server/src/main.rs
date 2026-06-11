use galvani_server::engine::McuDemoEngine;
use galvani_server::Server;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let firmware = std::env::args().nth(1).map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/firmware/demo/demo.hex")
    });
    let engine = McuDemoEngine::new(&firmware, "demo", "/boards/demo.kicad_pcb")?;
    let server = Server::new(Box::new(engine));
    let static_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../frontend/dist");
    let dir = static_dir.exists().then_some(static_dir);
    server.serve("127.0.0.1:3001", dir.as_deref()).await
}
