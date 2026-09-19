//! Shared helpers for the crate's split integration-test files.

use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Bind an ephemeral TCP listener, spawn `router` serving on it in the
/// background, and return the address to connect to. Used by every test file
/// that boots a real router over a real socket (`frontdoor_http`,
/// `ws_roundtrip`), so the bind/spawn boilerplate lives in one place.
pub async fn spawn_router(router: axum::Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    addr
}

/// Read a whole HTTP/1.1 response off the socket (until the server closes it
/// or we have read the declared Content-Length). Good enough for the small
/// bodies these tests exchange. Shared by every test file that speaks plain
/// HTTP over a real socket (`frontdoor_http`'s whole suite, `ws_roundtrip`'s
/// board-probe check).
pub async fn http(addr: SocketAddr, request: &str, body: &[u8]) -> String {
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    if !body.is_empty() {
        stream.write_all(body).await.unwrap();
    }
    stream.flush().await.unwrap();
    let mut buf = Vec::new();
    // The handlers do not keep-alive aggressively under test; read to EOF.
    let _ = stream.read_to_end(&mut buf).await;
    String::from_utf8_lossy(&buf).into_owned()
}
