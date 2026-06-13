//! Front-door HTTP test: boot the upload router with a stub analyzer (so the
//! server crate test stays independent of the engine crate), then drive it over
//! a real TCP socket — GET `/` returns the page, POST `/api/analyze` runs the
//! analyzer on the uploaded bytes and echoes the filename header back.

use std::sync::Arc;

use hauksbee_server::frontdoor;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Read a whole HTTP/1.1 response off the socket (until the server closes it or
/// we have read the declared Content-Length). Good enough for these small bodies.
async fn http(addr: std::net::SocketAddr, request: &str, body: &[u8]) -> String {
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

async fn spawn() -> std::net::SocketAddr {
    // Stub analyzer: proves the bytes and filename reach the callback, and
    // returns a JSON shape the real engine analyzer also returns.
    let analyze: frontdoor::Analyzer = Arc::new(|name: &str, contents: &str| {
        format!(
            "{{\"ok\":true,\"file_name\":\"{name}\",\"len\":{}}}",
            contents.len()
        )
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = frontdoor::router(analyze);
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    addr
}

#[tokio::test]
async fn index_serves_the_upload_page() {
    let addr = spawn().await;
    let resp = http(addr, "GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n", &[]).await;
    assert!(resp.contains("200 OK"), "index should 200: {resp:.120}");
    assert!(resp.contains("check your board"), "page heading missing");
    assert!(resp.contains("/api/analyze"), "page should post to the analyze endpoint");
}

#[tokio::test]
async fn analyze_runs_the_callback_on_uploaded_bytes() {
    let addr = spawn().await;
    let body = b"(kicad_pcb tiny)";
    let req = format!(
        "POST /api/analyze HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\
         X-Board-Filename: tiny.kicad_pcb\r\nContent-Type: application/octet-stream\r\n\
         Content-Length: {}\r\n\r\n",
        body.len()
    );
    let resp = http(addr, &req, body).await;
    assert!(resp.contains("200 OK"), "analyze should 200: {resp:.160}");
    assert!(resp.contains("application/json"), "should return JSON");
    // The filename header reached the analyzer and the body length is correct.
    assert!(resp.contains("\"file_name\":\"tiny.kicad_pcb\""), "filename not threaded through: {resp}");
    assert!(resp.contains(&format!("\"len\":{}", body.len())), "body bytes not threaded through");
}

#[tokio::test]
async fn analyze_without_filename_header_defaults_gracefully() {
    let addr = spawn().await;
    let body = b"x";
    let req = format!(
        "POST /api/analyze HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\
         Content-Type: application/octet-stream\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    let resp = http(addr, &req, body).await;
    assert!(resp.contains("200 OK"));
    assert!(resp.contains("\"file_name\":\"board\""), "should default the filename");
}
