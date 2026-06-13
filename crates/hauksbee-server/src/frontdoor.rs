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
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::Router;

/// Analyze an uploaded board: `(file_name, contents) -> JSON report string`.
/// Boxed so the engine can supply its `analyze_json` without the server crate
/// depending on the engine.
pub type Analyzer = Arc<dyn Fn(&str, &str) -> String + Send + Sync>;

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
pub async fn serve(addr: &str, analyze: Analyzer) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    println!("\n  hauksbee is live. Open this in your browser:\n");
    println!("      http://{addr}\n");
    println!("  Drop a board file (.kicad_pcb / .kicad_sch / .brd / gerber zip) on the page");
    println!("  to get a plain-language report. Ctrl-C to stop.\n");
    axum::serve(listener, router(analyze)).await?;
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
  canvas { width: 100%; max-width: 760px; background: #0b0d12; border: 1px solid #232a36; border-radius: 8px; display: block; margin-top: 8px; }
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
  <div id="out"></div>
  <footer>Runs locally via <code>hauksbee serve</code>. Same checks as the command line.</footer>
</main>
<script>
const drop = document.getElementById('drop');
const fileInput = document.getElementById('file');
const out = document.getElementById('out');

drop.addEventListener('click', () => fileInput.click());
['dragenter','dragover'].forEach(e => drop.addEventListener(e, ev => { ev.preventDefault(); drop.classList.add('over'); }));
['dragleave','drop'].forEach(e => drop.addEventListener(e, ev => { ev.preventDefault(); drop.classList.remove('over'); }));
drop.addEventListener('drop', ev => { const f = ev.dataTransfer.files[0]; if (f) upload(f); });
fileInput.addEventListener('change', () => { const f = fileInput.files[0]; if (f) upload(f); });

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

function render(r) {
  if (!r.ok) {
    out.innerHTML = '<div class="verdict bad"><div class="err">' + escapeHtml(r.error || 'Could not read the file.') + '</div></div>';
    return;
  }
  let cls = 'healthy';
  if (r.serious > 0) cls = 'bad';
  else if (r.total > 0) cls = 'warn';

  let html = '';
  html += '<div id="verdict" class="verdict ' + cls + '">' + escapeHtml(r.headline) +
          '<div class="meta">' + escapeHtml(r.board_name || r.file_name) + ' &middot; ' +
          r.num_components + ' parts &middot; ' + r.num_nets + ' nets</div></div>';

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
