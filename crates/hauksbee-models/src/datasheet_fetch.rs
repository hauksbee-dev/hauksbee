//! Downloading a datasheet PDF from a URL, and surviving what real vendor
//! sites do to a plain `curl`.
//!
//! Measured on real boards, a quarter to a half of the datasheet links a board
//! names do not download on the first try: curl exit 92 ("HTTP/2 stream not
//! closed cleanly") on many hosts, an HTML landing page where a PDF was
//! promised, HTTP 403, HTTP 404 for a browser's `User-Agent` that curl's own
//! gets through. Each has a bounded remedy here, in a fixed order, and nothing
//! loops:
//!
//! 1. A URL is tried with several client [`Strategy`]s, cheapest first: `curl`
//!    with its own `User-Agent`, `curl` with a browser's headers, `curl` with a
//!    fuller browser header set (including a same-site `Referer`), and, when
//!    `node` (18 or later) is on the `PATH`, Node's `fetch`, whose TLS stack
//!    (OpenSSL) several CDNs accept where macOS's `curl` is reset. (With `node`
//!    present, a `curl` that goes quiet is waited for 20 s, not the full stall
//!    time: such hosts send nothing at all, so the rest is pure waiting.) The first
//!    strategy that yields a verified PDF wins, and [`StrategyMemory`] makes
//!    the rest of the run try the winner first for that host. A transport
//!    failure (reset, stall, TLS) says nothing a different header set can fix,
//!    so the strategies on the same client are skipped after one and the next
//!    client is tried; an HTTP answer (403, 404, an HTML page) moves on to
//!    the next strategy. A rate limit (429) or server error that outlasts its
//!    retries ends the attempt: another client would hit the same wall.
//! 2. Within a strategy, a transient failure (a curl transport error such as 92,
//!    or HTTP 429, 502, 503, 504) is retried twice with a growing pause, the
//!    first retry over HTTP/1.1. A stall (no bytes for [`Timeouts::stall_secs`])
//!    is not retried: the next client is the remedy.
//! 3. A page that is HTML is searched for its PDF links; when exactly one is a
//!    plausible datasheet (or exactly one names the part), that is followed, one
//!    hop, with the same strategy.
//!
//! A download is bounded by a stall timeout (no bytes at all for
//! [`Timeouts::stall_secs`]) and a generous overall cap, not a short fixed
//! total: a large PDF from a slow host that keeps sending finishes, and a host
//! that goes quiet is abandoned quickly.
//!
//! [`HostLedger`] remembers, within one run, which hosts refused or did not
//! answer, so a batch does not spend a search and a download per part finding
//! that out again.
//!
//! Anything left over is a [`FetchError`] whose one-line text names the
//! strategies tried, and the caller decides what is next (discovery, or telling
//! the person to download it). Nothing here answers a CAPTCHA, hides what the
//! client is, or works around bot protection: a site that asks for a person gets
//! one, via `--datasheet`. The alternative clients that were measured and not
//!
//! The transport is the [`Http`] trait so every rule above is tested without a
//! network; [`CurlHttp`] is the one real implementation (`curl`, and `node`
//! when present; no shell).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

/// Which program makes the request. Two strategies on one client share a TLS
/// stack, so a connection-level failure on one is the same failure on the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Client {
    Curl,
    Node,
}

/// One way of asking for a URL, tried cheapest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Strategy {
    /// `curl` with its own `User-Agent` and no extra headers. Some manufacturer
    /// sites refuse a browser's `User-Agent` on a bare download link.
    CurlPlain,
    /// `curl` with a browser's `User-Agent`, a PDF-first `Accept` and a language
    /// (many manufacturer sites refuse curl's own).
    CurlStandard,
    /// `curl` with a different browser build's full header set, including a
    /// language and a same-site `Referer`.
    CurlBrowser,
    /// Node's built-in `fetch` with its default headers: another TLS stack
    /// (OpenSSL) and a different handshake, for hosts that reset `curl`'s.
    NodeFetch,
}

impl Strategy {
    /// Every strategy in the order they are tried.
    pub const ALL: [Self; 4] = [
        Self::CurlPlain,
        Self::CurlStandard,
        Self::CurlBrowser,
        Self::NodeFetch,
    ];

    /// The program that runs it.
    #[must_use]
    pub const fn client(self) -> Client {
        match self {
            Self::NodeFetch => Client::Node,
            _ => Client::Curl,
        }
    }

    /// A few words for an error line.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::CurlPlain => "curl",
            Self::CurlStandard => "curl with browser headers",
            Self::CurlBrowser => "curl with full browser headers",
            Self::NodeFetch => "node fetch",
        }
    }
}

/// One download attempt.
#[derive(Debug, Clone, Copy)]
pub struct Request<'a> {
    pub url: &'a str,
    /// Where the body is written.
    pub dest: &'a Path,
    pub strategy: Strategy,
    /// Force HTTP/1.1 (`--http1.1`): the cure for curl's HTTP/2 stream errors.
    /// Ignored by clients that have no such switch.
    pub http1: bool,
}

/// What the server answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    /// The URL after redirects, which relative links on a landing page resolve against.
    pub final_url: String,
}

/// The transport failed before an HTTP status came back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportError {
    /// The client's exit code, when there was one (curl's, or 28 for a Node
    /// fetch that stalled).
    pub code: Option<i32>,
    pub message: String,
}

impl TransportError {
    /// Worth one more try: curl's connection, framing, timeout and
    /// partial-transfer errors (16 and 92 HTTP/2, 18 partial, 28 timeout, 35 TLS
    /// connect, 52 empty reply, 55 and 56 send and receive).
    #[must_use]
    pub const fn transient(&self) -> bool {
        matches!(self.code, Some(16 | 18 | 28 | 35 | 52 | 55 | 56 | 92))
    }

    /// The client went quiet (curl's exit 28): the same client will again.
    #[must_use]
    pub const fn stalled(&self) -> bool {
        matches!(self.code, Some(28))
    }
}

/// The network, as far as a datasheet fetch needs it. `Sync` because a batch
/// downloads for several parts at once.
pub trait Http: Sync {
    /// Fetch `req.url` into `req.dest` and report the final status and URL.
    ///
    /// # Errors
    ///
    /// A [`TransportError`] when no HTTP answer came back.
    fn get(&self, req: &Request<'_>) -> Result<Response, TransportError>;

    /// Wait before a retry. The default does not wait, so a test double never
    /// sleeps; a real transport overrides it.
    fn pause(&self, _how_long: Duration) {}

    /// Whether this transport can run `strategy` at all (an optional program may
    /// be absent). A strategy that cannot run is skipped without a word. The
    /// default runs only curl's own, so a transport opts in to the fallbacks.
    fn available(&self, strategy: Strategy) -> bool {
        strategy == Strategy::CurlPlain
    }

    /// Which strategy worked for which host so far, if this transport keeps
    /// that. The default keeps nothing.
    fn memory(&self) -> Option<&StrategyMemory> {
        None
    }
}

/// Which [`Strategy`] last produced a PDF from each host, within one run, so the
/// rest of a batch asks that way first instead of rediscovering it. Shared
/// between the batch's threads.
#[derive(Debug, Default)]
pub struct StrategyMemory {
    hosts: Mutex<HashMap<String, Strategy>>,
}

impl StrategyMemory {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Strategy>> {
        self.hosts.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The strategy that last worked for `url`'s host.
    #[must_use]
    pub fn preferred(&self, url: &str) -> Option<Strategy> {
        self.lock().get(&host_of(url)?).copied()
    }

    /// Record that `strategy` fetched a PDF from `url`'s host.
    pub fn worked(&self, url: &str, strategy: Strategy) {
        if let Some(host) = host_of(url) {
            self.lock().insert(host, strategy);
        }
    }
}

/// How long one download may take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    /// Seconds to make the connection.
    pub connect_secs: u32,
    /// Seconds with no data at all before the transfer is abandoned. A download
    /// that keeps sending, however slowly, is not stalled.
    pub stall_secs: u32,
    /// The most a whole download may take, however steadily it progresses.
    pub overall_secs: u32,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect_secs: 20,
            stall_secs: 60,
            overall_secs: 900,
        }
    }
}

/// `curl`, and `node` when it is on the `PATH`, each run without a shell. The
/// name is the first, always-present client; see [`Strategy`] for the rest.
#[derive(Debug, Default)]
pub struct CurlHttp {
    pub timeouts: Timeouts,
    memory: StrategyMemory,
}

/// How long a quiet `curl` is waited for when `node` is there to take over.
/// Measured: the hosts that stall `curl` do so with no byte at all, so the
/// full stall time is pure waiting; a last resort keeps the full time.
const HANDOFF_STALL_SECS: u32 = 20;

impl CurlHttp {
    /// The limits for a `curl` attempt: the stall time is cut to
    /// [`HANDOFF_STALL_SECS`] when a second client can take over.
    fn curl_timeouts(&self, other_client: bool) -> Timeouts {
        Timeouts {
            stall_secs: if other_client {
                self.timeouts.stall_secs.min(HANDOFF_STALL_SECS)
            } else {
                self.timeouts.stall_secs
            },
            ..self.timeouts
        }
    }

    /// A transport with these limits and nothing learned yet.
    #[must_use]
    pub fn with_timeouts(timeouts: Timeouts) -> Self {
        Self {
            timeouts,
            memory: StrategyMemory::default(),
        }
    }
}

const STANDARD_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_0) AppleWebKit/537.36 \
                           (KHTML, like Gecko) Chrome/126.0 Safari/537.36";
const BROWSER_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                          (KHTML, like Gecko) Chrome/125.0.0.0 Safari/537.36 Edg/125.0.0.0";

/// The `curl` arguments for one attempt: redirects followed (most datasheet
/// links redirect to a CDN) but only to http(s), a bounded time, and the final
/// status and URL written to stdout.
#[must_use]
pub fn curl_args(req: &Request<'_>, timeouts: &Timeouts) -> Vec<String> {
    let mut args: Vec<String> = [
        "-sSL",
        "--proto",
        "=http,https",
        "--proto-redir",
        "=http,https",
        "--max-redirs",
        "10",
    ]
    .map(str::to_owned)
    .to_vec();
    args.extend([
        "--connect-timeout".to_owned(),
        timeouts.connect_secs.to_string(),
        "--max-time".to_owned(),
        timeouts.overall_secs.to_string(),
        // A server that goes quiet (bot protection holding the connection open,
        // a host that stops mid-file) is abandoned once nothing has arrived for
        // the stall time; a slow download that keeps moving is left to finish,
        // up to the overall cap.
        "--speed-limit".to_owned(),
        "1".to_owned(),
        "--speed-time".to_owned(),
        timeouts.stall_secs.max(1).to_string(),
    ]);
    if req.http1 {
        args.push("--http1.1".into());
    }
    let header = |args: &mut Vec<String>, h: String| {
        args.push("-H".into());
        args.push(h);
    };
    match req.strategy {
        Strategy::CurlPlain | Strategy::NodeFetch => args.push("--compressed".into()),
        Strategy::CurlStandard => {
            args.extend(["-A".into(), STANDARD_UA.into(), "--compressed".into()]);
            header(
                &mut args,
                "Accept: application/pdf,application/octet-stream;q=0.9,text/html;q=0.8,*/*;q=0.7"
                    .into(),
            );
            header(&mut args, "Accept-Language: en-US,en;q=0.9".into());
        }
        Strategy::CurlBrowser => {
            args.extend(["-A".into(), BROWSER_UA.into(), "--compressed".into()]);
            header(
                &mut args,
                "Accept: text/html,application/xhtml+xml,application/pdf,*/*;q=0.8".into(),
            );
            header(&mut args, "Accept-Language: en-US,en;q=0.9".into());
            if let Some(origin) = origin_of(req.url) {
                header(&mut args, format!("Referer: {origin}/"));
            }
        }
    }
    args.extend([
        "-w".into(),
        "%{http_code} %{url_effective}".into(),
        "-o".into(),
        req.dest.display().to_string(),
        req.url.into(),
    ]);
    args
}

/// Node's `fetch`, run as `node -e SCRIPT url dest stall_secs overall_secs`.
/// It sends Node's default headers (no `User-Agent` of a browser is claimed),
/// follows redirects (http and https only), streams the body to `dest`, and
/// prints `status final-url` like curl's `-w`. No bytes for `stall_secs` aborts
/// with exit 28, as curl's stall does; 78 means this Node has no `fetch`.
const NODE_FETCH: &str = r"
const [url, dest, stall, overall] = [process.argv[1], process.argv[2], +process.argv[3] * 1000, +process.argv[4] * 1000];
if (typeof fetch !== 'function') process.exit(78);
if (!/^https?:\/\//i.test(url)) process.exit(2);
const fs = require('fs');
const ac = new AbortController();
let timer;
const arm = () => { clearTimeout(timer); timer = setTimeout(() => ac.abort(), stall); };
setTimeout(() => ac.abort(), overall).unref();
(async () => {
  try {
    arm();
    const r = await fetch(url, { signal: ac.signal, redirect: 'follow' });
    const fd = fs.openSync(dest, 'w');
    if (r.body) {
      const rd = r.body.getReader();
      for (;;) {
        arm();
        const { done, value } = await rd.read();
        if (done) break;
        for (let at = 0; at < value.length;) at += fs.writeSync(fd, value, at);
      }
    }
    fs.closeSync(fd);
    process.stdout.write(r.status + ' ' + r.url);
    process.exit(0);
  } catch (e) {
    process.stderr.write(String((e && e.cause && (e.cause.code || e.cause.message)) || (e && e.message) || e));
    process.exit(ac.signal.aborted ? 28 : 1);
  }
})();
";

/// Whether a Node with `fetch` (18 or later) is on the `PATH`, asked once.
fn node_available() -> bool {
    static NODE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *NODE.get_or_init(|| {
        Command::new("node")
            .args(["-e", "process.exit(typeof fetch === 'function' ? 0 : 1)"])
            .stdin(std::process::Stdio::null())
            .output()
            .is_ok_and(|o| o.status.success())
    })
}

/// The first line of a program's stderr, kept short for an error line.
fn first_line(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    line.trim().chars().take(100).collect()
}

fn parse_status_line(said: &str) -> Response {
    let said = said.trim();
    let (status, final_url) = said.split_once(' ').unwrap_or((said, ""));
    Response {
        status: status.parse().unwrap_or(0),
        final_url: final_url.to_string(),
    }
}

impl CurlHttp {
    fn run(program: &str, args: &[String], stderr_label: &str) -> Result<Response, TransportError> {
        let out = Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|e| TransportError {
                code: None,
                message: format!("cannot run `{program}`: {e}"),
            })?;
        if !out.status.success() {
            let why = first_line(&out.stderr);
            return Err(TransportError {
                code: out.status.code(),
                message: match out.status.code() {
                    Some(code) if why.is_empty() => format!("{stderr_label} exit {code}"),
                    Some(code) => format!("{stderr_label} exit {code}: {why}"),
                    None => format!("{stderr_label} was killed"),
                },
            });
        }
        Ok(parse_status_line(&String::from_utf8_lossy(&out.stdout)))
    }
}

impl Http for CurlHttp {
    fn get(&self, req: &Request<'_>) -> Result<Response, TransportError> {
        match req.strategy.client() {
            Client::Curl => {
                let timeouts = self.curl_timeouts(node_available());
                Self::run("curl", &curl_args(req, &timeouts), "curl")
            }
            Client::Node => {
                let args = [
                    "-e".to_owned(),
                    NODE_FETCH.to_owned(),
                    req.url.to_owned(),
                    req.dest.display().to_string(),
                    self.timeouts.stall_secs.max(1).to_string(),
                    self.timeouts.overall_secs.to_string(),
                ];
                Self::run("node", &args, "node")
            }
        }
    }

    fn pause(&self, how_long: Duration) {
        std::thread::sleep(how_long);
    }

    fn available(&self, strategy: Strategy) -> bool {
        strategy.client() == Client::Curl || node_available()
    }

    fn memory(&self) -> Option<&StrategyMemory> {
        Some(&self.memory)
    }
}

/// `scheme://host[:port]` of an http(s) URL.
fn origin_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let host = rest
        .split(['/', '?', '#'])
        .next()
        .filter(|h| !h.is_empty())?;
    matches!(scheme, "http" | "https").then(|| format!("{scheme}://{host}"))
}

/// Resolve `href` (as found on the page at `base`) to an absolute http(s) URL.
#[must_use]
pub fn resolve_url(base: &str, href: &str) -> Option<String> {
    let href = href.trim();
    if href.starts_with("http://") || href.starts_with("https://") {
        return Some(href.to_string());
    }
    if href.is_empty() || href.starts_with('#') || href.contains(':') && !href.starts_with("//") {
        // mailto:, javascript:, data: and the like
        return None;
    }
    let origin = origin_of(base)?;
    if let Some(rest) = href.strip_prefix("//") {
        let scheme = origin.split_once("://")?.0;
        return Some(format!("{scheme}://{rest}"));
    }
    if href.starts_with('/') {
        return Some(format!("{origin}{href}"));
    }
    // Relative to the base's directory (query and fragment dropped).
    let path_start = origin.len();
    let path = base[path_start..].split(['?', '#']).next().unwrap_or("");
    let dir = path.rfind('/').map_or("/", |i| &path[..=i]);
    Some(format!("{origin}{dir}{href}"))
}

/// Whether `url` aims at somewhere only this machine or its network can reach.
///
/// Loopback, private and link-local literals and `localhost`: never a
/// manufacturer's datasheet, and not something an agent's answer may aim a
/// download at.
#[must_use]
pub fn is_local_host(url: &str) -> bool {
    let Some(origin) = origin_of(url) else {
        return true;
    };
    let authority = origin.split_once("://").map_or("", |(_, a)| a);
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, h)| h)
        .trim_start_matches('[');
    let host = host.split([']', '/']).next().unwrap_or("");
    let host = host
        .rsplit_once(':')
        .filter(|(h, _)| !h.contains(':'))
        .map_or(host, |(h, _)| h);
    let lower = host.to_ascii_lowercase();
    if lower == "localhost"
        || lower.ends_with(".localhost")
        || lower.rsplit('.').next() == Some("local")
        || lower.contains(':')
    {
        return true;
    }
    let octets: Vec<u8> = lower.split('.').filter_map(|o| o.parse().ok()).collect();
    if octets.len() == 4 {
        return matches!(
            octets[..],
            [10 | 127 | 0, ..] | [169, 254, ..] | [192, 168, ..]
        ) || (octets[0] == 172 && (16..=31).contains(&octets[1]));
    }
    false
}

// ── Landing pages ─────────────────────────────────────────────────────────────

/// One link on an HTML page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageLink {
    pub url: String,
    pub text: String,
}

/// Whether a downloaded body is an HTML page rather than a PDF or other file.
#[must_use]
pub fn looks_like_html(head: &[u8]) -> bool {
    let head = String::from_utf8_lossy(&head[..head.len().min(4096)]).to_ascii_lowercase();
    ["<!doctype html", "<html", "<head", "<body"]
        .iter()
        .any(|tag| head.contains(tag))
}

fn decode_entities(text: &str) -> String {
    text.replace("&amp;", "&")
        .replace("&#38;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}

/// The value of attribute `name` in the text of one tag.
fn attribute(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(at) = lower[from..].find(name) {
        let start = from + at;
        from = start + name.len();
        let before_ok = start == 0 || lower.as_bytes()[start - 1].is_ascii_whitespace();
        let rest = tag[from..].trim_start();
        let Some(rest) = rest.strip_prefix('=').filter(|_| before_ok) else {
            continue;
        };
        let rest = rest.trim_start();
        let value = match rest.chars().next()? {
            q @ ('"' | '\'') => rest[1..].split(q).next()?,
            _ => rest.split(|c: char| c.is_whitespace() || c == '>').next()?,
        };
        return Some(decode_entities(value));
    }
    None
}

/// Every PDF link on the page at `base`, in page order, deduplicated by URL.
/// A link is a PDF when its address (before any `#`) holds `.pdf` in its path
/// or query.
#[must_use]
pub fn pdf_links(html: &str, base: &str) -> Vec<PageLink> {
    let lower = html.to_ascii_lowercase();
    let mut links: Vec<PageLink> = Vec::new();
    let mut at = 0;
    while let Some(found) = lower[at..].find("<a") {
        let tag_start = at + found;
        at = tag_start + 2;
        if !lower[at..].starts_with(|c: char| c.is_ascii_whitespace()) {
            continue;
        }
        let Some(tag_len) = lower[tag_start..].find('>') else {
            break;
        };
        let tag = &html[tag_start..tag_start + tag_len];
        let body_start = tag_start + tag_len + 1;
        let body_end = lower[body_start..]
            .find("</a")
            .map_or(html.len(), |i| body_start + i);
        let Some(href) = attribute(tag, "href") else {
            continue;
        };
        let address = href.split('#').next().unwrap_or("");
        if !address.to_ascii_lowercase().contains(".pdf") {
            continue;
        }
        let Some(url) = resolve_url(base, address) else {
            continue;
        };
        if links.iter().any(|l| l.url == url) {
            continue;
        }
        let mut text = String::new();
        let mut in_tag = false;
        for c in html[body_start..body_end].chars() {
            match c {
                '<' => in_tag = true,
                '>' => in_tag = false,
                _ if !in_tag => text.push(c),
                _ => {}
            }
        }
        let text = decode_entities(&text)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        links.push(PageLink { url, text });
    }
    links
}

/// Words that mark a PDF as some other document than the datasheet.
const NOT_A_DATASHEET: &[&str] = &[
    "errata",
    "rohs",
    "reach",
    "tape",
    "reel",
    "packing",
    "packaging",
    "quality",
    "reliability",
    "moisture",
    "certificate",
    "brochure",
    "catalog",
    "catalogue",
    "selection-guide",
    "selection guide",
    "app-note",
    "appnote",
    "application-note",
    "application note",
    "user-guide",
    "user guide",
    "declaration",
    "pcn",
    "eol",
    "die-size",
];

/// The one link a landing page offers for `part`'s datasheet, if there is one.
///
/// Exactly one: the page's only plausible datasheet PDF, or, among several,
/// the only one whose address or text holds the part number (or its family
/// stem). Never a guess between candidates.
#[must_use]
pub fn pick_datasheet_link<'a>(links: &'a [PageLink], part: &str) -> Option<&'a PageLink> {
    let plausible: Vec<&PageLink> = links
        .iter()
        .filter(|l| {
            let both = format!("{} {}", l.url, l.text).to_ascii_lowercase();
            !NOT_A_DATASHEET.iter().any(|w| both.contains(w))
        })
        .collect();
    if let [only] = plausible[..] {
        return Some(only);
    }
    let wanted: Vec<String> = [
        Some(crate::datasheet::squash(part)),
        crate::datasheet::stem_of(part),
    ]
    .into_iter()
    .flatten()
    .filter(|w| w.len() >= 3)
    .collect();
    let names: Vec<&PageLink> = plausible
        .into_iter()
        .filter(|l| {
            let both = crate::datasheet::squash(&format!("{} {}", l.url, l.text));
            wanted.iter().any(|w| both.contains(w.as_str()))
        })
        .collect();
    match names[..] {
        [only] => Some(only),
        _ => None,
    }
}

// ── The fetch ─────────────────────────────────────────────────────────────────

/// Why a URL produced no PDF.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    /// HTTP 401, 403 or 429 after the retries: the site blocks automated
    /// downloads or asks for a person.
    Blocked { url: String, status: u16 },
    /// Any other HTTP error status.
    Status { url: String, status: u16 },
    /// A page answered 200 but it is not a PDF and offers no single datasheet
    /// link; `links` is how many PDF links it showed.
    LandingPage { url: String, links: usize },
    /// The link a landing page offered was itself not a PDF.
    LinkNotPdf { page: String, link: String },
    /// Something else that is not a PDF came back.
    NotPdf { url: String },
    /// curl could not complete, even after its retry.
    Transport { url: String, message: String },
    /// Several strategies were tried and none produced a PDF. `best` is the
    /// failure that got furthest (the one that says most about the host), and
    /// `tried` is every strategy with a few words on how it failed.
    Tried {
        best: Box<Self>,
        tried: Vec<(Strategy, String)>,
    },
}

impl FetchError {
    /// The URL that failed.
    #[must_use]
    pub fn url(&self) -> &str {
        match self {
            Self::Blocked { url, .. }
            | Self::Status { url, .. }
            | Self::LandingPage { url, .. }
            | Self::NotPdf { url }
            | Self::Transport { url, .. } => url,
            Self::LinkNotPdf { page, .. } => page,
            Self::Tried { best, .. } => best.url(),
        }
    }

    /// The single failure behind this one: itself, or the best of the several
    /// strategies tried. What a host ledger learns from.
    #[must_use]
    pub fn representative(&self) -> &Self {
        match self {
            Self::Tried { best, .. } => best.representative(),
            other => other,
        }
    }

    /// The failure in a few words, for a list of strategies.
    #[must_use]
    pub fn brief(&self) -> String {
        match self {
            Self::Blocked { status, .. } | Self::Status { status, .. } => format!("HTTP {status}"),
            Self::LandingPage { .. } => "a web page, no single datasheet link".into(),
            Self::LinkNotPdf { .. } => "a web page whose link was no PDF".into(),
            Self::NotPdf { .. } => "not a PDF".into(),
            Self::Transport { message, .. } => message.clone(),
            Self::Tried { best, .. } => best.brief(),
        }
    }

    /// How far a failure got, to pick the one that says most about the host: an
    /// answer that is no refusal (a dead link, a page) beats a refusal, which
    /// beats not being answered at all.
    fn reach(&self) -> u8 {
        match self {
            Self::Blocked { .. } => 2,
            Self::Transport { .. } => 1,
            Self::Status { status, .. } if *status >= 500 => 1,
            Self::Tried { best, .. } => best.reach(),
            _ => 3,
        }
    }

    /// Another strategy would meet the same wall: a rate limit or a server error
    /// that outlasted its retries.
    const fn ends_all_strategies(&self) -> bool {
        matches!(
            self,
            Self::Blocked { status: 429, .. } | Self::Status { status: 500.., .. }
        )
    }

    /// The failure as one clause, for a line that then says what happens next.
    #[must_use]
    pub fn why(&self) -> String {
        match self {
            Self::Blocked { status, .. } => format!(
                "the site refused the download (HTTP {status}): it blocks automated fetches or asks for a person"
            ),
            Self::Status { status, .. } => format!("the server answered HTTP {status}"),
            Self::LandingPage { links: 0, .. } => {
                "it is a web page with no PDF link, not a PDF".into()
            }
            Self::LandingPage { links, .. } => format!(
                "it is a web page offering {links} PDF links with no single datasheet among them, not a PDF"
            ),
            Self::LinkNotPdf { link, .. } => {
                format!("it is a web page, and its datasheet link {link} did not serve a PDF either")
            }
            Self::NotPdf { .. } => "it did not serve a PDF".into(),
            Self::Transport { message, .. } => message.clone(),
            Self::Tried { best, tried } => {
                let same = tried.windows(2).all(|w| w[0].1 == w[1].1);
                let list = tried
                    .iter()
                    .map(|(s, how)| {
                        if same {
                            s.label().to_string()
                        } else {
                            format!("{}: {how}", s.label())
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{} (tried {list})", best.why())
            }
        }
    }
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.url(), self.why())
    }
}

impl std::error::Error for FetchError {}

/// How a PDF was reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Via {
    /// The URL served the PDF itself.
    Direct,
    /// The URL was a landing page; its single datasheet link was followed.
    LandingPage { page: String },
}

/// A downloaded PDF.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fetched {
    pub path: PathBuf,
    /// The PDF's own URL (after redirects, or the landing page's link).
    pub url: String,
    pub via: Via,
}

const fn transient_status(status: u16) -> bool {
    matches!(status, 429 | 502 | 503 | 504)
}

/// The pauses before the first and second retry of a transient failure.
const BACKOFF: [Duration; 2] = [Duration::from_secs(2), Duration::from_secs(6)];

/// One URL with one strategy: a transient failure or status retried twice
/// (after a pause that grows), the first retry over HTTP/1.1. A stall is not
/// retried; the caller's next client is the remedy.
fn get_with_retries(
    http: &dyn Http,
    strategy: Strategy,
    url: &str,
    dest: &Path,
) -> Result<Response, TransportError> {
    let (mut http1, mut retries) = (false, 0);
    loop {
        let attempt = http.get(&Request {
            url,
            dest,
            strategy,
            http1,
        });
        let transient = match &attempt {
            Ok(r) => transient_status(r.status),
            Err(e) => e.transient() && !e.stalled(),
        };
        if !transient || retries >= BACKOFF.len() {
            return attempt;
        }
        http1 = true;
        http.pause(BACKOFF[retries]);
        retries += 1;
    }
}

enum Landed {
    Pdf(String),
    Page { url: String, html: String },
}

/// Fetch `url` once with `strategy` (and its retries) and say what landed in
/// `dest`.
fn land(http: &dyn Http, strategy: Strategy, url: &str, dest: &Path) -> Result<Landed, FetchError> {
    let _ = std::fs::remove_file(dest);
    let response =
        get_with_retries(http, strategy, url, dest).map_err(|e| FetchError::Transport {
            url: url.to_string(),
            message: e.message,
        })?;
    let url_error = |status| match status {
        401 | 403 | 429 => FetchError::Blocked {
            url: url.to_string(),
            status,
        },
        _ => FetchError::Status {
            url: url.to_string(),
            status,
        },
    };
    if response.status >= 400 {
        let _ = std::fs::remove_file(dest);
        return Err(url_error(response.status));
    }
    let final_url = if response.final_url.is_empty() {
        url.to_string()
    } else {
        response.final_url
    };
    if crate::datasheet::is_pdf(dest) {
        return Ok(Landed::Pdf(final_url));
    }
    let body = std::fs::read(dest).unwrap_or_default();
    let _ = std::fs::remove_file(dest);
    if looks_like_html(&body) && body.len() <= 4 << 20 {
        let html = String::from_utf8_lossy(&body).into_owned();
        return Ok(Landed::Page {
            url: final_url,
            html,
        });
    }
    Err(FetchError::NotPdf {
        url: url.to_string(),
    })
}

/// `url` with one strategy: the landing page's single datasheet link followed,
/// when it is a page.
fn fetch_with(
    http: &dyn Http,
    strategy: Strategy,
    url: &str,
    dest: &Path,
    part: &str,
) -> Result<Fetched, FetchError> {
    match land(http, strategy, url, dest)? {
        Landed::Pdf(final_url) => Ok(Fetched {
            path: dest.to_path_buf(),
            url: final_url,
            via: Via::Direct,
        }),
        Landed::Page { url: page, html } => {
            let links = pdf_links(&html, &page);
            let Some(link) = pick_datasheet_link(&links, part) else {
                return Err(FetchError::LandingPage {
                    url: url.to_string(),
                    links: links.len(),
                });
            };
            match land(http, strategy, &link.url, dest) {
                Ok(Landed::Pdf(final_url)) => Ok(Fetched {
                    path: dest.to_path_buf(),
                    url: final_url,
                    via: Via::LandingPage { page },
                }),
                Ok(Landed::Page { .. }) | Err(FetchError::NotPdf { .. }) => {
                    Err(FetchError::LinkNotPdf {
                        page,
                        link: link.url.clone(),
                    })
                }
                Err(e) => Err(e),
            }
        }
    }
}

/// The strategies to try for `url`, in order: the one that worked for its host
/// before, then the rest cheapest first, leaving out those the transport cannot
/// run.
fn strategy_order(http: &dyn Http, url: &str) -> Vec<Strategy> {
    let first = http.memory().and_then(|m| m.preferred(url));
    first
        .into_iter()
        .chain(Strategy::ALL.into_iter().filter(|s| Some(*s) != first))
        .filter(|s| http.available(*s))
        .collect()
}

/// Download `url` to `dest` as a PDF for `part`, applying the remedies in the
/// module doc. `dest` holds a PDF on `Ok` and nothing otherwise.
///
/// # Errors
///
/// A [`FetchError`] naming the URL and why no PDF came of it, and, when several
/// strategies failed, which they were.
pub fn fetch_pdf(
    http: &dyn Http,
    url: &str,
    dest: &Path,
    part: &str,
) -> Result<Fetched, FetchError> {
    let mut failed: Vec<(Strategy, FetchError)> = Vec::new();
    let mut unreachable: Vec<Client> = Vec::new();
    for strategy in strategy_order(http, url) {
        if unreachable.contains(&strategy.client()) {
            continue;
        }
        match fetch_with(http, strategy, url, dest, part) {
            Ok(got) => {
                if let Some(memory) = http.memory() {
                    memory.worked(url, strategy);
                }
                return Ok(got);
            }
            Err(e) => {
                let _ = std::fs::remove_file(dest);
                if matches!(e, FetchError::Transport { .. }) {
                    unreachable.push(strategy.client());
                }
                let stop = e.ends_all_strategies();
                failed.push((strategy, e));
                if stop {
                    break;
                }
            }
        }
    }
    let Some(best) = failed
        .iter()
        .rev()
        .max_by_key(|(_, e)| e.reach())
        .map(|(_, e)| e.clone())
    else {
        return Err(FetchError::Transport {
            url: url.to_string(),
            message: "no way to download is available".into(),
        });
    };
    if failed.len() == 1 {
        return Err(best);
    }
    Err(FetchError::Tried {
        best: Box::new(best),
        tried: failed.iter().map(|(s, e)| (*s, e.brief())).collect(),
    })
}

// ── Hosts that have said no ───────────────────────────────────────────────────

/// The host of an http(s) URL, lowercased, without credentials or port.
#[must_use]
pub fn host_of(url: &str) -> Option<String> {
    let origin = origin_of(url)?;
    let authority = origin.split_once("://")?.1;
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = host.strip_prefix('[').map_or_else(
        || host.split(':').next().unwrap_or(""),
        |rest| rest.split(']').next().unwrap_or(""),
    );
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// How many times a host may fail to answer (not an HTTP refusal) before it is
/// given up on for the rest of the run.
const UNREACHABLE_AFTER: usize = 2;

#[derive(Debug, Default)]
struct HostRecord {
    /// Why the host is skipped from now on, once it is.
    blocked: Option<String>,
    /// Failures to answer so far.
    silent: usize,
}

/// What a run has learned about the hosts it downloads from.
///
/// A host that refuses automated downloads (HTTP 401, 403 or 429 after the
/// retries) is given up on at once, and one that twice fails to answer (timed
/// out, reset, a server error) after that. Later parts of the same batch then go
/// straight to their next source instead of spending a search and a download to
/// learn it again. A dead link (404) or a page that is not a PDF says nothing
/// about the host. Shared between the batch's threads.
#[derive(Debug, Default)]
pub struct HostLedger {
    hosts: Mutex<HashMap<String, HostRecord>>,
}

impl HostLedger {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, HostRecord>> {
        self.hosts.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Why `url`'s host is being skipped, if it is.
    #[must_use]
    pub fn refusal(&self, url: &str) -> Option<String> {
        let host = host_of(url)?;
        self.lock().get(&host)?.blocked.clone()
    }

    /// Record how a download from `url`'s host went. `Some(reason)` when this
    /// call is what made the host skipped from now on.
    pub fn note(&self, url: &str, result: &Result<(), &FetchError>) -> Option<String> {
        let host = host_of(url)?;
        let mut hosts = self.lock();
        let record = hosts.entry(host).or_default();
        let before = record.blocked.is_some();
        match result.as_ref().map_err(|e| e.representative()) {
            Ok(()) => record.silent = 0,
            Err(FetchError::Blocked { status, .. }) => {
                record.blocked.get_or_insert_with(|| {
                    format!("it refused an automated download (HTTP {status})")
                });
            }
            Err(FetchError::Transport { .. }) => record.silent += 1,
            Err(FetchError::Status { status, .. }) if *status >= 500 => record.silent += 1,
            Err(_) => {}
        }
        if record.silent >= UNREACHABLE_AFTER {
            record.blocked.get_or_insert_with(|| {
                format!(
                    "it failed to answer {} times (timed out or reset)",
                    record.silent
                )
            });
        }
        let blocked = record.blocked.clone();
        drop(hosts);
        if before {
            None
        } else {
            blocked
        }
    }

    /// Every host skipped so far with its reason, sorted by host.
    #[must_use]
    pub fn skipped(&self) -> Vec<(String, String)> {
        let mut all: Vec<(String, String)> = self
            .lock()
            .iter()
            .filter_map(|(h, r)| Some((h.clone(), r.blocked.clone()?)))
            .collect();
        all.sort();
        all
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex as StdMutex;

    /// What a scripted server does on one request.
    enum Step {
        /// Answer with this status, writing `body` to the destination.
        Answer(u16, &'static [u8]),
        /// Fail like curl with this exit code.
        Curl(i32),
    }

    /// A transport that plays `steps` in order and records each request.
    struct Scripted {
        steps: StdMutex<VecDeque<(&'static str, Step)>>,
        seen: StdMutex<Vec<(String, Strategy, bool)>>,
        paused: StdMutex<Vec<Duration>>,
        /// The strategies it can run (`new` runs only the first).
        runs: Vec<Strategy>,
        memory: StrategyMemory,
    }

    impl Scripted {
        /// One strategy only (plain `curl`): the retry rules, free of fallbacks.
        fn new(steps: Vec<(&'static str, Step)>) -> Self {
            Self::running(&[Strategy::CurlPlain], steps)
        }

        /// Every strategy, as on a machine with `node`.
        fn all(steps: Vec<(&'static str, Step)>) -> Self {
            Self::running(&Strategy::ALL, steps)
        }

        fn running(runs: &[Strategy], steps: Vec<(&'static str, Step)>) -> Self {
            Self {
                steps: StdMutex::new(steps.into()),
                seen: StdMutex::default(),
                paused: StdMutex::default(),
                runs: runs.to_vec(),
                memory: StrategyMemory::default(),
            }
        }

        /// The strategies of the requests so far, in order.
        fn strategies(&self) -> Vec<Strategy> {
            self.seen.lock().unwrap().iter().map(|s| s.1).collect()
        }
    }

    impl Http for Scripted {
        fn pause(&self, how_long: Duration) {
            self.paused.lock().unwrap().push(how_long);
        }

        fn available(&self, strategy: Strategy) -> bool {
            self.runs.contains(&strategy)
        }

        fn memory(&self) -> Option<&StrategyMemory> {
            Some(&self.memory)
        }

        fn get(&self, req: &Request<'_>) -> Result<Response, TransportError> {
            self.seen
                .lock()
                .unwrap()
                .push((req.url.to_string(), req.strategy, req.http1));
            let (expected, step) = self
                .steps
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| panic!("unscripted request for {}", req.url));
            assert_eq!(expected, req.url, "requests come in the scripted order");
            match step {
                Step::Answer(status, body) => {
                    std::fs::write(req.dest, body).unwrap();
                    Ok(Response {
                        status,
                        final_url: req.url.to_string(),
                    })
                }
                Step::Curl(code) => Err(TransportError {
                    code: Some(code),
                    message: format!("curl failed: exit {code}"),
                }),
            }
        }
    }

    const PDF: &[u8] = b"%PDF-1.4\nbody";

    fn dest() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.pdf");
        (dir, path)
    }

    fn page(links: &[(&str, &str)]) -> Vec<u8> {
        use std::fmt::Write as _;
        let mut anchors = String::new();
        for (href, text) in links {
            let _ = write!(anchors, "<a class=\"d\" href=\"{href}\">{text}</a>");
        }
        format!("<!DOCTYPE html><html><body><p>Product</p>{anchors}</body></html>").into_bytes()
    }

    /// curl 92 ("HTTP/2 stream not closed cleanly") was the commonest failure.
    /// It is retried once, and the retry is over HTTP/1.1.
    #[test]
    fn curl_92_is_retried_once_over_http1() {
        let (_d, path) = dest();
        let http = Scripted::new(vec![
            ("https://a.example/ds.pdf", Step::Curl(92)),
            ("https://a.example/ds.pdf", Step::Answer(200, PDF)),
        ]);
        let got = fetch_pdf(&http, "https://a.example/ds.pdf", &path, "LM358").unwrap();
        assert_eq!(got.via, Via::Direct);
        let seen = http.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 2);
        assert!(
            !seen[0].2 && seen[1].2,
            "second attempt is HTTP/1.1: {seen:?}"
        );
        assert!(curl_args(
            &Request {
                url: "u",
                dest: &path,
                strategy: Strategy::CurlStandard,
                http1: true,
            },
            &Timeouts::default(),
        )
        .contains(&"--http1.1".to_string()));
    }

    /// Other transient failures get the same single retry, and no more: a
    /// second failure is reported, not looped on.
    #[test]
    fn a_transient_failure_gets_one_retry_and_then_is_reported() {
        let (_d, path) = dest();
        let http = Scripted::new(vec![
            ("https://a.example/ds.pdf", Step::Curl(56)),
            ("https://a.example/ds.pdf", Step::Curl(56)),
            ("https://a.example/ds.pdf", Step::Curl(56)),
        ]);
        let err = fetch_pdf(&http, "https://a.example/ds.pdf", &path, "LM358").unwrap_err();
        assert!(matches!(err, FetchError::Transport { .. }), "{err}");
        assert_eq!(http.seen.lock().unwrap().len(), 3, "two retries, no more");
        // A permanent curl error (6: cannot resolve the host) is not retried.
        let http = Scripted::new(vec![("https://a.example/ds.pdf", Step::Curl(6))]);
        assert!(fetch_pdf(&http, "https://a.example/ds.pdf", &path, "LM358").is_err());
        assert_eq!(http.seen.lock().unwrap().len(), 1);
        // A 503 is transient too.
        let http = Scripted::new(vec![
            ("https://a.example/ds.pdf", Step::Answer(503, b"busy")),
            ("https://a.example/ds.pdf", Step::Answer(200, PDF)),
        ]);
        assert!(fetch_pdf(&http, "https://a.example/ds.pdf", &path, "LM358").is_ok());
    }

    const U: &str = "https://b.example/ds.pdf";

    /// The strategies run cheapest first and the first verified PDF stops them:
    /// curl's own `User-Agent`, then a browser's, then the fuller set, then node.
    #[test]
    fn strategies_run_cheapest_first_and_stop_at_the_first_pdf() {
        let (_d, path) = dest();
        let http = Scripted::all(vec![
            (U, Step::Answer(404, b"no")),
            (U, Step::Answer(403, b"no")),
            (U, Step::Answer(404, b"no")),
            (U, Step::Answer(200, PDF)),
        ]);
        let got = fetch_pdf(&http, U, &path, "LM358").unwrap();
        assert_eq!(got.via, Via::Direct);
        assert_eq!(http.strategies(), Strategy::ALL);
        assert!(http.paused.lock().unwrap().is_empty(), "no pause for a 404");
        let http = Scripted::all(vec![(U, Step::Answer(200, PDF))]);
        assert!(fetch_pdf(&http, U, &path, "LM358").is_ok());
        assert_eq!(http.strategies(), [Strategy::CurlPlain], "one request");
        // A site that refuses curl's own agent and takes a browser's: two requests.
        let http = Scripted::all(vec![
            (U, Step::Answer(403, b"no")),
            (U, Step::Answer(200, PDF)),
        ]);
        assert!(fetch_pdf(&http, U, &path, "LM358").is_ok());
        assert_eq!(
            http.strategies(),
            [Strategy::CurlPlain, Strategy::CurlStandard]
        );
    }

    /// A dead link or refusal on one strategy does not end the others (a vendor
    /// that 404s a browser's agent serves curl's own).
    #[test]
    fn an_http_error_falls_back_to_the_next_strategy_then_reports_all() {
        let (_d, path) = dest();
        let http = Scripted::all(vec![
            (U, Step::Answer(403, b"no")),
            (U, Step::Answer(403, b"no")),
            (U, Step::Answer(403, b"no")),
            (U, Step::Answer(403, b"no")),
        ]);
        let err = fetch_pdf(&http, U, &path, "LM358").unwrap_err();
        assert_eq!(
            err.representative(),
            &FetchError::Blocked {
                url: U.into(),
                status: 403
            }
        );
        assert_eq!(err.url(), U);
        assert!(!path.exists(), "a failed fetch leaves no file");
        let args = curl_args(
            &Request {
                url: "https://b.example/x/ds.pdf",
                dest: &path,
                strategy: Strategy::CurlBrowser,
                http1: false,
            },
            &Timeouts::default(),
        );
        assert!(
            args.contains(&"Referer: https://b.example/".to_string()),
            "{args:?}"
        );
        assert!(args.contains(&"Accept-Language: en-US,en;q=0.9".to_string()));
    }

    /// curl's own request carries no `User-Agent` of a browser and no extra
    /// headers; the other curl sets do.
    #[test]
    fn plain_curl_sends_its_own_headers() {
        let plain = args_of(Strategy::CurlPlain, &Timeouts::default());
        assert!(!plain.contains(&"-A".to_string()), "{plain:?}");
        assert!(!plain.contains(&"-H".to_string()), "{plain:?}");
        assert!(plain.contains(&"--compressed".to_string()));
        for s in [Strategy::CurlStandard, Strategy::CurlBrowser] {
            assert!(args_of(s, &Timeouts::default()).contains(&"-A".to_string()));
        }
    }

    /// A reset (curl 92) is a fact about the TLS stack, not the headers: after
    /// its retries the other curl strategies are skipped and node is tried. A
    /// stall is not retried at all.
    #[test]
    fn a_transport_failure_skips_the_same_client_and_tries_the_next() {
        let (_d, path) = dest();
        let http = Scripted::all(vec![
            (U, Step::Curl(92)),
            (U, Step::Curl(92)),
            (U, Step::Curl(92)),
            (U, Step::Answer(200, PDF)),
        ]);
        assert!(fetch_pdf(&http, U, &path, "LM358").is_ok());
        assert_eq!(
            http.strategies(),
            [
                Strategy::CurlPlain,
                Strategy::CurlPlain,
                Strategy::CurlPlain,
                Strategy::NodeFetch
            ]
        );
        assert_eq!(*http.paused.lock().unwrap(), BACKOFF.to_vec());
        let http = Scripted::all(vec![(U, Step::Curl(28)), (U, Step::Answer(200, PDF))]);
        assert!(fetch_pdf(&http, U, &path, "LM358").is_ok());
        assert_eq!(
            http.strategies(),
            [Strategy::CurlPlain, Strategy::NodeFetch],
            "a stall costs one wait, not one per curl strategy"
        );
        assert!(http.paused.lock().unwrap().is_empty());
    }

    /// A quiet `curl` is waited for less when node can take over, and the full
    /// stall time otherwise.
    #[test]
    fn a_quiet_curl_is_handed_to_node_sooner() {
        let http = CurlHttp::default();
        assert_eq!(http.curl_timeouts(true).stall_secs, HANDOFF_STALL_SECS);
        assert_eq!(http.curl_timeouts(false).stall_secs, 60);
        assert_eq!(http.curl_timeouts(true).overall_secs, 900);
        let short = CurlHttp::with_timeouts(Timeouts {
            connect_secs: 1,
            stall_secs: 2,
            overall_secs: 3,
        });
        assert_eq!(short.curl_timeouts(true).stall_secs, 2, "never lengthened");
    }

    /// Without node the curl strategies still run and node is never asked for;
    /// the error names only what was tried.
    #[test]
    fn a_missing_optional_program_is_skipped_quietly() {
        let (_d, path) = dest();
        let http = Scripted::running(
            &[
                Strategy::CurlPlain,
                Strategy::CurlStandard,
                Strategy::CurlBrowser,
            ],
            vec![
                (U, Step::Answer(404, b"gone")),
                (U, Step::Answer(404, b"gone")),
                (U, Step::Answer(404, b"gone")),
            ],
        );
        let err = fetch_pdf(&http, U, &path, "LM358").unwrap_err();
        assert!(!http.strategies().contains(&Strategy::NodeFetch));
        assert!(!err.why().contains("node"), "{}", err.why());
        assert!(err.why().contains("curl"), "{}", err.why());
    }

    /// A rate limit or server error that outlasts its retries ends the attempt:
    /// another client would meet the same wall, and hammering it is rude.
    #[test]
    fn a_rate_limit_ends_the_attempt() {
        let (_d, path) = dest();
        let http = Scripted::all(vec![
            (U, Step::Answer(429, b"slow down")),
            (U, Step::Answer(429, b"slow down")),
            (U, Step::Answer(429, b"slow down")),
        ]);
        let err = fetch_pdf(&http, U, &path, "LM358").unwrap_err();
        assert_eq!(
            err,
            FetchError::Blocked {
                url: U.into(),
                status: 429
            }
        );
        assert_eq!(http.strategies(), [Strategy::CurlPlain; 3]);
    }

    /// The strategy that worked is remembered per host and tried first next
    /// time; another host starts from the beginning; a remembered strategy that
    /// stops working falls back to the rest.
    #[test]
    fn the_strategy_that_worked_for_a_host_is_tried_first_next_time() {
        let (_d, path) = dest();
        let (u1, u2) = ("https://h.example/a.pdf", "https://h.example/b.pdf");
        let http = Scripted::all(vec![
            (u1, Step::Curl(28)),
            (u1, Step::Answer(200, PDF)),
            (u2, Step::Answer(200, PDF)),
            ("https://other.example/c.pdf", Step::Answer(200, PDF)),
            ("https://h.example/d.pdf", Step::Answer(404, b"gone")),
            ("https://h.example/d.pdf", Step::Answer(200, PDF)),
        ]);
        assert!(fetch_pdf(&http, u1, &path, "LM358").is_ok());
        assert_eq!(http.memory.preferred(u2), Some(Strategy::NodeFetch));
        assert!(fetch_pdf(&http, u2, &path, "LM358").is_ok());
        assert!(fetch_pdf(&http, "https://other.example/c.pdf", &path, "LM358").is_ok());
        assert!(fetch_pdf(&http, "https://h.example/d.pdf", &path, "LM358").is_ok());
        assert_eq!(
            http.strategies(),
            [
                Strategy::CurlPlain,
                Strategy::NodeFetch,
                Strategy::NodeFetch,
                Strategy::CurlPlain,
                Strategy::NodeFetch,
                Strategy::CurlPlain,
            ]
        );
        // A failure teaches nothing: the host keeps the strategy that worked.
        let http = Scripted::all(vec![
            (U, Step::Answer(404, b"gone")),
            (U, Step::Answer(404, b"gone")),
            (U, Step::Answer(404, b"gone")),
            (U, Step::Answer(404, b"gone")),
        ]);
        assert!(fetch_pdf(&http, U, &path, "LM358").is_err());
        assert_eq!(http.memory.preferred(U), None);
    }

    /// A page that offers no single datasheet link may be bot protection's
    /// page: the next strategy is tried before giving up.
    #[test]
    fn a_page_without_a_datasheet_link_tries_the_next_strategy() {
        let (_d, path) = dest();
        let html: &'static [u8] = Box::leak(page(&[("/buy", "Buy")]).into_boxed_slice());
        let http = Scripted::all(vec![
            (U, Step::Answer(200, html)),
            (U, Step::Answer(200, PDF)),
        ]);
        let got = fetch_pdf(&http, U, &path, "LM358").unwrap();
        assert_eq!(got.via, Via::Direct);
        assert_eq!(
            http.strategies(),
            [Strategy::CurlPlain, Strategy::CurlStandard]
        );
    }

    /// The error for a part names the strategies tried on one short line.
    #[test]
    fn the_error_names_the_strategies_tried_in_one_short_line() {
        let (_d, path) = dest();
        let http = Scripted::all(vec![
            (U, Step::Answer(404, b"gone")),
            (U, Step::Answer(404, b"gone")),
            (U, Step::Answer(404, b"gone")),
            (U, Step::Answer(404, b"gone")),
        ]);
        let err = fetch_pdf(&http, U, &path, "LM358").unwrap_err();
        let line = err.to_string();
        assert!(!line.contains('\n') && line.len() < 220, "{line}");
        assert!(line.starts_with(U), "{line}");
        for s in Strategy::ALL {
            assert!(line.contains(s.label()), "{line}");
        }
        assert!(line.contains("HTTP 404"), "{line}");
        // Different outcomes are shown per strategy.
        let http = Scripted::all(vec![
            (U, Step::Answer(403, b"no")),
            (U, Step::Answer(404, b"gone")),
            (U, Step::Answer(403, b"no")),
            (U, Step::Curl(1)),
        ]);
        let line = fetch_pdf(&http, U, &path, "LM358").unwrap_err().to_string();
        assert!(line.contains("curl: HTTP 403"), "{line}");
        assert!(line.contains("node fetch: curl failed: exit 1"), "{line}");
        assert!(!line.contains('\n'), "{line}");
    }

    /// A landing page with exactly one datasheet PDF link is followed to it.
    #[test]
    fn a_landing_page_with_one_pdf_link_is_followed() {
        let (_d, path) = dest();
        let html: &'static [u8] = Box::leak(
            page(&[("/lit/ds/lm358.pdf", "Datasheet (PDF)"), ("/buy", "Buy")]).into_boxed_slice(),
        );
        let http = Scripted::new(vec![
            ("https://c.example/product/lm358", Step::Answer(200, html)),
            ("https://c.example/lit/ds/lm358.pdf", Step::Answer(200, PDF)),
        ]);
        let got = fetch_pdf(&http, "https://c.example/product/lm358", &path, "LM358").unwrap();
        assert_eq!(got.url, "https://c.example/lit/ds/lm358.pdf");
        assert_eq!(
            got.via,
            Via::LandingPage {
                page: "https://c.example/product/lm358".into()
            }
        );
    }

    /// Several PDF links: the one naming the part wins; if none or several do,
    /// or all are other documents, there is no pick and nothing is guessed.
    #[test]
    fn a_landing_page_with_several_links_picks_only_the_one_naming_the_part() {
        let base = "https://c.example/p";
        let links = pdf_links(
            std::str::from_utf8(&page(&[
                ("a/errata.pdf", "Errata"),
                ("a/lm358-ds.pdf", "LM358 datasheet"),
                ("a/lm2904-ds.pdf", "LM2904 datasheet"),
            ]))
            .unwrap(),
            base,
        );
        assert_eq!(links.len(), 3);
        let picked = pick_datasheet_link(&links, "LM358DR").unwrap();
        assert_eq!(picked.url, "https://c.example/a/lm358-ds.pdf");
        assert!(
            pick_datasheet_link(&links, "XYZ999").is_none(),
            "no link names it"
        );
        let two = pdf_links(
            std::str::from_utf8(&page(&[
                ("a/one.pdf", "Datasheet"),
                ("a/two.pdf", "Datasheet"),
            ]))
            .unwrap(),
            base,
        );
        assert!(
            pick_datasheet_link(&two, "LM358").is_none(),
            "two plausible, neither names it"
        );
        let none = pdf_links("<html><a href=\"/buy\">Buy</a></html>", base);
        assert!(none.is_empty());
        // The fetch reports it as a landing page with the link count.
        let (_d, path) = dest();
        let html: &'static [u8] = Box::leak(
            page(&[("a/one.pdf", "Datasheet"), ("a/two.pdf", "Datasheet")]).into_boxed_slice(),
        );
        let http = Scripted::new(vec![("https://c.example/p", Step::Answer(200, html))]);
        let err = fetch_pdf(&http, "https://c.example/p", &path, "LM358").unwrap_err();
        assert_eq!(
            err,
            FetchError::LandingPage {
                url: "https://c.example/p".into(),
                links: 2
            }
        );
        assert!(err.why().contains("2 PDF links"));
    }

    /// A page that is not a PDF and not HTML, and a followed link that is not a
    /// PDF, are errors, never accepted.
    #[test]
    fn something_that_is_not_a_pdf_is_refused() {
        let (_d, path) = dest();
        let http = Scripted::new(vec![(
            "https://d.example/f",
            Step::Answer(200, b"PK\x03\x04zip"),
        )]);
        assert!(matches!(
            fetch_pdf(&http, "https://d.example/f", &path, "LM358"),
            Err(FetchError::NotPdf { .. })
        ));
        let html: &'static [u8] = Box::leak(page(&[("/x.pdf", "Datasheet")]).into_boxed_slice());
        let http = Scripted::new(vec![
            ("https://d.example/p", Step::Answer(200, html)),
            ("https://d.example/x.pdf", Step::Answer(200, html)),
        ]);
        assert!(matches!(
            fetch_pdf(&http, "https://d.example/p", &path, "LM358"),
            Err(FetchError::LinkNotPdf { .. })
        ));
        assert!(!path.exists());
    }

    #[test]
    fn urls_resolve_the_way_a_browser_resolves_them() {
        let base = "https://h.example/a/b/page.html?x=1#top";
        assert_eq!(
            resolve_url(base, "/ds/x.pdf").unwrap(),
            "https://h.example/ds/x.pdf"
        );
        assert_eq!(
            resolve_url(base, "x.pdf").unwrap(),
            "https://h.example/a/b/x.pdf"
        );
        assert_eq!(
            resolve_url(base, "//cdn.example/x.pdf").unwrap(),
            "https://cdn.example/x.pdf"
        );
        assert_eq!(
            resolve_url(base, "http://o.example/x.pdf").unwrap(),
            "http://o.example/x.pdf"
        );
        assert!(resolve_url(base, "mailto:a@b.c").is_none());
        assert!(resolve_url(base, "javascript:void(0)").is_none());
    }

    /// Only the manufacturer's public internet is a download target.
    #[test]
    fn local_and_private_hosts_are_not_download_targets() {
        for url in [
            "http://localhost/x.pdf",
            "http://127.0.0.1:8080/x.pdf",
            "http://10.1.2.3/x.pdf",
            "http://192.168.0.5/x.pdf",
            "http://172.20.0.1/x.pdf",
            "http://169.254.169.254/latest",
            "http://[::1]/x.pdf",
            "http://printer.local/x.pdf",
            "ftp://a.example/x.pdf",
        ] {
            assert!(is_local_host(url), "{url}");
        }
        for url in [
            "https://www.ti.com/lit/ds/symlink/lm358b.pdf",
            "http://172.32.0.1/x",
            "https://example.com:8443/x.pdf",
        ] {
            assert!(!is_local_host(url), "{url}");
        }
    }

    #[test]
    fn only_http_redirects_are_followed() {
        let (_d, path) = dest();
        let args = curl_args(
            &Request {
                url: "https://a.example/x.pdf",
                dest: &path,
                strategy: Strategy::CurlStandard,
                http1: false,
            },
            &Timeouts::default(),
        );
        let at = args.iter().position(|a| a == "--proto-redir").unwrap();
        assert_eq!(args[at + 1], "=http,https");
    }

    fn args_of(strategy: Strategy, timeouts: &Timeouts) -> Vec<String> {
        let (_d, path) = dest();
        curl_args(
            &Request {
                url: "https://a.example/x.pdf",
                dest: &path,
                strategy,
                http1: false,
            },
            timeouts,
        )
    }

    /// The very first request is a browser's: its user agent, an Accept that
    /// asks for the PDF, and a language. Vendor sites 403 curl's own.
    #[test]
    fn the_first_request_already_carries_browser_headers() {
        let args = args_of(Strategy::CurlStandard, &Timeouts::default());
        let ua = args.iter().position(|a| a == "-A").unwrap();
        assert!(args[ua + 1].starts_with("Mozilla/5.0"), "{args:?}");
        for header in ["Accept: application/pdf", "Accept-Language: en-US"] {
            assert!(
                args.iter().any(|a| a.starts_with(header)),
                "{header}: {args:?}"
            );
        }
        assert!(args.contains(&"--compressed".to_string()));
    }

    /// A server that goes quiet is abandoned after the stall time, and a
    /// download is capped overall, not after a short fixed total.
    #[test]
    fn the_limits_are_a_stall_time_and_a_generous_overall_cap() {
        let args = args_of(Strategy::CurlStandard, &Timeouts::default());
        let value = |flag: &str| {
            let at = args.iter().position(|a| a == flag).unwrap();
            args[at + 1].clone()
        };
        assert_eq!(value("--connect-timeout"), "20");
        // "No bytes at all" for the stall time, not a minimum data rate.
        assert_eq!(value("--speed-limit"), "1");
        assert_eq!(value("--speed-time"), "60");
        assert_eq!(value("--max-time"), "900");
        let custom = args_of(
            Strategy::CurlBrowser,
            &Timeouts {
                connect_secs: 3,
                stall_secs: 7,
                overall_secs: 11,
            },
        );
        assert!(custom.windows(2).any(|w| w == ["--speed-time", "7"]));
        assert!(custom.windows(2).any(|w| w == ["--max-time", "11"]));
    }

    /// Transient failures wait longer each time and give up after two retries;
    /// a 403's browser retry does not wait; a permanent failure never waits.
    #[test]
    fn transient_failures_back_off_and_a_403_does_not() {
        let (_d, path) = dest();
        let u = "https://a.example/ds.pdf";
        let http = Scripted::new(vec![
            (u, Step::Answer(503, b"busy")),
            (u, Step::Curl(92)),
            (u, Step::Answer(200, PDF)),
        ]);
        assert!(fetch_pdf(&http, u, &path, "LM358").is_ok());
        assert_eq!(*http.paused.lock().unwrap(), BACKOFF.to_vec());
        let http = Scripted::new(vec![
            (u, Step::Answer(503, b"busy")),
            (u, Step::Answer(503, b"busy")),
            (u, Step::Answer(503, b"busy")),
        ]);
        let err = fetch_pdf(&http, u, &path, "LM358").unwrap_err();
        assert!(
            matches!(err, FetchError::Status { status: 503, .. }),
            "{err}"
        );
        assert_eq!(
            http.paused.lock().unwrap().len(),
            2,
            "two retries, then stop"
        );
        let http = Scripted::all(vec![
            (u, Step::Answer(403, b"no")),
            (u, Step::Answer(200, PDF)),
        ]);
        assert!(fetch_pdf(&http, u, &path, "LM358").is_ok());
        assert!(http.paused.lock().unwrap().is_empty());
        let http = Scripted::new(vec![(u, Step::Answer(404, b"gone"))]);
        assert!(fetch_pdf(&http, u, &path, "LM358").is_err());
        assert!(http.paused.lock().unwrap().is_empty());
    }

    /// A host that refuses is skipped from then on; a dead link says nothing
    /// about its host; two silences do.
    #[test]
    fn the_host_ledger_learns_from_refusals_and_silence_but_not_dead_links() {
        let ledger = HostLedger::default();
        let blocked = FetchError::Blocked {
            url: "https://Blocked.example:8443/a.pdf".into(),
            status: 403,
        };
        assert!(ledger.refusal("https://blocked.example/b.pdf").is_none());
        assert!(ledger.note(blocked.url(), &Err(&blocked)).is_some());
        assert!(
            ledger.note(blocked.url(), &Err(&blocked)).is_none(),
            "only the first refusal is news"
        );
        let why = ledger.refusal("https://blocked.example/other.pdf").unwrap();
        assert!(why.contains("HTTP 403"), "{why}");
        let dead = FetchError::Status {
            url: "https://dead.example/a.pdf".into(),
            status: 404,
        };
        for _ in 0..3 {
            ledger.note(dead.url(), &Err(&dead));
        }
        assert!(ledger.refusal(dead.url()).is_none());
        let silent = FetchError::Transport {
            url: "https://slow.example/a.pdf".into(),
            message: "curl failed: timed out".into(),
        };
        assert!(ledger.note(silent.url(), &Err(&silent)).is_none());
        assert!(
            ledger.refusal(silent.url()).is_none(),
            "one timeout is bad luck"
        );
        assert!(ledger.note(silent.url(), &Err(&silent)).is_some());
        assert!(ledger.refusal(silent.url()).unwrap().contains("2 times"));
        // A download that works resets the count.
        let flaky = "https://flaky.example/a.pdf";
        ledger.note(flaky, &Err(&FetchError::NotPdf { url: flaky.into() }));
        let slow = FetchError::Transport {
            url: flaky.into(),
            message: String::new(),
        };
        ledger.note(flaky, &Err(&slow));
        ledger.note(flaky, &Ok(()));
        ledger.note(flaky, &Err(&slow));
        assert!(ledger.refusal(flaky).is_none());
        let hosts: Vec<String> = ledger.skipped().into_iter().map(|(h, _)| h).collect();
        assert_eq!(hosts, ["blocked.example", "slow.example"]);
        assert_eq!(
            host_of("https://user:pw@WWW.Example.com:8080/x"),
            Some("www.example.com".into())
        );
        assert_eq!(host_of("not a url"), None);
    }

    /// The ledger learns from the strategy failure that got furthest: a 404 from
    /// any client means the host answers; only refusals everywhere block it.
    #[test]
    fn the_ledger_reads_through_several_strategies() {
        let ledger = HostLedger::default();
        let tried = |best: FetchError| FetchError::Tried {
            best: Box::new(best),
            tried: vec![
                (Strategy::CurlPlain, "x".into()),
                (Strategy::NodeFetch, "y".into()),
            ],
        };
        let dead = tried(FetchError::Status {
            url: "https://dead.example/a.pdf".into(),
            status: 404,
        });
        for _ in 0..3 {
            ledger.note(dead.url(), &Err(&dead));
        }
        assert!(ledger.refusal(dead.url()).is_none());
        let blocked = tried(FetchError::Blocked {
            url: "https://no.example/a.pdf".into(),
            status: 403,
        });
        assert!(ledger.note(blocked.url(), &Err(&blocked)).is_some());
        assert!(ledger.refusal("https://no.example/b.pdf").is_some());
    }

    // ── Over a real socket, through curl ─────────────────────────────────────

    use std::io::{Read as _, Write as _};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Arc;

    /// A throwaway HTTP/1.1 server on loopback: `handler(request head, nth
    /// request, stream)` answers each connection, which is then closed. The
    /// heads it was sent are kept in `heads`.
    struct Server {
        base: String,
        heads: Arc<StdMutex<Vec<String>>>,
    }

    fn serve(handler: impl Fn(&str, usize, &mut TcpStream) + Send + Sync + 'static) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let heads = Arc::new(StdMutex::new(Vec::new()));
        let (log, handler) = (Arc::clone(&heads), Arc::new(handler));
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (log, handler) = (Arc::clone(&log), Arc::clone(&handler));
                std::thread::spawn(move || {
                    let mut stream = stream;
                    let mut head = Vec::new();
                    let mut byte = [0u8; 1];
                    while !head.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap_or(0) == 1 {
                        head.push(byte[0]);
                    }
                    let head = String::from_utf8_lossy(&head).into_owned();
                    let nth = {
                        let mut log = log.lock().unwrap();
                        log.push(head.clone());
                        log.len()
                    };
                    handler(&head, nth, &mut stream);
                });
            }
        });
        Server { base, heads }
    }

    fn has_header(head: &str, name: &str) -> bool {
        let want = format!("{}:", name.to_ascii_lowercase());
        head.lines()
            .any(|l| l.to_ascii_lowercase().starts_with(&want))
    }

    fn reply(stream: &mut TcpStream, status: &str, body: &[u8]) {
        let head = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/pdf\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(body);
    }

    fn curl_available() -> bool {
        let ok = Command::new("curl").arg("--version").output().is_ok();
        if !ok {
            eprintln!("SKIP: curl is not installed");
        }
        ok
    }

    /// A site that 403s anything that is not a browser serves the PDF to the
    /// second request, which looks like one; a site that also wants a same-site
    /// `Referer` 403s that, and the fuller browser strategy gets in.
    #[test]
    fn a_403_site_serves_the_pdf_once_the_request_looks_like_a_browser() {
        if !curl_available() {
            return;
        }
        let looks_like_browser = |head: &str| {
            head.contains("User-Agent: Mozilla/5.0") && has_header(head, "Accept-Language")
        };
        let ua_only = serve(move |head, _, stream| {
            if looks_like_browser(head) {
                reply(stream, "200 OK", PDF);
            } else {
                reply(stream, "403 Forbidden", b"bots not welcome");
            }
        });
        let (_d, path) = dest();
        let url = format!("{}/ds.pdf", ua_only.base);
        let http = CurlHttp::default();
        let got = fetch_pdf(&http, &url, &path, "LM358").unwrap();
        assert_eq!(got.via, Via::Direct);
        assert_eq!(
            ua_only.heads.lock().unwrap().len(),
            2,
            "curl's own agent refused, the browser's served"
        );
        assert_eq!(
            http.memory().unwrap().preferred(&url),
            Some(Strategy::CurlStandard)
        );
        // The next part from that host goes straight to what worked.
        assert!(fetch_pdf(&http, &url, &path, "LM358").is_ok());
        assert_eq!(ua_only.heads.lock().unwrap().len(), 3);

        let strict = serve(move |head, _, stream| {
            if looks_like_browser(head) && has_header(head, "Referer") {
                reply(stream, "200 OK", PDF);
            } else {
                reply(stream, "403 Forbidden", b"no");
            }
        });
        let url = format!("{}/ds.pdf", strict.base);
        assert!(fetch_pdf(&CurlHttp::default(), &url, &path, "LM358").is_ok());
        let heads = strict.heads.lock().unwrap();
        assert_eq!(heads.len(), 3, "two refusals, then the fuller browser set");
        assert!(!has_header(&heads[1], "Referer") && has_header(&heads[2], "Referer"));
        drop(heads);

        // The reverse of the above: a site that serves curl's own agent and
        // refuses a browser's.
        let curl_only = serve(|head, _, stream| {
            if head.contains("User-Agent: curl/") {
                reply(stream, "200 OK", PDF);
            } else {
                reply(stream, "404 Not Found", b"no");
            }
        });
        let url = format!("{}/ds.pdf", curl_only.base);
        assert!(fetch_pdf(&CurlHttp::default(), &url, &path, "LM358").is_ok());
        assert_eq!(curl_only.heads.lock().unwrap().len(), 1);

        let never = serve(|_, _, stream| reply(stream, "403 Forbidden", b"no"));
        let url = format!("{}/ds.pdf", never.base);
        let err = fetch_pdf(&CurlHttp::default(), &url, &path, "LM358").unwrap_err();
        assert!(
            matches!(
                err.representative(),
                FetchError::Blocked { status: 403, .. }
            ),
            "{err}"
        );
        let asked = never.heads.lock().unwrap().len();
        assert!(
            (3..=4).contains(&asked),
            "three curl sets, plus node when it is installed, not forever: {asked}"
        );
        assert!(!err.to_string().contains('\n'), "{err}");
    }

    /// Node's `fetch` (when node is installed): it fetches a PDF across a
    /// redirect, reports an HTTP error status rather than failing, refuses a URL
    /// that is not http(s), and abandons a server that goes quiet with curl's
    /// stall code.
    #[test]
    fn node_fetch_follows_redirects_and_stops_at_a_stall() {
        if !node_available() {
            eprintln!("SKIP: node is not installed");
            return;
        }
        let timeouts = Timeouts {
            connect_secs: 5,
            stall_secs: 2,
            overall_secs: 40,
        };
        let http = CurlHttp::with_timeouts(timeouts);
        let (_d, path) = dest();
        let get = |url: &str| {
            http.get(&Request {
                url,
                dest: &path,
                strategy: Strategy::NodeFetch,
                http1: false,
            })
        };
        let server = serve(|head, _, stream| {
            if head.starts_with("GET /go ") {
                let _ = stream.write_all(
                    b"HTTP/1.1 302 Found\r\nLocation: /ds.pdf\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
            } else if head.starts_with("GET /ds.pdf ") {
                reply(stream, "200 OK", PDF);
            } else {
                reply(stream, "404 Not Found", b"gone");
            }
        });
        let got = get(&format!("{}/go", server.base)).unwrap();
        assert_eq!(got.status, 200);
        assert_eq!(got.final_url, format!("{}/ds.pdf", server.base));
        assert_eq!(std::fs::read(&path).unwrap(), PDF);
        let got = get(&format!("{}/missing", server.base)).unwrap();
        assert_eq!(got.status, 404, "an HTTP error is an answer, not a failure");
        assert!(get("data:application/pdf;base64,JVBERg==").is_err());
        assert!(get("file:///etc/hostname").is_err());

        let stalled = serve(|_, _, stream| {
            let head = "HTTP/1.1 200 OK\r\nContent-Type: application/pdf\r\nContent-Length: 100000\r\nConnection: close\r\n\r\n";
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(PDF);
            let _ = stream.flush();
            std::thread::sleep(Duration::from_secs(20));
        });
        let started = std::time::Instant::now();
        let err = get(&format!("{}/ds.pdf", stalled.base)).unwrap_err();
        assert_eq!(err.code, Some(28), "{err:?}");
        assert!(err.stalled());
        assert!(
            started.elapsed() < Duration::from_secs(12),
            "abandoned after about the stall time: {:?}",
            started.elapsed()
        );
    }

    /// A download that keeps sending finishes even though it takes far longer
    /// than the stall time; one that sends its headers and then goes quiet is
    /// abandoned after the stall time with curl's timeout error, which is
    /// transient (retried by the caller).
    #[test]
    fn a_slow_download_that_progresses_finishes_and_a_stalled_one_is_abandoned() {
        if !curl_available() {
            return;
        }
        let timeouts = Timeouts {
            connect_secs: 5,
            stall_secs: 2,
            overall_secs: 40,
        };
        let http = CurlHttp::with_timeouts(timeouts);
        let trickle = serve(|_, _, stream| {
            let chunks = 14usize;
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/pdf\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                PDF.len() + chunks
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(PDF);
            for _ in 0..chunks {
                std::thread::sleep(Duration::from_millis(400));
                let _ = stream.write_all(b"x");
                let _ = stream.flush();
            }
        });
        let (_d, path) = dest();
        let started = std::time::Instant::now();
        let got = fetch_pdf(&http, &format!("{}/ds.pdf", trickle.base), &path, "LM358");
        let took = started.elapsed();
        assert!(got.is_ok(), "{got:?}");
        assert!(
            took > Duration::from_secs(2 * u64::from(timeouts.stall_secs)),
            "it took {took:?}: longer than the stall time, so a fixed total would have cut it"
        );
        assert!(std::fs::metadata(&path).unwrap().len() > PDF.len() as u64);

        let stalled = serve(|_, _, stream| {
            let head = "HTTP/1.1 200 OK\r\nContent-Type: application/pdf\r\nContent-Length: 100000\r\nConnection: close\r\n\r\n";
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(PDF);
            let _ = stream.flush();
            std::thread::sleep(Duration::from_secs(20));
        });
        let started = std::time::Instant::now();
        let err = http
            .get(&Request {
                url: &format!("{}/ds.pdf", stalled.base),
                dest: &path,
                strategy: Strategy::CurlStandard,
                http1: false,
            })
            .unwrap_err();
        assert_eq!(err.code, Some(28), "{err:?}");
        assert!(err.transient());
        assert!(
            started.elapsed() < Duration::from_secs(12),
            "abandoned after about the stall time, not the overall cap: {:?}",
            started.elapsed()
        );
    }
}
