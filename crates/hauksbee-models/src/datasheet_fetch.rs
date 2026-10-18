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
//!    503, 504) is retried once, over HTTP/1.1.
//! 2. An HTTP 403 is retried once with a fuller browser-style request.
//! 3. A page that is HTML is searched for its PDF links; when exactly one is a
//!    plausible datasheet (or exactly one names the part), that is followed, one
//!    hop.
//!
//! Anything left over is a [`FetchError`], and the caller decides what is next
//! (discovery, or telling the person to download it). Nothing here answers a
//! CAPTCHA or works around bot protection: a site that asks for a person gets
//! one, via `--datasheet`.
//!
//! The transport is the [`Http`] trait so every rule above is tested without a
//! network; [`CurlHttp`] is the one real implementation (`curl`, no shell).

use std::path::{Path, PathBuf};
use std::process::Command;

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

/// The network, as far as a datasheet fetch needs it.
pub trait Http {
    /// Fetch `req.url` into `req.dest` and report the final status and URL.
    ///
    /// # Errors
    ///
    /// A [`TransportError`] when no HTTP answer came back.
    fn get(&self, req: &Request<'_>) -> Result<Response, TransportError>;
}

/// `curl`, run without a shell.
#[derive(Debug, Clone, Copy, Default)]
pub struct CurlHttp;

const STANDARD_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_0) AppleWebKit/537.36 \
                           (KHTML, like Gecko) Chrome/126.0 Safari/537.36";
const BROWSER_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                          (KHTML, like Gecko) Chrome/125.0.0.0 Safari/537.36 Edg/125.0.0.0";

/// The `curl` arguments for one attempt: redirects followed (most datasheet
/// links redirect to a CDN) but only to http(s), a bounded time, and the final
/// status and URL written to stdout.
#[must_use]
pub fn curl_args(req: &Request<'_>) -> Vec<String> {
    let mut args: Vec<String> = [
        "-sSL",
        "--proto",
        "=http,https",
        "--proto-redir",
        "=http,https",
        "--max-redirs",
        "10",
        "--connect-timeout",
        "20",
        "--max-time",
        "300",
        // A server that goes quiet (bot protection holding the connection open)
        // is abandoned after 30 s below 1 kB/s; a slow but moving download is not.
        "--speed-limit",
        "1000",
        "--speed-time",
        "30",
    ]
    .map(str::to_owned)
    .to_vec();
    if req.http1 {
        args.push("--http1.1".into());
    }
    let header = |args: &mut Vec<String>, h: String| {
        args.push("-H".into());
        args.push(h);
    };
    match req.profile {
        Profile::Standard => {
            args.extend(["-A".into(), STANDARD_UA.into()]);
            header(
                &mut args,
                "Accept: application/pdf,application/octet-stream;q=0.9,*/*;q=0.8".into(),
            );
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
            .args(curl_args(req))
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

/// One URL with the transport rules: a transient failure or status retried once
/// over HTTP/1.1, an HTTP 403 retried once with the browser profile.
fn get_with_retries(http: &dyn Http, url: &str, dest: &Path) -> Result<Response, TransportError> {
    let (mut profile, mut http1) = (Profile::Standard, false);
    let (mut retried_transport, mut retried_blocked) = (false, false);
    loop {
        let attempt = http.get(&Request {
            url,
            dest,
            profile,
            http1,
        });
        match &attempt {
            Ok(r) if r.status == 403 && !retried_blocked => {
                (profile, retried_blocked) = (Profile::Browser, true);
            }
            Ok(r) if transient_status(r.status) && !retried_transport => {
                (http1, retried_transport) = (true, true);
            }
            Err(e) if e.transient() && !retried_transport => {
                (http1, retried_transport) = (true, true);
            }
            _ => return attempt,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    /// What a scripted server does on one request.
    enum Step {
        /// Answer with this status, writing `body` to the destination.
        Answer(u16, &'static [u8]),
        /// Fail like curl with this exit code.
        Curl(i32),
    }

    /// A transport that plays `steps` in order and records each request.
    struct Scripted {
        steps: RefCell<VecDeque<(&'static str, Step)>>,
        seen: RefCell<Vec<(String, Profile, bool)>>,
    }

    impl Scripted {
        fn new(steps: Vec<(&'static str, Step)>) -> Self {
            Self {
                steps: RefCell::new(steps.into()),
                seen: RefCell::default(),
            }
        }
    }

    impl Http for Scripted {
        fn get(&self, req: &Request<'_>) -> Result<Response, TransportError> {
            self.seen
                .borrow_mut()
                .push((req.url.to_string(), req.profile, req.http1));
            let (expected, step) = self
                .steps
                .borrow_mut()
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
        let seen = http.seen.borrow();
        assert_eq!(seen.len(), 2);
        assert!(
            !seen[0].2 && seen[1].2,
            "second attempt is HTTP/1.1: {seen:?}"
        );
        assert!(curl_args(&Request {
            url: "u",
            dest: &path,
            profile: Profile::Standard,
            http1: true,
        })
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
        ]);
        let err = fetch_pdf(&http, "https://a.example/ds.pdf", &path, "LM358").unwrap_err();
        assert!(matches!(err, FetchError::Transport { .. }), "{err}");
        // A permanent curl error (6: cannot resolve the host) is not retried.
        let http = Scripted::new(vec![("https://a.example/ds.pdf", Step::Curl(6))]);
        assert!(fetch_pdf(&http, "https://a.example/ds.pdf", &path, "LM358").is_err());
        assert_eq!(http.seen.borrow().len(), 1);
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
            http.seen.borrow().iter().map(|s| s.1).collect::<Vec<_>>(),
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
        let args = curl_args(&Request {
            url: "https://b.example/x/ds.pdf",
            dest: &path,
            profile: Profile::Browser,
            http1: false,
        });
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
        let args = curl_args(&Request {
            url: "https://a.example/x.pdf",
            dest: &path,
            profile: Profile::Standard,
            http1: false,
        });
        let at = args.iter().position(|a| a == "--proto-redir").unwrap();
        assert_eq!(args[at + 1], "=http,https");
    }

    /// A server that holds the connection open without sending is abandoned
    /// quickly (connect and stall limits), not after minutes.
    #[test]
    fn a_stalled_server_is_abandoned_quickly() {
        let (_d, path) = dest();
        let args = curl_args(&Request {
            url: "https://a.example/x.pdf",
            dest: &path,
            profile: Profile::Standard,
            http1: false,
        });
        let value = |flag: &str| {
            let at = args.iter().position(|a| a == flag).unwrap();
            args[at + 1].clone()
        };
        assert_eq!(value("--connect-timeout"), "20");
        assert_eq!(value("--speed-limit"), "1000");
        assert_eq!(value("--speed-time"), "30");
        assert_eq!(value("--max-time"), "300");
    }
}
