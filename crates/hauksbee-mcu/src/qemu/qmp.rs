//! Minimal QMP (QEMU Machine Protocol) client over TCP.
//!
//! QMP is a line-delimited JSON protocol. On connect QEMU emits a greeting
//! (`{"QMP": {...}}`); the client must then send `qmp_capabilities` to leave
//! negotiation mode before any other command is accepted. Each command is one
//! JSON object on its own line; each reply is one JSON object (`{"return": ...}`
//! or `{"error": ...}`), possibly preceded by asynchronous `{"event": ...}`
//! messages which we skip.
//!
//! We deliberately avoid a JSON dependency: the messages we send are tiny and
//! fixed-shape, and the only field we need to parse out of replies is a
//! `"return"` value (a hex string from the human monitor, or an empty object).
//! A hand-rolled brace-balanced reader frames one JSON object at a time and a
//! tiny extractor pulls the fields we care about. This keeps the crate's
//! dependency surface identical to the Renode backend (pure std).

use anyhow::{bail, Context, Result};
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

/// A connected QMP session, already out of capabilities-negotiation mode.
pub struct Qmp {
    stream: TcpStream,
    /// Bytes read past the end of the last framed JSON object.
    carry: Vec<u8>,
    timeout: Duration,
}

impl Qmp {
    /// Connect to a QMP server on `addr`, retrying until `connect_timeout`
    /// elapses (QEMU takes a moment to bind), then perform the capabilities
    /// handshake so the session is ready for commands.
    pub fn connect<A: ToSocketAddrs + Clone>(addr: A, connect_timeout: Duration) -> Result<Self> {
        let deadline = Instant::now() + connect_timeout;
        let stream = loop {
            let mut resolved = addr
                .clone()
                .to_socket_addrs()
                .context("resolving QMP address")?;
            let sock = resolved.next().context("no socket address for QMP")?;
            match TcpStream::connect_timeout(&sock, Duration::from_millis(500)) {
                Ok(s) => break s,
                Err(e) => {
                    if Instant::now() >= deadline {
                        return Err(e).context("connecting to QEMU QMP socket");
                    }
                    std::thread::sleep(Duration::from_millis(150));
                }
            }
        };
        stream.set_read_timeout(Some(Duration::from_millis(200))).ok();
        stream.set_nodelay(true).ok();
        let mut q = Qmp {
            stream,
            carry: Vec::new(),
            timeout: Duration::from_secs(30),
        };
        // Read the greeting, then negotiate out of capabilities mode.
        let _greeting = q.read_message(Duration::from_secs(10))?;
        q.execute("qmp_capabilities", Duration::from_secs(10))?;
        Ok(q)
    }

    /// Set the default per-command response timeout.
    pub fn set_timeout(&mut self, t: Duration) {
        self.timeout = t;
    }

    /// Send a bare QMP command (no arguments) and return its `return` payload as
    /// a raw JSON fragment string.
    pub fn execute(&mut self, command: &str, timeout: Duration) -> Result<String> {
        let req = format!("{{\"execute\":\"{command}\"}}\n");
        self.send_and_collect(&req, timeout)
    }

    /// Run a Human-Monitor command (HMP) through QMP's `human-monitor-command`.
    /// Returns the monitor's textual output (the `return` string).
    pub fn hmp(&mut self, line: &str) -> Result<String> {
        let timeout = self.timeout;
        // The HMP line is embedded as a JSON string argument; escape the few
        // characters that matter for our fixed command shapes.
        let escaped = line.replace('\\', "\\\\").replace('"', "\\\"");
        let req = format!(
            "{{\"execute\":\"human-monitor-command\",\"arguments\":{{\"command-line\":\"{escaped}\"}}}}\n"
        );
        let ret = self.send_and_collect(&req, timeout)?;
        Ok(unescape_json_string(&ret))
    }

    /// Pause the vCPU (QMP `stop`).
    pub fn stop(&mut self) -> Result<()> {
        let t = self.timeout;
        self.execute("stop", t).map(|_| ())
    }

    /// Resume the vCPU (QMP `cont`).
    pub fn cont(&mut self) -> Result<()> {
        let t = self.timeout;
        self.execute("cont", t).map(|_| ())
    }

    /// Read one 32-bit word from guest *physical* memory via the human monitor's
    /// `xp` (examine physical) command. The ESP32 GPIO matrix registers are
    /// memory-mapped at fixed physical addresses, so this is the GPIO-out read
    /// channel. Returns the little-endian word value.
    ///
    /// `xp /1wx 0x3ff44004` prints e.g. `000000003ff44004: 0x00000020`.
    pub fn read_u32(&mut self, addr: u32) -> Result<u32> {
        let out = self.hmp(&format!("xp /1wx 0x{addr:08x}"))?;
        parse_xp_word(&out)
            .with_context(|| format!("parsing xp output for 0x{addr:08x}: {out:?}"))
    }

    // NOTE on memory WRITES: the QEMU human monitor has no portable
    // physical-word poke, so [`Qmp`] exposes reads only. The backend writes GPIO
    // inputs through the gdbstub `M` packet (see `gdb.rs`). Keeping that split
    // explicit avoids a half-working HMP write path.

    /// Send a request and collect the matching `return`/`error` reply, skipping
    /// any asynchronous events that arrive first.
    fn send_and_collect(&mut self, req: &str, timeout: Duration) -> Result<String> {
        self.stream
            .write_all(req.as_bytes())
            .context("writing QMP request")?;
        self.stream.flush().ok();
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!("QMP command timed out: {}", req.trim());
            }
            let msg = self.read_message(remaining)?;
            if let Some(ret) = extract_field(&msg, "return") {
                return Ok(ret);
            }
            if let Some(err) = extract_field(&msg, "error") {
                bail!("QMP error for {}: {err}", req.trim());
            }
            // Otherwise it was an async event; keep reading for the reply.
        }
    }

    /// Read one complete brace-balanced JSON object (one QMP message) as a
    /// string, blocking up to `timeout`.
    fn read_message(&mut self, timeout: Duration) -> Result<String> {
        let deadline = Instant::now() + timeout;
        let mut buf = std::mem::take(&mut self.carry);
        loop {
            if let Some(end) = first_json_object_end(&buf) {
                let msg = buf[..=end].to_vec();
                self.carry = buf[end + 1..].to_vec();
                return Ok(String::from_utf8_lossy(&msg).into_owned());
            }
            if Instant::now() >= deadline {
                bail!(
                    "QMP read timed out with no complete message; partial: {:?}",
                    String::from_utf8_lossy(&buf[..buf.len().min(200)])
                );
            }
            let mut chunk = [0u8; 4096];
            match self.stream.read(&mut chunk) {
                Ok(0) => bail!("QMP connection closed"),
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(ref e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    std::thread::sleep(Duration::from_millis(3));
                }
                Err(e) => return Err(e).context("reading QMP socket"),
            }
        }
    }
}

/// Find the byte index of the closing brace of the first complete top-level
/// JSON object in `buf`, honouring strings and escapes so a `}` inside a string
/// does not end the object early.
fn first_json_object_end(buf: &[u8]) -> Option<usize> {
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    let mut started = false;
    for (i, &b) in buf.iter().enumerate() {
        if in_str {
            if esc {
                esc = false;
            } else if b == b'\\' {
                esc = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => {
                depth += 1;
                started = true;
            }
            b'}' => {
                depth -= 1;
                if started && depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Extract the raw JSON fragment for a top-level `"key": <value>` from a QMP
/// message, where `<value>` is an object, string, number, or `true/false`. This
/// is a deliberately small extractor (no full JSON parse) sufficient for the
/// `return` / `error` fields QMP replies carry.
fn extract_field(msg: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let start = msg.find(&needle)? + needle.len();
    let rest = &msg[start..];
    // Skip whitespace and the colon.
    let colon = rest.find(':')?;
    let mut value = rest[colon + 1..].trim_start();
    // Strip a leading byte-order/quote handling: capture by type.
    if let Some(stripped) = value.strip_prefix('{') {
        // Object: balance braces.
        let mut depth = 1i32;
        let mut in_str = false;
        let mut esc = false;
        for (i, c) in stripped.char_indices() {
            if in_str {
                if esc {
                    esc = false;
                } else if c == '\\' {
                    esc = true;
                } else if c == '"' {
                    in_str = false;
                }
                continue;
            }
            match c {
                '"' => in_str = true,
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(format!("{{{}}}", &stripped[..i]));
                    }
                }
                _ => {}
            }
        }
        None
    } else if let Some(stripped) = value.strip_prefix('"') {
        // String: read to the closing unescaped quote, keep the raw (escaped)
        // contents so the caller can unescape if needed.
        let mut esc = false;
        for (i, c) in stripped.char_indices() {
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                return Some(stripped[..i].to_string());
            }
        }
        None
    } else {
        // Scalar (number / bool / null): read to the next delimiter.
        let endp = value
            .find([',', '}', '\n'])
            .unwrap_or(value.len());
        value = value[..endp].trim();
        Some(value.to_string())
    }
}

/// Turn a JSON string body with `\n`, `\r`, `\"`, `\\` escapes into raw text.
fn unescape_json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some('/') => out.push('/'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Parse the word value out of an `xp /1wx <addr>` human-monitor line, which
/// looks like `000000003ff44004: 0x00000020` (address, colon, hex word).
fn parse_xp_word(out: &str) -> Option<u32> {
    // Find the last `0x` token and parse the hex after it.
    let after = out.rsplit_once("0x")?.1;
    let hex: String = after
        .chars()
        .take_while(|c| c.is_ascii_hexdigit())
        .collect();
    if hex.is_empty() {
        return None;
    }
    u32::from_str_radix(&hex, 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_one_object() {
        let buf = b"{\"QMP\":{\"version\":1}}\n{\"return\":{}}";
        let end = first_json_object_end(buf).unwrap();
        assert_eq!(&buf[..=end], b"{\"QMP\":{\"version\":1}}");
    }

    #[test]
    fn brace_in_string_does_not_close() {
        let buf = b"{\"a\":\"}}}\"}";
        let end = first_json_object_end(buf).unwrap();
        assert_eq!(end, buf.len() - 1);
    }

    #[test]
    fn extracts_object_return() {
        let msg = "{\"return\": {\"running\": true}}";
        assert_eq!(extract_field(msg, "return").as_deref(), Some("{\"running\": true}"));
    }

    #[test]
    fn extracts_string_return() {
        let msg = "{\"return\": \"000000003ff44004: 0x00000020\\r\\n\"}";
        let r = extract_field(msg, "return").unwrap();
        assert!(r.contains("0x00000020"));
        let txt = unescape_json_string(&r);
        assert!(txt.contains("0x00000020"));
    }

    #[test]
    fn detects_error() {
        let msg = "{\"error\": {\"class\": \"GenericError\", \"desc\": \"bad\"}}";
        assert!(extract_field(msg, "return").is_none());
        assert!(extract_field(msg, "error").is_some());
    }

    #[test]
    fn parses_xp_word() {
        assert_eq!(parse_xp_word("000000003ff44004: 0x00000020"), Some(0x20));
        assert_eq!(parse_xp_word("0x3ff44004: 0xdeadbeef\r\n"), Some(0xdeadbeef));
        assert_eq!(parse_xp_word("no hex here"), None);
    }
}
