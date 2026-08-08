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
//!
//! Long-form how-and-why: docs/how-and-why/hauksbee-mcu/qemu.md.

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
    /// Host timestamp (epoch seconds) of the newest `RESUME` event seen since
    /// [`Qmp::clear_run_events`]. QEMU stamps every asynchronous event with
    /// the host time at which the state transition actually happened, which is
    /// what makes the cont→stop window measurable instead of assumed.
    event_resume: Option<f64>,
    /// Host timestamp (epoch seconds) of the newest `STOP` event seen since
    /// [`Qmp::clear_run_events`].
    event_stop: Option<f64>,
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
        stream
            .set_read_timeout(Some(Duration::from_millis(200)))
            .ok();
        stream.set_nodelay(true).ok();
        let mut q = Qmp {
            stream,
            carry: Vec::new(),
            timeout: Duration::from_secs(30),
            event_resume: None,
            event_stop: None,
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
        parse_xp_word(&out).with_context(|| format!("parsing xp output for 0x{addr:08x}: {out:?}"))
    }

    // NOTE on memory WRITES: the QEMU human monitor has no portable
    // physical-word poke, so [`Qmp`] exposes reads only. The backend writes GPIO
    // inputs through the gdbstub `M` packet (see `gdb.rs`). Keeping that split
    // explicit avoids a half-working HMP write path.

    /// Set a QOM object property via `qom-set`. `value_json` is the raw JSON
    /// value to assign (an integer literal like `"35000"`, or a quoted string).
    /// Used to push a sensor reading into an emulated I2C device (e.g. the ESP32
    /// machine's built-in `tmp105` temperature) so the firmware reads it over its
    /// own I2C controller.
    pub fn qom_set(&mut self, path: &str, property: &str, value_json: &str) -> Result<()> {
        let req = format!(
            "{{\"execute\":\"qom-set\",\"arguments\":{{\"path\":\"{path}\",\"property\":\"{property}\",\"value\":{value_json}}}}}\n"
        );
        self.send_and_collect(&req, Duration::from_secs(5))
            .map(|_| ())
    }

    /// Get a QOM object property via `qom-get`, returning the raw `return` field
    /// as a string (a JSON scalar, e.g. `"72"` for an integer `address`).
    pub fn qom_get(&mut self, path: &str, property: &str) -> Result<String> {
        let req = format!(
            "{{\"execute\":\"qom-get\",\"arguments\":{{\"path\":\"{path}\",\"property\":\"{property}\"}}}}\n"
        );
        self.send_and_collect(&req, Duration::from_secs(5))
    }

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
            self.note_event(&msg);
            if let Some(ret) = extract_field(&msg, "return") {
                return Ok(ret);
            }
            if let Some(err) = extract_field(&msg, "error") {
                bail!("QMP error for {}: {err}", req.trim());
            }
            // Otherwise it was an async event; keep reading for the reply.
        }
    }

    /// Record the host timestamp of a `RESUME`/`STOP` event message; a no-op
    /// for command replies and other events. Called on every framed message so
    /// an event is captured whether it arrives before its command's `return`
    /// or is drained afterwards by [`Qmp::measured_run_window`].
    fn note_event(&mut self, msg: &str) {
        let Some(event) = extract_field(msg, "event") else {
            return;
        };
        let slot = match event.as_str() {
            "RESUME" => &mut self.event_resume,
            "STOP" => &mut self.event_stop,
            _ => return,
        };
        let Some(ts) = extract_field(msg, "timestamp") else {
            return;
        };
        let (Some(secs), Some(micros)) = (
            extract_field(&ts, "seconds").and_then(|v| v.parse::<f64>().ok()),
            extract_field(&ts, "microseconds").and_then(|v| v.parse::<f64>().ok()),
        ) else {
            return;
        };
        *slot = Some(secs + micros / 1e6);
    }

    /// Forget any recorded `RESUME`/`STOP` event timestamps. Call immediately
    /// before the `cont` that opens a run window, so a stale pair from an
    /// earlier chunk can never masquerade as this chunk's measurement.
    ///
    /// A previous chunk's STOP can still be sitting unread in the TCP buffer
    /// when this runs, and it will be noted into the cleared slot as soon as
    /// something reads the socket. That stale value is NOT drained here (an
    /// empty-socket probe costs a blocking read timeout on every chunk);
    /// instead [`Qmp::measured_run_window`] refuses to settle until the pair
    /// is ordered, which a stale STOP next to this chunk's RESUME never is.
    pub fn clear_run_events(&mut self) {
        self.event_resume = None;
        self.event_stop = None;
    }

    /// The wall-clock window the guest ACTUALLY ran between the last
    /// `cont` and `stop`, measured from QEMU's own RESUME/STOP event
    /// timestamps, or `None` when it cannot be measured (an event never
    /// arrived, or the pair is inconsistent).
    ///
    /// The STOP event races the `stop` command's `return`, so this drains the
    /// socket for up to `grace` waiting for a missing event before giving up.
    /// `None` must be treated as "unmeasured", never as "zero".
    ///
    /// The drain keeps going until the pair is ORDERED (stop after resume),
    /// not merely present: a stale STOP that slipped past
    /// [`Qmp::clear_run_events`]'s buffer drain fills the slot with a
    /// timestamp older than this chunk's RESUME, and stopping there would
    /// return `None` while the real STOP sits buffered, silently restoring
    /// the uncredited-slack bias for this chunk AND arming the next one with
    /// another stale event. Timestamps are monotone and `note_event` keeps
    /// the newest, so waiting for order converges on the true pair.
    pub fn measured_run_window(&mut self, grace: Duration) -> Option<Duration> {
        let deadline = Instant::now() + grace;
        let ordered = |resume: Option<f64>, stop: Option<f64>| matches!((resume, stop), (Some(r), Some(s)) if s > r);
        while !ordered(self.event_resume, self.event_stop) && Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            // A read timeout just means no more messages are in flight.
            let Ok(msg) = self.read_message(remaining) else {
                break;
            };
            self.note_event(&msg);
        }
        let (resume, stop) = (self.event_resume?, self.event_stop?);
        let span = stop - resume;
        (span > 0.0).then(|| Duration::from_secs_f64(span))
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
                // Put the partial message BACK before bailing: timeouts are
                // routine (event drains use near-zero deadlines), and dropping
                // a half-read message here would desync every later frame.
                let partial = String::from_utf8_lossy(&buf[..buf.len().min(200)]).into_owned();
                self.carry = buf;
                bail!("QMP read timed out with no complete message; partial: {partial:?}");
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
                Err(e) => {
                    // Same discipline for transient socket errors: whatever was
                    // read so far stays queued for the next call.
                    self.carry = buf;
                    return Err(e).context("reading QMP socket");
                }
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
        let endp = value.find([',', '}', '\n']).unwrap_or(value.len());
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
        assert_eq!(
            extract_field(msg, "return").as_deref(),
            Some("{\"running\": true}")
        );
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
        assert_eq!(
            parse_xp_word("0x3ff44004: 0xdeadbeef\r\n"),
            Some(0xdeadbeef)
        );
        assert_eq!(parse_xp_word("no hex here"), None);
    }
}
