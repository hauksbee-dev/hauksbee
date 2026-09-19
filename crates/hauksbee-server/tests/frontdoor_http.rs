//! Front-door API HTTP test: boot the design routes with stub callbacks (so the
//! server crate test stays independent of the engine crate), then drive them
//! over a real TCP socket. `POST /api/analyze` runs the analyzer on the raw
//! uploaded bytes and echoes the filename header back;
//! `/api/analyze-with-firmware` threads every multipart companion through
//! verbatim; `/api/sim` relays the deck simulator's JSON. There is no
//! server-rendered page: the React bundle owns `/` in the unified router.

use std::sync::Arc;

use hauksbee_server::frontdoor;

use crate::support;

/// Serve the design routes with stub callbacks: the analyzer echoes back every
/// part it received (proving the bytes and names reach it verbatim), and the
/// deck simulator echoes the deck unless it is asked to refuse.
async fn spawn() -> std::net::SocketAddr {
    let analyze: frontdoor::DesignAnalyzer = Arc::new(|upload| {
        let named = |file: Option<frontdoor::NamedUpload>| {
            file.map(|f| serde_json::json!({ "name": f.name, "len": f.bytes.len() }))
        };
        serde_json::json!({
            "ok": true,
            "file_name": upload.board.name,
            "len": upload.board.bytes.len(),
            "fw": named(upload.firmware),
            "schematic": named(upload.schematic),
            "bom": upload.bom.map(|f| f.name),
            "placement": upload.placement.map(|f| f.name),
            "variant": upload.variant.map(|f| f.name),
            "asbuilt": upload.asbuilt.map(|f| f.name),
            "models": upload.models.into_iter().map(|f| f.name).collect::<Vec<_>>(),
        })
        .to_string()
    });
    let sim: frontdoor::DeckSimulator = Arc::new(|name: &str, deck: &str| {
        if deck.contains(".include") {
            return Err(format!("{name}: this deck pulls in another file"));
        }
        Ok(format!(
            "{{\"ok\":true,\"analysis\":\"op\",\"deck_len\":{}}}",
            deck.len()
        ))
    });
    support::spawn_router(frontdoor::api_routes_with_design(analyze, sim)).await
}

/// A `POST <path>` request head with the raw body content type.
fn raw_post(path: &str, extra: &str, len: usize) -> String {
    format!(
        "POST {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n{extra}\
         Content-Type: application/octet-stream\r\nContent-Length: {len}\r\n\r\n"
    )
}

/// A `POST <path>` request head for a multipart body under `boundary`.
fn multipart_post(path: &str, boundary: &str, len: usize) -> String {
    format!(
        "POST {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\
         Content-Type: multipart/form-data; boundary={boundary}\r\nContent-Length: {len}\r\n\r\n"
    )
}

/// A `GET <path>` request with no extra headers.
fn get_req(path: &str) -> String {
    format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
}

/// A `POST <path>` request with an empty body and no declared content type
/// (the `/api/deps/install/...` route takes its target id from the path, not
/// from a body, so unlike `raw_post` there is no `Content-Type` header).
fn post_no_body(path: &str, extra: &str) -> String {
    format!(
        "POST {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n{extra}Content-Length: 0\r\n\r\n"
    )
}

// There is no server-rendered HTML front door: the one web experience is the
// React bundle in `frontend/dist`, and this crate exposes only the JSON
// analysis API it fetches. The static bundle owns `/` in the unified router.

#[tokio::test]
async fn analyze_runs_the_callback_on_uploaded_bytes() {
    let addr = spawn().await;
    let body = b"(kicad_pcb tiny)";
    let req = raw_post(
        "/api/analyze",
        "X-Board-Filename: tiny.kicad_pcb\r\n",
        body.len(),
    );
    let resp = support::http(addr, &req, body).await;
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
    let req = raw_post(
        "/api/analyze",
        "X-Board-Filename: board.PcbDoc\r\n",
        body.len(),
    );
    let resp = support::http(addr, &req, body).await;
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
    let req = raw_post("/api/analyze", "", body.len());
    let resp = support::http(addr, &req, body).await;
    assert!(resp.contains("200 OK"));
    assert!(
        resp.contains("\"file_name\":\"board\""),
        "should default the filename"
    );
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
async fn design_router_threads_every_supplemental_input_and_repeated_models() {
    let addr = spawn().await;
    let boundary = "----hauksbeedesignboundary";
    // Firmware and schematic bytes deliberately include a null + high byte to
    // prove no part is lossy-UTF8-decoded (which would change the byte count).
    let fw: &[u8] = &[0x7f, b'E', b'L', b'F', 0x00, 0xff, 0x42];
    let schematic = b"<eagle><drawing><schematic/></drawing></eagle>";
    let body = multipart_body(
        boundary,
        &[
            ("board", "design.kicad_pcb", b"board"),
            ("firmware", "drone.elf", fw),
            ("schematic", "design.sch", schematic),
            ("bom", "bom.csv", b"bom"),
            ("placement", "board.pos", b"placement"),
            ("variant", "production.variant.toml", b"variant"),
            ("asbuilt", "unit.asbuilt.toml", b"overlay"),
            ("model_file", "a.toml", b"a"),
            ("model_file", "b.toml", b"b"),
        ],
    );
    let req = multipart_post("/api/analyze-with-firmware", boundary, body.len());
    let response = support::http(addr, &req, &body).await;
    assert!(
        response.contains(&format!(
            "\"fw\":{{\"len\":{},\"name\":\"drone.elf\"}}",
            fw.len()
        )),
        "firmware bytes must reach the analyzer verbatim: {response}"
    );
    assert!(
        response.contains(&format!(
            "\"schematic\":{{\"len\":{},\"name\":\"design.sch\"}}",
            schematic.len()
        )),
        "schematic bytes must reach the analyzer verbatim: {response}"
    );
    assert!(response.contains("\"bom\":\"bom.csv\""), "{response}");
    assert!(
        response.contains("\"placement\":\"board.pos\""),
        "{response}"
    );
    assert!(
        response.contains("\"variant\":\"production.variant.toml\""),
        "{response}"
    );
    assert!(
        response.contains("\"asbuilt\":\"unit.asbuilt.toml\""),
        "{response}"
    );
    assert!(
        response.contains("\"models\":[\"a.toml\",\"b.toml\"]"),
        "{response}"
    );
}

#[tokio::test]
async fn checks_thread_optional_eagle_schematic_bytes_to_the_runner() {
    let check: frontdoor::DesignCheckRunner = Arc::new(|upload, spec| {
        let (schematic_name, schematic_len) = upload
            .schematic
            .map(|file| (file.name, file.bytes.len()))
            .unwrap_or(("none".to_string(), 0));
        format!(
            "{{\"ok\":true,\"board_name\":\"{}\",\"board_len\":{},\"schematic_name\":\"{schematic_name}\",\"schematic_len\":{schematic_len},\"spec_len\":{}}}",
            upload.board.name,
            upload.board.bytes.len(),
            spec.len()
        )
    });
    let addr = support::spawn_router(frontdoor::check_route_with_design(check)).await;

    let boundary = "----hauksbee-check-schematic-boundary";
    let board = b"<eagle><drawing><board/></drawing></eagle>";
    let schematic = b"<eagle><drawing><schematic/></drawing></eagle>";
    let spec = b"duration_ms = 1";
    let body = multipart_body(
        boundary,
        &[
            ("board", "design.brd", board),
            ("schematic", "design.sch", schematic),
            ("spec", "spec.toml", spec),
        ],
    );
    let req = multipart_post("/api/check", boundary, body.len());
    let resp = support::http(addr, &req, &body).await;
    assert!(resp.contains("200 OK"), "should 200: {resp:.200}");
    assert!(resp.contains("\"schematic_name\":\"design.sch\""), "{resp}");
    assert!(
        resp.contains(&format!("\"schematic_len\":{}", schematic.len())),
        "schematic bytes were not threaded verbatim: {resp}"
    );
}

#[tokio::test]
async fn live_launch_threads_optional_eagle_schematic_to_the_launcher() {
    let launch: frontdoor::DesignLiveLauncher = Arc::new(|upload| match upload.schematic {
        Some(file) => Err(format!("saw {} with {} bytes", file.name, file.bytes.len())),
        None => Err("schematic missing".to_string()),
    });
    let addr = support::spawn_router(frontdoor::live_routes_with_design(
        hauksbee_server::LiveHub::new(),
        launch,
    ))
    .await;

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
    let request = multipart_post("/api/live/launch", boundary, body.len());
    let response = support::http(addr, &request, &body).await;
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

    let analyze: frontdoor::DesignAnalyzer = Arc::new(|_| "{\"ok\":true}".to_string());
    let sim: frontdoor::DeckSimulator = Arc::new(|_, _| Ok("{\"ok\":true}".to_string()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let dir_clone = dir.clone();
    tokio::spawn(async move {
        hauksbee_server::serve_frontdoor_on_with_design(
            listener,
            Some(&dir_clone),
            analyze,
            sim,
            None,
            None,
            None,
            "{\"preloaded\":false}".to_string(),
        )
        .await
        .unwrap();
    });

    let html = support::http(addr, &get_req("/"), b"").await;
    assert!(html.contains("200 OK"), "index should 200: {html:.160}");
    let html_lower = html.to_ascii_lowercase();
    assert!(
        html_lower.contains("cache-control: no-cache"),
        "index.html must be no-cache: {html:.400}"
    );

    let js = support::http(addr, &get_req("/assets/app-abc123.js"), b"").await;
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
    let addr = spawn().await;
    let boundary = "----hauksbeetestboundary3";
    // Declares a boundary that never appears: multer hits end-of-stream while
    // still searching for the first boundary and errors.
    let body = b"this is not a multipart body at all";
    let req = multipart_post("/api/analyze-with-firmware", boundary, body.len());
    let resp = support::http(addr, &req, body).await;
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
    support::spawn_router(frontdoor::deps_routes(status, install)).await
}

#[tokio::test]
async fn deps_status_relays_the_engine_json() {
    let addr = spawn_deps().await;
    let resp = support::http(addr, &get_req("/api/deps"), b"").await;
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
    let resp = support::http(addr, &post_no_body("/api/deps/install/ok-dep", ""), b"").await;
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
    let resp = support::http(addr, &post_no_body("/api/deps/install/renode", ""), b"").await;
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
    let resp = support::http(
        addr,
        &post_no_body("/api/deps/install/ok-dep", "Sec-Fetch-Site: cross-site\r\n"),
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
    let ok = support::http(
        addr,
        &post_no_body(
            "/api/deps/install/ok-dep",
            "Sec-Fetch-Site: same-origin\r\n",
        ),
        b"",
    )
    .await;
    assert!(ok.contains("200 OK"), "same-origin must pass: {ok:.200}");
    assert!(ok.contains("event: done"), "{ok}");
}

#[tokio::test]
async fn analyze_with_firmware_without_firmware_part_falls_back() {
    let addr = spawn().await;
    let boundary = "----hauksbeetestboundary2";
    let board = b"(kicad_pcb tiny board)";
    let body = multipart_body(boundary, &[("board", "tiny.kicad_pcb", board)]);
    let req = multipart_post("/api/analyze-with-firmware", boundary, body.len());
    let resp = support::http(addr, &req, &body).await;
    assert!(resp.contains("200 OK"), "should 200: {resp:.200}");
    assert!(
        resp.contains("\"fw\":null"),
        "missing firmware part must fall back to board-only: {resp}"
    );
}

#[tokio::test]
async fn sim_relays_the_simulator_json_and_carries_the_deck_verbatim() {
    let addr = spawn().await;
    let deck = b"rc\nV1 in 0 1\nR1 in out 1k\nC1 out 0 1u\n.op\n.end\n";
    let req = raw_post("/api/sim", "X-Board-Filename: rc.cir\r\n", deck.len());
    let resp = support::http(addr, &req, deck).await;
    assert!(resp.contains("200 OK"), "sim should 200: {resp:.200}");
    assert!(resp.contains("\"analysis\":\"op\""), "{resp}");
    assert!(
        resp.contains(&format!("\"deck_len\":{}", deck.len())),
        "the deck bytes must reach the simulator verbatim: {resp}"
    );
}

#[tokio::test]
async fn sim_refusal_is_a_4xx_carrying_the_reason() {
    // A deck the simulator will not run (here, one that pulls in another file)
    // is a bad request, not a server fault: the status says so and the body
    // still uses this module's {ok:false,error} shape.
    let addr = spawn().await;
    let deck = b".include model.lib\nV1 in 0 1\n.op\n";
    let req = raw_post("/api/sim", "X-Board-Filename: dep.cir\r\n", deck.len());
    let resp = support::http(addr, &req, deck).await;
    assert!(resp.contains(" 422 "), "refusal must be a 4xx: {resp:.200}");
    assert!(resp.contains("\"ok\":false"), "{resp}");
    assert!(resp.contains("pulls in another file"), "{resp}");
    assert!(
        resp.contains("dep.cir"),
        "the name reaches the reason: {resp}"
    );
}

#[tokio::test]
async fn sim_refuses_cross_site_requests() {
    // The deck route runs a solver on uploaded input, so a drive-by POST from a
    // hostile page (Sec-Fetch-Site, which page JS cannot forge) is refused
    // before anything is parsed, the same guard the analyze routes carry.
    let addr = spawn().await;
    let deck = b"rc\nV1 in 0 1\nR1 in 0 1k\n.op\n";
    let req = raw_post(
        "/api/sim",
        "Sec-Fetch-Site: cross-site\r\nX-Board-Filename: rc.cir\r\n",
        deck.len(),
    );
    let resp = support::http(addr, &req, deck).await;
    assert!(resp.contains("403"), "cross-site sim must 403: {resp:.300}");
    assert!(
        !resp.contains("deck_len"),
        "the deck must not have run: {resp}"
    );
}
