//! Downloading a datasheet PDF from a URL, and surviving what real vendor
//! sites do to a plain `curl`.
//!
//! Measured on real boards, a quarter to a half of the datasheet links a board
//! names do not download on the first try: curl exit 92 ("HTTP/2 stream not
//! closed cleanly") on many hosts, an HTML landing page where a PDF was
//! promised, HTTP 403. Each has a bounded remedy here, in a fixed order, and
//! nothing loops:
//!
//! 1. A transient failure (a curl transport error such as 92, or HTTP 429, 502,
//!    503, 504) is retried twice with a growing pause, the first retry over
//!    HTTP/1.1.
//! 2. An HTTP 403 is retried once with a fuller browser-style request. The first
//!    request already carries a browser's `User-Agent`, `Accept` and
//!    `Accept-Language`; many manufacturer sites refuse curl's own.
//! 3. A page that is HTML is searched for its PDF links; when exactly one is a
//!    plausible datasheet (or exactly one names the part), that is followed, one
//!    hop.
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
//! Anything left over is a [`FetchError`], and the caller decides what is next
//! (discovery, or telling the person to download it). Nothing here answers a
//! CAPTCHA or works around bot protection: a site that asks for a person gets
//! one, via `--datasheet`.
//!
//! The transport is the [`Http`] trait so every rule above is tested without a
//! network; [`CurlHttp`] is the one real implementation (`curl`, no shell).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

/// Which request headers a fetch sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// A browser's `User-Agent` and a PDF-first `Accept` (vendor sites refuse
    /// curl's own).
    Standard,
    /// The retry after an HTTP 403: a different browser build's full header set,
    /// including a language and a same-site `Referer`.
    Browser,
}

/// One download attempt.
#[derive(Debug, Clone, Copy)]
pub struct Request<'a> {
    pub url: &'a str,
    /// Where the body is written.
    pub dest: &'a Path,
    pub profile: Profile,
    /// Force HTTP/1.1 (`--http1.1`): the cure for curl's HTTP/2 stream errors.
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
    /// curl's exit code, when there was one.
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

/// `curl`, run without a shell.
#[derive(Debug, Clone, Copy, Default)]
pub struct CurlHttp {
    pub timeouts: Timeouts,
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
    match req.profile {
        Profile::Standard => {
            args.extend(["-A".into(), STANDARD_UA.into(), "--compressed".into()]);
            header(
                &mut args,
                "Accept: application/pdf,application/octet-stream;q=0.9,text/html;q=0.8,*/*;q=0.7"
                    .into(),
            );
            header(&mut args, "Accept-Language: en-US,en;q=0.9".into());
        }
        Profile::Browser => {
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

impl Http for CurlHttp {
    fn get(&self, req: &Request<'_>) -> Result<Response, TransportError> {
        let out = Command::new("curl")
            .args(curl_args(req, &self.timeouts))
            .output()
            .map_err(|e| TransportError {
                code: None,
                message: format!("cannot run `curl`: {e}"),
            })?;
        if !out.status.success() {
            return Err(TransportError {
                code: out.status.code(),
                message: format!(
                    "curl failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            });
        }
        let said = String::from_utf8_lossy(&out.stdout);
        let (status, final_url) = said
            .trim()
            .split_once(' ')
            .unwrap_or_else(|| (said.trim(), ""));
        Ok(Response {
            status: status.parse().unwrap_or(0),
            final_url: final_url.to_string(),
        })
    }

    fn pause(&self, how_long: Duration) {
        std::thread::sleep(how_long);
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
        }
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

/// One URL with the transport rules: a transient failure or status retried
/// twice (after a pause that grows), the first retry over HTTP/1.1, and an
/// HTTP 403 retried once with the browser profile.
fn get_with_retries(http: &dyn Http, url: &str, dest: &Path) -> Result<Response, TransportError> {
    let (mut profile, mut http1) = (Profile::Standard, false);
    let (mut transient_retries, mut retried_blocked) = (0, false);
    loop {
        let attempt = http.get(&Request {
            url,
            dest,
            profile,
            http1,
        });
        let again = match &attempt {
            Ok(r) if r.status == 403 && !retried_blocked => {
                (profile, retried_blocked) = (Profile::Browser, true);
                true
            }
            Ok(r) if transient_status(r.status) => true,
            Err(e) if e.transient() => true,
            _ => false,
        };
        let transient = matches!(&attempt, Ok(r) if transient_status(r.status))
            || matches!(&attempt, Err(e) if e.transient());
        if !again || (transient && transient_retries >= BACKOFF.len()) {
            return attempt;
        }
        if transient {
            http1 = true;
            http.pause(BACKOFF[transient_retries]);
            transient_retries += 1;
        }
    }
}

enum Landed {
    Pdf(String),
    Page { url: String, html: String },
}

/// Fetch `url` once (with retries) and say what landed in `dest`.
fn land(http: &dyn Http, url: &str, dest: &Path) -> Result<Landed, FetchError> {
    let _ = std::fs::remove_file(dest);
    let response = get_with_retries(http, url, dest).map_err(|e| FetchError::Transport {
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

/// Download `url` to `dest` as a PDF for `part`, applying the remedies in the
/// module doc. `dest` holds a PDF on `Ok` and nothing otherwise.
///
/// # Errors
///
/// A [`FetchError`] naming the URL and why no PDF came of it.
pub fn fetch_pdf(
    http: &dyn Http,
    url: &str,
    dest: &Path,
    part: &str,
) -> Result<Fetched, FetchError> {
    match land(http, url, dest)? {
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
            match land(http, &link.url, dest) {
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
        match result {
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
        seen: StdMutex<Vec<(String, Profile, bool)>>,
        paused: StdMutex<Vec<Duration>>,
    }

    impl Scripted {
        fn new(steps: Vec<(&'static str, Step)>) -> Self {
            Self {
                steps: StdMutex::new(steps.into()),
                seen: StdMutex::default(),
                paused: StdMutex::default(),
            }
        }
    }

    impl Http for Scripted {
        fn pause(&self, how_long: Duration) {
            self.paused.lock().unwrap().push(how_long);
        }

        fn get(&self, req: &Request<'_>) -> Result<Response, TransportError> {
            self.seen
                .lock()
                .unwrap()
                .push((req.url.to_string(), req.profile, req.http1));
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
                profile: Profile::Standard,
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

    /// An HTTP 403 is retried once with the browser profile; if that works the
    /// PDF is used, and if it does not the error is the block, naming the URL.
    #[test]
    fn a_403_is_retried_once_with_a_browser_profile() {
        let (_d, path) = dest();
        let http = Scripted::new(vec![
            ("https://b.example/ds.pdf", Step::Answer(403, b"no")),
            ("https://b.example/ds.pdf", Step::Answer(200, PDF)),
        ]);
        assert!(fetch_pdf(&http, "https://b.example/ds.pdf", &path, "LM358").is_ok());
        assert_eq!(
            http.seen
                .lock()
                .unwrap()
                .iter()
                .map(|s| s.1)
                .collect::<Vec<_>>(),
            [Profile::Standard, Profile::Browser]
        );
        let http = Scripted::new(vec![
            ("https://b.example/ds.pdf", Step::Answer(403, b"no")),
            ("https://b.example/ds.pdf", Step::Answer(403, b"no")),
        ]);
        let err = fetch_pdf(&http, "https://b.example/ds.pdf", &path, "LM358").unwrap_err();
        assert_eq!(
            err,
            FetchError::Blocked {
                url: "https://b.example/ds.pdf".into(),
                status: 403
            }
        );
        assert!(!path.exists(), "a failed fetch leaves no file");
        let args = curl_args(
            &Request {
                url: "https://b.example/x/ds.pdf",
                dest: &path,
                profile: Profile::Browser,
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
                profile: Profile::Standard,
                http1: false,
            },
            &Timeouts::default(),
        );
        let at = args.iter().position(|a| a == "--proto-redir").unwrap();
        assert_eq!(args[at + 1], "=http,https");
    }

    fn args_of(profile: Profile, timeouts: &Timeouts) -> Vec<String> {
        let (_d, path) = dest();
        curl_args(
            &Request {
                url: "https://a.example/x.pdf",
                dest: &path,
                profile,
                http1: false,
            },
            timeouts,
        )
    }

    /// The very first request is a browser's: its user agent, an Accept that
    /// asks for the PDF, and a language. Vendor sites 403 curl's own.
    #[test]
    fn the_first_request_already_carries_browser_headers() {
        let args = args_of(Profile::Standard, &Timeouts::default());
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
        let args = args_of(Profile::Standard, &Timeouts::default());
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
            Profile::Browser,
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
        let http = Scripted::new(vec![
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
    /// first request, which now looks like one; a site that also wants a
    /// same-site `Referer` 403s it, and the single browser-profile retry gets in.
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
        let got = fetch_pdf(&CurlHttp::default(), &url, &path, "LM358").unwrap();
        assert_eq!(got.via, Via::Direct);
        assert_eq!(
            ua_only.heads.lock().unwrap().len(),
            1,
            "no 403 round trip needed"
        );

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
        assert_eq!(heads.len(), 2, "one refusal, one browser-profile retry");
        assert!(!has_header(&heads[0], "Referer") && has_header(&heads[1], "Referer"));
        drop(heads);

        let never = serve(|_, _, stream| reply(stream, "403 Forbidden", b"no"));
        let url = format!("{}/ds.pdf", never.base);
        let err = fetch_pdf(&CurlHttp::default(), &url, &path, "LM358").unwrap_err();
        assert!(
            matches!(err, FetchError::Blocked { status: 403, .. }),
            "{err}"
        );
        assert_eq!(
            never.heads.lock().unwrap().len(),
            2,
            "tried twice, not forever"
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
        let http = CurlHttp { timeouts };
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
                profile: Profile::Standard,
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
