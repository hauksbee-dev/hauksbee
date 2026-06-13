use hauksbee_server::engine::McuDemoEngine;
use hauksbee_server::Server;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let arg = std::env::args().nth(1);
    // The demo server only knows the synthetic `McuDemoEngine`. Live board
    // co-simulation (extract → bind → solve + MCU + digital) lives behind the
    // `HauksbeeEngine` in the `hauksbee-engine` crate; pointing this binary at a
    // real board file would mean depending on that crate (which depends on this
    // one), so we direct the user to the dedicated `hauksbee` binary instead.
    if let Some(a) = &arg {
        let lower = a.to_ascii_lowercase();
        if lower.ends_with(".kicad_pcb") || lower.ends_with(".net") || lower.ends_with(".brd") {
            eprintln!(
                "hauksbee-server is the demo MCU server. To bring a real board to life run:\n  \
                 hauksbee run {a} [--firmware <hex>] [--port 3001]\n(the `hauksbee` binary is built \
                 from the hauksbee-engine crate)."
            );
            std::process::exit(2);
        }
    }
    let firmware = arg.map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/firmware/demo/demo.hex")
    });
    let engine = McuDemoEngine::new(&firmware, "demo", "/boards/demo.kicad_pcb")?;
    let server = Server::new(Box::new(engine));
    let static_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../frontend/dist");
    let dir = static_dir.exists().then_some(static_dir);
    server.serve("127.0.0.1:3001", dir.as_deref()).await
}
