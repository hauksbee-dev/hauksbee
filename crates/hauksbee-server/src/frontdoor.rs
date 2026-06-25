//! The local "drop your board, get a report" web front door.
//!
//! A non-CLI, non-engineer user runs `hauksbee serve`, opens the printed URL,
//! drops a board file onto the page, and gets back the plain-language verdict,
//! the full report, and a simple 2D map of where the parts sit — all in the
//! browser, no terminal beyond starting the server.
//!
//! This module is the thin HTTP layer only. The actual analysis is injected as a
//! callback (`Analyzer`) so the server crate stays free of any dependency on the
//! engine/extract crates (which depend on *this* crate); the binary wires the
//! engine's `analyze_json` in. The page itself is a single self-contained HTML
//! string (no build step, no extra assets), so it works from a fresh clone with
//! nothing but `cargo run`.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Multipart, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::Router;

/// Analyze an uploaded board: `(file_name, contents) -> JSON report string`.
/// Boxed so the engine can supply its `analyze_json` without the server crate
/// depending on the engine.
pub type Analyzer = Arc<dyn Fn(&str, &str) -> String + Send + Sync>;

/// Analyze a board AND an optional firmware: `(board_name, board_contents,
/// Option<(firmware_name, firmware_bytes)>) -> JSON report string`. Firmware is
/// passed as raw `&[u8]` (never lossy-decoded) so an uploaded ELF stays intact.
/// Parallel to [`Analyzer`] so the server crate stays engine-free and the
/// existing `/api/analyze` path + call sites are untouched.
pub type FirmwareAnalyzer =
    Arc<dyn Fn(&str, &str, Option<(&str, &[u8])>) -> String + Send + Sync>;

/// Largest board upload accepted (32 MiB). Gerber zips and big KiCad layouts fit
/// comfortably; this just stops a pathological upload from exhausting memory.
const MAX_UPLOAD_BYTES: usize = 32 * 1024 * 1024;

struct FrontDoorState {
    analyze: Analyzer,
}

/// Build the front-door router: the upload page at `/` and the analysis endpoint
/// at `/api/analyze`.
pub fn router(analyze: Analyzer) -> Router {
    let state = Arc::new(FrontDoorState { analyze });
    Router::new()
        .route("/", get(index))
        .route("/api/analyze", post(analyze_handler))
        .layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES))
        .with_state(state)
}

/// Serve the front-door on `addr`, printing a friendly banner.
/// Bind `addr`, but if its port is already in use, fall back to the next few
/// ports and finally an OS-assigned free port — so `serve` never dies with a
/// bare "Address already in use (os error 48)" that reads as "the tool is
/// broken". The caller prints the ACTUAL bound address via `local_addr()`.
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
                sa.set_port(0); // let the OS choose any free port
                if let Ok(l) = tokio::net::TcpListener::bind(sa).await {
                    return Ok(l);
                }
            }
            Err(e.into())
        }
        Err(e) => Err(e.into()),
    }
}

pub async fn serve(addr: &str, analyze: Analyzer) -> anyhow::Result<()> {
    let listener = bind_with_fallback(addr).await?;
    let bound = listener.local_addr()?;
    println!("\n  hauksbee is live. Open this in your browser:\n");
    println!("      http://{bound}\n");
    println!("  Drop a board file (.kicad_pcb / .kicad_sch / .brd / gerber zip) on the page");
    println!("  to get a plain-language report. Ctrl-C to stop.\n");
    axum::serve(listener, router(analyze)).await?;
    Ok(())
}

struct FirmwareState {
    analyze: FirmwareAnalyzer,
}

/// Build the firmware-aware front-door router: the upload page at `/`, the
/// board-only analysis at `/api/analyze`, AND the firmware co-sim endpoint at
/// `/api/analyze-with-firmware` (multipart: `board` + optional `firmware`).
///
/// Both endpoints share the one [`FirmwareAnalyzer`]; the board-only path simply
/// passes `None` for the firmware. The existing [`router`] (board-only) is left
/// in place so call sites that have no engine firmware path keep working.
pub fn router_with_firmware(analyze: FirmwareAnalyzer) -> Router {
    let state = Arc::new(FirmwareState { analyze });
    Router::new()
        .route("/", get(index))
        .route("/api/analyze", post(analyze_handler_fw))
        .route("/api/analyze-with-firmware", post(analyze_firmware_handler))
        .layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES))
        .with_state(state)
}

/// Serve the firmware-aware front-door on `addr`, printing a friendly banner.
pub async fn serve_with_firmware(addr: &str, analyze: FirmwareAnalyzer) -> anyhow::Result<()> {
    let listener = bind_with_fallback(addr).await?;
    let bound = listener.local_addr()?;
    println!("\n  hauksbee is live. Open this in your browser:\n");
    println!("      http://{bound}\n");
    println!("  Drop a board file (.kicad_pcb / .kicad_sch / .brd / gerber zip) on the page");
    println!("  to get a plain-language report. Optionally drop firmware (.elf / .hex)");
    println!("  alongside it to run a short co-sim. Ctrl-C to stop.\n");
    axum::serve(listener, router_with_firmware(analyze)).await?;
    Ok(())
}

async fn index() -> Html<&'static str> {
    Html(PAGE)
}

/// Accept the raw board file as the request body, with the original filename in
/// the `X-Board-Filename` header (the page sets it). Returns the analysis JSON.
async fn analyze_handler(
    State(state): State<Arc<FrontDoorState>>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let file_name = headers
        .get("x-board-filename")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("board")
        .to_string();

    // Board files are text (KiCad/Eagle/IPC) or a zip (gerbers). The analyzer's
    // extractor sniffs the format; we hand it a lossy-UTF8 view, which is exact
    // for the text formats and lets the zip path still recognise its magic.
    let contents = String::from_utf8_lossy(&body).into_owned();
    let json = (state.analyze)(&file_name, &contents);

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        json,
    )
}

/// Board-only analysis for the firmware-aware router: same contract as
/// [`analyze_handler`] (raw body + `X-Board-Filename`) but routed through the
/// [`FirmwareAnalyzer`] with `None` firmware, so the single-file drop path is
/// unchanged when the firmware-aware router is mounted.
async fn analyze_handler_fw(
    State(state): State<Arc<FirmwareState>>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let file_name = headers
        .get("x-board-filename")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("board")
        .to_string();
    let contents = String::from_utf8_lossy(&body).into_owned();
    let json = (state.analyze)(&file_name, &contents, None);
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        json,
    )
}

/// Accept a `multipart/form-data` upload with a `board` part (required) and a
/// `firmware` part (optional). The board is decoded lossily (it is text or a
/// zip the extractor sniffs); the firmware is passed as raw `&[u8]` — NEVER
/// lossy-decoded, which would corrupt an ELF. Falls back to a board-only
/// analysis when no firmware part is present.
async fn analyze_firmware_handler(
    State(state): State<Arc<FirmwareState>>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    let mut board_name = "board".to_string();
    let mut board_bytes: Option<Vec<u8>> = None;
    let mut fw_name = String::new();
    let mut fw_bytes: Option<Vec<u8>> = None;

    while let Ok(Some(field)) = multipart.next_field().await {
        let part = field.name().unwrap_or("").to_string();
        let filename = field.file_name().map(|s| s.to_string());
        let data = match field.bytes().await {
            Ok(b) => b,
            Err(e) => {
                let json = format!(
                    "{{\"ok\":false,\"error\":\"failed to read upload part: {}\"}}",
                    e.to_string().replace('"', "'")
                );
                return (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "application/json")],
                    json,
                );
            }
        };
        match part.as_str() {
            // Accept "board" or "file" for the PCB part. The browser form uses a
            // `file` input id and the raw /api/analyze path is conceptually "the
            // file", so a caller who reaches for either name should just work
            // instead of hitting a confusing "expected a 'board' part" error.
            "board" | "file" => {
                if let Some(f) = filename {
                    board_name = f;
                }
                board_bytes = Some(data.to_vec());
            }
            "firmware" => {
                if let Some(f) = filename {
                    fw_name = f;
                }
                // Ignore an empty firmware part (e.g. the browser sent the field
                // with no file selected) so we cleanly fall back to board-only.
                if !data.is_empty() {
                    fw_bytes = Some(data.to_vec());
                }
            }
            _ => {}
        }
    }

    let board_bytes = match board_bytes {
        Some(b) => b,
        None => {
            let json =
                "{\"ok\":false,\"error\":\"no board file in the upload (expected a 'board' or 'file' part)\"}"
                    .to_string();
            return (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/json")],
                json,
            );
        }
    };
    let contents = String::from_utf8_lossy(&board_bytes).into_owned();

    let json = match &fw_bytes {
        Some(bytes) => (state.analyze)(&board_name, &contents, Some((&fw_name, bytes))),
        None => (state.analyze)(&board_name, &contents, None),
    };

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        json,
    )
}

/// The whole front-door UI: one self-contained HTML page. Upload (click or
/// drag-drop) -> POST the bytes -> render the verdict, the per-section findings,
/// and a 2D part map drawn on a canvas from the returned component positions.
const PAGE: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8" />
<meta name="viewport" content="width=device-width, initial-scale=1" />
<title>hauksbee — check your board</title>
<style>
  :root { color-scheme: dark; }
  * { box-sizing: border-box; }
  body {
    margin: 0; font: 16px/1.5 -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
    background: #0f1115; color: #e6e8eb;
  }
  header { padding: 28px 24px 8px; }
  h1 { margin: 0 0 4px; font-size: 22px; letter-spacing: .2px; }
  .sub { color: #9aa3ad; font-size: 14px; }
  main { max-width: 880px; margin: 0 auto; padding: 16px 24px 64px; }
  #drop {
    margin-top: 16px; border: 2px dashed #313846; border-radius: 12px;
    padding: 44px 20px; text-align: center; cursor: pointer; transition: .15s;
    background: #151922;
  }
  #drop:hover, #drop.over { border-color: #4f8cff; background: #18202e; }
  #drop strong { color: #cdd5df; }
  #drop .hint { color: #7e8794; font-size: 13px; margin-top: 6px; }
  input[type=file] { display: none; }
  .verdict {
    margin-top: 22px; padding: 16px 18px; border-radius: 10px; font-size: 17px;
    border: 1px solid #2a3140; background: #151922;
  }
  .verdict.healthy { border-color: #2f6b41; background: #122017; }
  .verdict.warn { border-color: #7a6320; background: #1f1b10; }
  .verdict.bad { border-color: #8a3a3a; background: #201414; }
  .meta { color: #9aa3ad; font-size: 13px; margin-top: 6px; }
  section.block { margin-top: 26px; }
  section.block > h2 { font-size: 15px; color: #aab2bd; text-transform: uppercase; letter-spacing: .6px; margin: 0 0 10px; }
  .sect-verdict { color: #9aa3ad; font-size: 14px; margin-bottom: 10px; }
  .finding { border: 1px solid #262d3a; border-left-width: 4px; border-radius: 8px; padding: 12px 14px; margin-bottom: 10px; background: #141823; }
  .finding.serious { border-left-color: #e5484d; }
  .finding.warning { border-left-color: #e2a336; }
  .finding.note { border-left-color: #5b6577; }
  .finding .tag { font-size: 11px; font-weight: 700; letter-spacing: .8px; text-transform: uppercase; }
  .finding.serious .tag { color: #ff6b6f; }
  .finding.warning .tag { color: #f0b75a; }
  .finding.note .tag { color: #8b94a3; }
  .finding .what { margin: 4px 0 8px; font-weight: 600; }
  .finding .row { font-size: 14px; margin: 2px 0; }
  .finding .row b { color: #9aa3ad; font-weight: 600; }
  .headsup { border: 1px solid #355a7a; border-left-width: 4px; border-left-color: #4f8cff; border-radius: 8px; padding: 10px 14px; margin-bottom: 10px; background: #111a26; }
  .headsup .tag { font-size: 11px; font-weight: 700; letter-spacing: .8px; text-transform: uppercase; color: #6fa8ff; }
  .headsup .text { margin-top: 4px; font-size: 14px; color: #cdd5df; }
  .bindbanner { margin-top: 16px; padding: 12px 16px; border-radius: 10px; border: 1px solid #7a6320; background: #1f1b10; }
  .bindbanner .tag { font-size: 11px; font-weight: 700; letter-spacing: .8px; text-transform: uppercase; color: #f0b75a; }
  .bindbanner .text { margin-top: 4px; font-size: 14px; color: #e7d8ad; }
  .note { border: 1px solid #2a3140; border-left-width: 4px; border-left-color: #8b94a3; border-radius: 8px; padding: 10px 14px; margin-bottom: 10px; background: #141823; }
  .note .tag { font-size: 11px; font-weight: 700; letter-spacing: .8px; text-transform: uppercase; color: #8b94a3; }
  .note .text { margin-top: 4px; font-size: 14px; color: #cdd5df; }
  canvas { width: 100%; max-width: 760px; background: #0b0d12; border: 1px solid #232a36; border-radius: 8px; display: block; margin-top: 8px; }
  #firmware-drop {
    margin-top: 12px; border: 2px dashed #2a3140; border-radius: 12px;
    padding: 20px; text-align: center; cursor: pointer; transition: .15s;
    background: #131722; font-size: 14px;
  }
  #firmware-drop:hover, #firmware-drop.over { border-color: #4f8cff; background: #18202e; }
  #firmware-drop strong { color: #cdd5df; }
  #firmware-drop .hint { color: #7e8794; font-size: 12px; margin-top: 4px; }
  #firmware-drop.set { border-style: solid; border-color: #2f6b41; background: #112017; }
  .uart { background: #0b0d12; border: 1px solid #232a36; border-radius: 8px; padding: 10px 12px; font: 12px/1.5 ui-monospace, SFMono-Regular, Menlo, monospace; color: #cdd5df; white-space: pre-wrap; overflow-x: auto; margin-bottom: 10px; }
  table.gpio { width: 100%; border-collapse: collapse; font-size: 13px; margin-top: 6px; }
  table.gpio th, table.gpio td { text-align: left; padding: 4px 8px; border-bottom: 1px solid #232a36; }
  table.gpio th { color: #9aa3ad; font-weight: 600; }
  table.gpio td.driven { color: #6bd08a; }
  table.gpio td.idle { color: #7e8794; }
  .err { color: #ff8a8a; }
  .spinner { color: #9aa3ad; margin-top: 22px; }
  footer { color: #5b6577; font-size: 12px; margin-top: 40px; }
  code { background: #1b2330; padding: 1px 5px; border-radius: 4px; }
</style>
</head>
<body>
<header>
  <h1>hauksbee — check your board</h1>
  <div class="sub">Drop a PCB file and get a plain-language report: what is wrong, why it matters, and how to fix it. Nothing leaves your machine.</div>
</header>
<main>
  <label id="drop" for="file">
    <strong>Click to choose a board file, or drop one here</strong>
    <div class="hint">KiCad <code>.kicad_pcb</code> / <code>.kicad_sch</code>, Eagle <code>.brd</code>, IPC <code>.d356</code>, or a gerber <code>.zip</code></div>
  </label>
  <input id="file" type="file" accept=".kicad_pcb,.kicad_sch,.brd,.d356,.zip,.txt" />
  <label id="firmware-drop" for="firmware-file">
    <strong>Optional: drop firmware (.elf / .hex) to run a co-sim</strong>
    <div class="hint">Runs the firmware on the board's microcontroller for a fraction of a second and reports any electrical stress. In-process MCUs only.</div>
  </label>
  <input id="firmware-file" type="file" accept=".elf,.hex" />
  <div id="out"></div>
  <footer>Runs locally via <code>hauksbee serve</code>. Same checks as the command line.</footer>
</main>
<script>
const drop = document.getElementById('drop');
const fileInput = document.getElementById('file');
const fwDrop = document.getElementById('firmware-drop');
const fwInput = document.getElementById('firmware-file');
const out = document.getElementById('out');

// The most recent board file, so dropping firmware afterwards re-runs with it.
let lastBoardFile = null;
// The selected firmware file (if any); when set, board uploads run the co-sim.
let firmwareFile = null;

drop.addEventListener('click', () => fileInput.click());
['dragenter','dragover'].forEach(e => drop.addEventListener(e, ev => { ev.preventDefault(); drop.classList.add('over'); }));
['dragleave','drop'].forEach(e => drop.addEventListener(e, ev => { ev.preventDefault(); drop.classList.remove('over'); }));
drop.addEventListener('drop', ev => { const f = ev.dataTransfer.files[0]; if (f) handleBoard(f); });
fileInput.addEventListener('change', () => { const f = fileInput.files[0]; if (f) handleBoard(f); });

fwDrop.addEventListener('click', () => fwInput.click());
['dragenter','dragover'].forEach(e => fwDrop.addEventListener(e, ev => { ev.preventDefault(); fwDrop.classList.add('over'); }));
['dragleave','drop'].forEach(e => fwDrop.addEventListener(e, ev => { ev.preventDefault(); fwDrop.classList.remove('over'); }));
fwDrop.addEventListener('drop', ev => { const f = ev.dataTransfer.files[0]; if (f) handleFirmware(f); });
fwInput.addEventListener('change', () => { const f = fwInput.files[0]; if (f) handleFirmware(f); });

function handleBoard(file) {
  lastBoardFile = file;
  if (firmwareFile) uploadWithFirmware(file, firmwareFile);
  else upload(file);
}

function handleFirmware(file) {
  firmwareFile = file;
  fwDrop.querySelector('strong').textContent = 'Firmware: ' + file.name + ' (click to change)';
  fwDrop.classList.add('set');
  // If a board is already loaded, re-run immediately with the firmware.
  if (lastBoardFile) uploadWithFirmware(lastBoardFile, firmwareFile);
}

async function upload(file) {
  out.innerHTML = '<div class="spinner">Analyzing ' + escapeHtml(file.name) + ' ...</div>';
  let buf;
  try { buf = await file.arrayBuffer(); }
  catch (e) { out.innerHTML = '<div class="err">Could not read that file.</div>'; return; }
  try {
    const res = await fetch('/api/analyze', {
      method: 'POST',
      headers: { 'X-Board-Filename': file.name, 'Content-Type': 'application/octet-stream' },
      body: buf,
    });
    const report = await res.json();
    render(report);
  } catch (e) {
    out.innerHTML = '<div class="err">Analysis failed: ' + escapeHtml(String(e)) + '</div>';
  }
}

async function uploadWithFirmware(boardFile, fwFile) {
  out.innerHTML = '<div class="spinner">Analyzing ' + escapeHtml(boardFile.name) +
                  ' + co-sim of ' + escapeHtml(fwFile.name) + ' ...</div>';
  try {
    const fd = new FormData();
    fd.append('board', boardFile, boardFile.name);
    fd.append('firmware', fwFile, fwFile.name);
    const res = await fetch('/api/analyze-with-firmware', { method: 'POST', body: fd });
    const report = await res.json();
    render(report);
  } catch (e) {
    out.innerHTML = '<div class="err">Analysis failed: ' + escapeHtml(String(e)) + '</div>';
  }
}

function render(r) {
  if (!r.ok) {
    out.innerHTML = '<div class="verdict bad"><div class="err">' + escapeHtml(r.error || 'Could not read the file.') + '</div></div>';
    return;
  }
  // An open active IC on the live circuit or an actionable heads-up note means
  // the board is NOT simply "healthy", even with zero findings. Reflect that in
  // the verdict colour so the web report can never give false comfort.
  const bindOpen = !!(r.bind && r.bind.active_path_unresolved && r.bind.active_path_unresolved.length);
  const hasHeadsUp = (r.sections || []).some(s => s.heads_up && s.heads_up.length);
  let cls = 'healthy';
  if (r.serious > 0) cls = 'bad';
  else if (r.total > 0 || bindOpen) cls = 'warn';
  else if (hasHeadsUp) cls = 'warn';

  let html = '';
  html += '<div id="verdict" class="verdict ' + cls + '">' + escapeHtml(r.headline) +
          '<div class="meta">' + escapeHtml(r.board_name || r.file_name) + ' &middot; ' +
          r.num_components + ' parts &middot; ' + r.num_nets + ' nets</div></div>';

  // Bind banner: active ICs left open on the live circuit make analog/AC/thermal
  // results untrustworthy. Render it loudly, right under the verdict.
  if (bindOpen) {
    html += '<div class="bindbanner"><span class="tag">Active parts unresolved</span>' +
            '<div class="text">' +
            escapeHtml(r.bind.active_path_unresolved.join(', ')) +
            ' could not be bound or are left open on the live circuit. Analog / AC / thermal results on their nets are not trustworthy.' +
            '</div></div>';
  }

  // Top-level honesty notes (bind roles, coverage caveats).
  for (const n of (r.notes || [])) {
    html += '<div class="note"><span class="tag">Note</span>' +
            '<div class="text">' + escapeHtml(n.message) + '</div></div>';
  }

  for (const s of r.sections) {
    html += '<section class="block"><h2>' + escapeHtml(s.title) + '</h2>';
    html += '<div class="sect-verdict">' + escapeHtml(s.verdict) + '</div>';
    for (const f of s.findings) {
      html += '<div class="finding ' + f.level + '">';
      html += '<span class="tag">' + f.level + '</span>';
      html += '<div class="what">' + escapeHtml(f.what) + '</div>';
      html += '<div class="row"><b>Why it matters:</b> ' + escapeHtml(f.why) + '</div>';
      html += '<div class="row"><b>What to do:</b> ' + escapeHtml(f.fix) + '</div>';
      html += '</div>';
    }
    // Heads-up notes (e.g. the 171-ohm USB controlled-impedance note): worth
    // knowing, not a failure, but NEVER silently dropped on the web surface.
    for (const h of (s.heads_up || [])) {
      html += '<div class="headsup"><span class="tag">Heads up</span>' +
              '<div class="text">' + escapeHtml(h) + '</div></div>';
    }
    html += '</section>';
  }

  // Firmware co-sim section (only present when firmware was dropped). Reuses the
  // exact .finding card markup so it can never disagree with the CLI co-sim.
  if (r.cosim) {
    html += '<section class="block"><h2>Firmware co-sim</h2>';
    if (r.cosim.ran) {
      html += '<div class="sect-verdict">Ran the firmware for ' +
              (r.cosim.seconds_simulated || 0).toFixed(3) + 's on the board’s microcontroller.</div>';
      if (!r.cosim.findings || !r.cosim.findings.length) {
        html += '<div class="note"><span class="tag">Note</span><div class="text">' +
                'No electrical-stress faults during the run.</div></div>';
      }
      for (const f of (r.cosim.findings || [])) {
        html += '<div class="finding ' + f.level + '">';
        html += '<span class="tag">' + f.level + '</span>';
        html += '<div class="what">' + escapeHtml(f.what) + '</div>';
        html += '<div class="row"><b>Why it matters:</b> ' + escapeHtml(f.why) + '</div>';
        html += '<div class="row"><b>What to do:</b> ' + escapeHtml(f.fix) + '</div>';
        html += '</div>';
      }
      if (r.cosim.uart_output) {
        html += '<div class="row"><b>UART output:</b></div>';
        html += '<div class="uart">' + escapeHtml(r.cosim.uart_output) + '</div>';
      }
      if (r.cosim.gpio_nets && r.cosim.gpio_nets.length) {
        html += '<table class="gpio"><thead><tr><th>Net</th><th>Volts</th><th>Activity</th></tr></thead><tbody>';
        for (const g of r.cosim.gpio_nets) {
          html += '<tr><td>' + escapeHtml(g.name) + '</td><td>' + (g.volts || 0).toFixed(3) + '</td>' +
                  '<td class="' + (g.driven ? 'driven' : 'idle') + '">' +
                  (g.driven ? 'driven' : 'idle') + '</td></tr>';
        }
        html += '</tbody></table>';
      }
    } else {
      // Board has no MCU / external backend / firmware failed to load: the
      // engine returns the reason as a single note-level finding.
      for (const f of (r.cosim.findings || [])) {
        html += '<div class="note"><span class="tag">Co-sim not available</span>' +
                '<div class="text">' + escapeHtml(f.what + ' ' + f.why) + '</div></div>';
      }
      if (!r.cosim.findings || !r.cosim.findings.length) {
        html += '<div class="note"><span class="tag">Co-sim not available</span>' +
                '<div class="text">Co-sim not available for this board.</div></div>';
      }
    }
    html += '</section>';
  }

  if (r.components && r.components.length) {
    html += '<section class="block"><h2>Board map (2D)</h2>' +
            '<canvas id="map" width="760" height="460"></canvas></section>';
  }

  out.innerHTML = html;
  if (r.components && r.components.length) drawMap(r.components);
}

function drawMap(comps) {
  const cv = document.getElementById('map');
  const ctx = cv.getContext('2d');
  const W = cv.width, H = cv.height, pad = 28;
  let minX = Infinity, minY = Infinity, maxX = -Infinity, maxY = -Infinity;
  for (const c of comps) { minX = Math.min(minX, c.x); minY = Math.min(minY, c.y); maxX = Math.max(maxX, c.x); maxY = Math.max(maxY, c.y); }
  const spanX = Math.max(1e-6, maxX - minX), spanY = Math.max(1e-6, maxY - minY);
  const scale = Math.min((W - 2*pad)/spanX, (H - 2*pad)/spanY);
  const tx = x => pad + (x - minX) * scale;
  const ty = y => pad + (y - minY) * scale;
  ctx.clearRect(0,0,W,H);
  for (const c of comps) {
    const x = tx(c.x), y = ty(c.y);
    ctx.fillStyle = '#4f8cff';
    ctx.beginPath(); ctx.arc(x, y, 3, 0, Math.PI*2); ctx.fill();
    ctx.fillStyle = '#8b94a3'; ctx.font = '10px sans-serif';
    ctx.fillText(c.reference, x + 5, y + 3);
  }
}

function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
}
</script>
</body>
</html>
"##;
