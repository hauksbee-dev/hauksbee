//! Front-door API HTTP test: boot the analysis routes with a stub analyzer (so
//! the server crate test stays independent of the engine crate), then drive them
//! over a real TCP socket, POST `/api/analyze` runs the analyzer on the uploaded
//! bytes and echoes the filename header back; `/api/analyze-with-firmware` threads
//! board + firmware parts through verbatim. There is no server-rendered page
//! (W6 §1): the React bundle owns `/` in the unified server router.

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
    let analyze: frontdoor::Analyzer = Arc::new(|name: &str, contents: &[u8]| {
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

// (W6 §1) The server-rendered HTML front door is gone: the one web experience
// is the React bundle in `frontend/dist`, and this crate now exposes only the
// JSON analysis API it fetches. There is no GET `/` here anymore; the static
// bundle owns `/` in the unified server router.

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
    assert!(
        resp.contains("\"file_name\":\"tiny.kicad_pcb\""),
        "filename not threaded through: {resp}"
    );
    assert!(
        resp.contains(&format!("\"len\":{}", body.len())),
        "body bytes not threaded through"
    );
}

#[tokio::test]
async fn analyze_passes_binary_board_bytes_verbatim() {
    // Regression (web bytes fix): the handler used to lossy-UTF8-decode the
    // body before calling the analyzer, so a binary board (Altium .PcbDoc, an
    // OLE2 container) was corrupted before it was ever parsed, each invalid
    // byte below would have ballooned into a 3-byte U+FFFD. The exact byte
    // count echoed back proves the raw bytes now reach the analyzer intact.
    let addr = spawn().await;
    let body: &[u8] = &[0xD0, 0xCF, 0x11, 0xE0, 0x00, 0xff, 0x42]; // OLE2 magic + junk
    assert_ne!(
        String::from_utf8_lossy(body).len(),
        body.len(),
        "the fixture must be one a lossy decode would corrupt"
    );
    let req = format!(
        "POST /api/analyze HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\
         X-Board-Filename: board.PcbDoc\r\nContent-Type: application/octet-stream\r\n\
         Content-Length: {}\r\n\r\n",
        body.len()
    );
    let resp = http(addr, &req, body).await;
    assert!(resp.contains("200 OK"), "analyze should 200: {resp:.160}");
    assert!(
        resp.contains(&format!("\"len\":{}", body.len())),
        "board bytes must reach the analyzer verbatim (no lossy decode): {resp}"
    );
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
    assert!(
        resp.contains("\"file_name\":\"board\""),
        "should default the filename"
    );
}

/// Spawn the firmware-aware router with a stub FirmwareAnalyzer that echoes back
/// what it received (board name, board length, firmware name + length, or "none").
async fn spawn_fw() -> std::net::SocketAddr {
    let analyze: frontdoor::FirmwareAnalyzer = Arc::new(
        |name: &str, contents: &[u8], fw: Option<(&str, &[u8])>| {
            match fw {
            Some((fw_name, fw_bytes)) => format!(
                "{{\"ok\":true,\"file_name\":\"{name}\",\"len\":{},\"fw_name\":\"{fw_name}\",\"fw_len\":{}}}",
                contents.len(),
                fw_bytes.len()
            ),
            None => format!(
                "{{\"ok\":true,\"file_name\":\"{name}\",\"len\":{},\"fw\":\"none\"}}",
                contents.len()
            ),
        }
        },
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = frontdoor::router_with_firmware(analyze);
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    addr
}

async fn spawn_schematic() -> std::net::SocketAddr {
    let analyze: frontdoor::SchematicAnalyzer = Arc::new(
        |name: &str,
         contents: &[u8],
         fw: Option<(&str, &[u8])>,
         schematic: Option<(&str, &[u8])>| {
            let (schematic_name, schematic_len) = schematic
                .map(|(name, bytes)| (name, bytes.len()))
                .unwrap_or(("none", 0));
            format!(
                "{{\"ok\":true,\"file_name\":\"{name}\",\"len\":{},\"fw\":{},\"schematic_name\":\"{schematic_name}\",\"schematic_len\":{schematic_len}}}",
                contents.len(),
                fw.is_some()
            )
        },
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = frontdoor::api_routes_with_schematic(analyze);
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    addr
}

/// Build a minimal multipart/form-data body for the given parts.
/// Each part is (field_name, filename, raw_bytes).
fn multipart_body(boundary: &str, parts: &[(&str, &str, &[u8])]) -> Vec<u8> {
    let mut body = Vec::new();
    for (name, filename, data) in parts {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!(
                "Content-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\"\r\n\
                 Content-Type: application/octet-stream\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(data);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    body
}

#[tokio::test]
async fn firmware_router_still_serves_board_only_analyze() {
    // The firmware-aware router keeps /api/analyze working (board-only path).
    let addr = spawn_fw().await;
    let body = b"(kicad_pcb tiny)";
    let req = format!(
        "POST /api/analyze HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\
         X-Board-Filename: tiny.kicad_pcb\r\nContent-Type: application/octet-stream\r\n\
         Content-Length: {}\r\n\r\n",
        body.len()
    );
    let resp = http(addr, &req, body).await;
    assert!(
        resp.contains("200 OK"),
        "board-only should 200: {resp:.160}"
    );
    assert!(
        resp.contains("\"fw\":\"none\""),
        "board-only path passes no firmware"
    );
}

#[tokio::test]
async fn analyze_with_firmware_threads_board_and_firmware_bytes() {
    let addr = spawn_fw().await;
    let boundary = "----hauksbeetestboundary";
    // Firmware bytes deliberately include a null + high byte to prove they are
    // NOT lossy-UTF8-decoded (which would change the byte count / content).
    let fw: &[u8] = &[0x7f, b'E', b'L', b'F', 0x00, 0xff, 0x42];
    let board = b"(kicad_pcb tiny board)";
    let body = multipart_body(
        boundary,
        &[
            ("board", "tiny.kicad_pcb", board),
            ("firmware", "drone.elf", fw),
        ],
    );
    let req = format!(
        "POST /api/analyze-with-firmware HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\
         Content-Type: multipart/form-data; boundary={boundary}\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    let resp = http(addr, &req, &body).await;
    assert!(resp.contains("200 OK"), "should 200: {resp:.200}");
    assert!(
        resp.contains("\"file_name\":\"tiny.kicad_pcb\""),
        "board name threaded: {resp}"
    );
    assert!(
        resp.contains("\"fw_name\":\"drone.elf\""),
        "fw name threaded: {resp}"
    );
    // Exact byte count proves the firmware bytes survived intact (no UTF-8 lossy).
    assert!(
        resp.contains(&format!("\"fw_len\":{}", fw.len())),
        "firmware bytes must reach the analyzer verbatim: {resp}"
    );
}

#[tokio::test]
async fn analyze_threads_optional_eagle_schematic_bytes_to_the_analyzer() {
    let addr = spawn_schematic().await;
    let boundary = "----hauksbee-schematic-boundary";
    let board = b"<eagle><drawing><board/></drawing></eagle>";
    let schematic = b"<eagle><drawing><schematic/></drawing></eagle>";
    let body = multipart_body(
        boundary,
        &[
            ("board", "design.brd", board),
            ("schematic", "design.sch", schematic),
        ],
    );
    let req = format!(
        "POST /api/analyze-with-firmware HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\
         Content-Type: multipart/form-data; boundary={boundary}\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    let resp = http(addr, &req, &body).await;
    assert!(resp.contains("200 OK"), "should 200: {resp:.200}");
    assert!(resp.contains("\"file_name\":\"design.brd\""), "{resp}");
    assert!(resp.contains("\"schematic_name\":\"design.sch\""), "{resp}");
    assert!(
        resp.contains(&format!("\"schematic_len\":{}", schematic.len())),
        "schematic bytes were not threaded verbatim: {resp}"
    );
}

#[tokio::test]
async fn live_launch_threads_optional_eagle_schematic_to_the_launcher() {
    let launch: frontdoor::SchematicLiveLauncher =
        Arc::new(|_name, _board, _firmware, schematic| match schematic {
            Some((name, bytes)) => Err(format!("saw {name} with {} bytes", bytes.len())),
            None => Err("schematic missing".to_string()),
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = frontdoor::live_routes_with_schematic(hauksbee_server::LiveHub::new(), launch);
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let boundary = "----hauksbee-live-schematic-boundary";
    let schematic = b"<eagle><drawing><schematic/></drawing></eagle>";
    let body = multipart_body(
        boundary,
        &[
            (
                "board",
                "design.brd",
                b"<eagle><drawing><board/></drawing></eagle>",
            ),
            ("schematic", "design.sch", schematic),
        ],
    );
    let request = format!(
        "POST /api/live/launch HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\
         Content-Type: multipart/form-data; boundary={boundary}\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    let response = http(addr, &request, &body).await;
    assert!(response.contains("saw design.sch"), "{response}");
    assert!(
        response.contains(&format!("with {} bytes", schematic.len())),
        "{response}"
    );
}

#[tokio::test]
async fn served_html_is_no_cache_but_hashed_assets_are_not() {
    // frontend/dist is a gitignored build artifact served under stable names;
    // a cached index.html kept pointing at old asset hashes after a rebuild
    // and resurrected "already fixed" bugs. The entry HTML must always
    // revalidate; the hash-named assets need no such marking.
    let dir = std::env::temp_dir().join(format!("hauksbee-cache-test-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("assets")).unwrap();
    std::fs::write(dir.join("index.html"), "<!doctype html><title>x</title>").unwrap();
    std::fs::write(dir.join("assets/app-abc123.js"), "console.log(1)").unwrap();

    let analyze: frontdoor::FirmwareAnalyzer =
        Arc::new(|_: &str, _: &[u8], _: Option<(&str, &[u8])>| "{\"ok\":true}".to_string());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let dir_clone = dir.clone();
    tokio::spawn(async move {
        hauksbee_server::serve_frontdoor_on(
            listener,
            Some(&dir_clone),
            analyze,
            None,
            None,
            None,
            "{\"preloaded\":false}".to_string(),
        )
        .await
        .unwrap();
    });

    let html = http(
        addr,
        "GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        b"",
    )
    .await;
    assert!(html.contains("200 OK"), "index should 200: {html:.160}");
    let html_lower = html.to_ascii_lowercase();
    assert!(
        html_lower.contains("cache-control: no-cache"),
        "index.html must be no-cache: {html:.400}"
    );

    let js = http(
        addr,
        "GET /assets/app-abc123.js HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        b"",
    )
    .await;
    assert!(js.contains("200 OK"), "asset should 200: {js:.160}");
    assert!(
        !js.to_ascii_lowercase().contains("cache-control: no-cache"),
        "hash-named assets must not be forced to revalidate: {js:.400}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn malformed_multipart_names_the_real_cause() {
    // Regression (swallowed multipart error): a body that never parses as
    // multipart used to fall out of the field loop silently and get reported as
    // the misleading "no board file in the upload". It must instead name the
    // malformed upload, mirroring the sibling read-error arm's JSON shape.
    let addr = spawn_fw().await;
    let boundary = "----hauksbeetestboundary3";
    // Declares a boundary that never appears: multer hits end-of-stream while
    // still searching for the first boundary and errors.
    let body = b"this is not a multipart body at all";
    let req = format!(
        "POST /api/analyze-with-firmware HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\
         Content-Type: multipart/form-data; boundary={boundary}\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    let resp = http(addr, &req, body).await;
    assert!(
        resp.contains("200 OK"),
        "error is carried in the JSON body: {resp:.200}"
    );
    assert!(
        resp.contains("\"ok\":false"),
        "a malformed upload must not report ok: {resp}"
    );
    assert!(
        resp.contains("malformed multipart upload"),
        "must name the malformed upload as the cause: {resp}"
    );
    assert!(
        !resp.contains("no board file in the upload"),
        "must not hide the parse error behind the missing-board message: {resp}"
    );
}

/// Spawn the dependency routes with stub hooks: status returns a fixed JSON,
/// the installer streams two lines then succeeds for "ok-dep" and fails with a
/// tail-bearing message for anything else.
async fn spawn_deps() -> std::net::SocketAddr {
    let status: frontdoor::DepsStatus =
        Arc::new(|| "{\"deps\":[{\"id\":\"stub\",\"present\":false}]}".to_string());
    let install: frontdoor::DepInstaller = Arc::new(|id, progress| {
        progress("step one");
        progress("step two");
        if id == "ok-dep" {
            Ok(())
        } else {
            Err(format!(
                "install of '{id}' failed. Last output:\nthe disk is full"
            ))
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = frontdoor::deps_routes(status, install);
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    addr
}

#[tokio::test]
async fn deps_status_relays_the_engine_json() {
    let addr = spawn_deps().await;
    let resp = http(
        addr,
        "GET /api/deps HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        b"",
    )
    .await;
    assert!(resp.contains("200 OK"), "{resp:.200}");
    assert!(resp.contains("application/json"), "{resp:.200}");
    assert!(
        resp.contains("\"id\":\"stub\""),
        "status JSON relayed: {resp}"
    );
}

#[tokio::test]
async fn deps_install_streams_progress_then_done() {
    let addr = spawn_deps().await;
    let resp = http(
        addr,
        "POST /api/deps/install/ok-dep HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\
         Content-Length: 0\r\n\r\n",
        b"",
    )
    .await;
    assert!(resp.contains("200 OK"), "{resp:.200}");
    assert!(
        resp.contains("text/event-stream"),
        "SSE content type: {resp:.300}"
    );
    assert!(
        resp.contains("data: step one"),
        "progress line one streamed: {resp}"
    );
    assert!(
        resp.contains("data: step two"),
        "progress line two streamed: {resp}"
    );
    assert!(resp.contains("event: done"), "terminal done event: {resp}");
    assert!(
        !resp.contains("event: error"),
        "no error on success: {resp}"
    );
}

#[tokio::test]
async fn deps_install_failure_carries_the_real_tail() {
    let addr = spawn_deps().await;
    let resp = http(
        addr,
        "POST /api/deps/install/renode HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\
         Content-Length: 0\r\n\r\n",
        b"",
    )
    .await;
    assert!(
        resp.contains("event: error"),
        "terminal error event: {resp}"
    );
    // The multi-line error survives SSE framing as consecutive data lines.
    assert!(
        resp.contains("data: the disk is full"),
        "the child's output tail reaches the browser: {resp}"
    );
}

#[tokio::test]
async fn deps_install_refuses_cross_site_requests() {
    // The install route runs an installer, so a drive-by POST from a hostile
    // page (Sec-Fetch-Site: cross-site, which page JS cannot forge) must be
    // refused before anything starts, same guard as the analyze/check routes.
    let addr = spawn_deps().await;
    let resp = http(
        addr,
        "POST /api/deps/install/ok-dep HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\
         Sec-Fetch-Site: cross-site\r\nContent-Length: 0\r\n\r\n",
        b"",
    )
    .await;
    assert!(
        resp.contains("403"),
        "cross-site install must 403: {resp:.300}"
    );
    assert!(
        !resp.contains("event: done") && !resp.contains("data: step one"),
        "the installer must not have run: {resp}"
    );
    // Same-origin page JS (Sec-Fetch-Site: same-origin) stays allowed.
    let ok = http(
        addr,
        "POST /api/deps/install/ok-dep HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\
         Sec-Fetch-Site: same-origin\r\nContent-Length: 0\r\n\r\n",
        b"",
    )
    .await;
    assert!(ok.contains("200 OK"), "same-origin must pass: {ok:.200}");
    assert!(ok.contains("event: done"), "{ok}");
}

#[tokio::test]
async fn analyze_with_firmware_without_firmware_part_falls_back() {
    let addr = spawn_fw().await;
    let boundary = "----hauksbeetestboundary2";
    let board = b"(kicad_pcb tiny board)";
    let body = multipart_body(boundary, &[("board", "tiny.kicad_pcb", board)]);
    let req = format!(
        "POST /api/analyze-with-firmware HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\
         Content-Type: multipart/form-data; boundary={boundary}\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    let resp = http(addr, &req, &body).await;
    assert!(resp.contains("200 OK"), "should 200: {resp:.200}");
    assert!(
        resp.contains("\"fw\":\"none\""),
        "missing firmware part must fall back to board-only: {resp}"
    );
}
