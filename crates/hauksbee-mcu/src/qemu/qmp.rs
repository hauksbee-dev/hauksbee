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

use crate::external::{connect_loopback, read_step};
use anyhow::{bail, Context, Result};
use std::io::Write;
use std::net::TcpStream;
use std::time::{Duration, Instant};

/// A connected QMP session, already out of capabilities-negotiation mode.
pub struct Qmp {
    stream: TcpStream,
    /// Bytes read past the end of the last framed JSON object.
    carry: Vec<u8>,
    timeout: Duration,
}

impl Qmp {
    /// Connect to a QMP server on `127.0.0.1:port`, retrying until
    /// `connect_timeout` elapses (QEMU takes a moment to bind), then perform
    /// the capabilities handshake so the session is ready for commands.
    pub fn connect(port: u16, connect_timeout: Duration) -> Result<Self> {
        let stream = connect_loopback("QEMU QMP", port, connect_timeout, || None)?;
        stream
            .set_read_timeout(Some(Duration::from_millis(200)))
            .ok();
        let mut q = Self {
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
    pub const fn set_timeout(&mut self, t: Duration) {
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

    /// Resume, hold `window`, pause, and return how long the guest's virtual
    /// clock ran: `cont` SENT to `stop` SENT on the host's monotonic clock.
    ///
    /// QEMU thaws the virtual clock when it dispatches `cont` and freezes it
    /// when it dispatches `stop` (`do_vm_stop` disables ticks first), so both
    /// ends lag their send by the same kind of dispatch delay, which cancels
    /// to within dispatch jitter. What must NOT be credited is everything
    /// after the freeze: QEMU then waits for the vCPU threads to park before
    /// it emits `STOP` and replies, and that wait grows without bound under
    /// host load. The `RESUME`→`STOP` event span credited it, over-crediting
    /// the guest's CCOUNT by up to 39% on an oversubscribed host.
    pub fn run_for(&mut self, window: Duration) -> Result<Duration> {
        let sent = Instant::now();
        self.cont().context("QMP cont")?;
        std::thread::sleep(window);
        let ran = sent.elapsed();
        self.stop().context("QMP stop")?;
        Ok(ran)
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

    /// Read `count` consecutive 32-bit words from guest physical memory in ONE
    /// monitor round trip (`xp /<count>wx`). The GPIO-matrix output-select
    /// block is 32+ words, so reading it word by word would cost as many
    /// round trips; `xp` prints four words per line.
    ///
    /// # Errors
    ///
    /// The monitor command fails, or its output does not hold exactly `count`
    /// words (a short read must never decode as "unrouted").
    pub fn read_words(&mut self, addr: u32, count: usize) -> Result<Vec<u32>> {
        let out = self.hmp(&format!("xp /{count}wx 0x{addr:08x}"))?;
        let words = parse_xp_words(&out);
        if words.len() == count {
            Ok(words)
        } else {
            anyhow::bail!(
                "xp /{count}wx 0x{addr:08x} returned {} words: {out:?}",
                words.len()
            )
        }
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
                let msg = String::from_utf8_lossy(&buf[..=end]).into_owned();
                self.carry = buf.split_off(end + 1);
                return Ok(msg);
            }
            match read_step(&mut self.stream, &mut buf, deadline, "QEMU QMP") {
                Ok(true) => {}
                // Put the partial message BACK before bailing: timeouts are
                // routine (event drains use near-zero deadlines) and transient
                // socket errors recover, so dropping a half-read message here
                // would desync every later frame.
                Ok(false) => {
                    let partial = String::from_utf8_lossy(&buf[..buf.len().min(200)]).into_owned();
                    self.carry = buf;
                    bail!("QMP read timed out with no complete message; partial: {partial:?}");
                }
                Err(e) => {
                    self.carry = buf;
                    return Err(e);
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
                Some('\\') | None => out.push('\\'),
                Some('/') => out.push('/'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
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
    let hex: String = after.chars().take_while(char::is_ascii_hexdigit).collect();
    if hex.is_empty() {
        return None;
    }
    u32::from_str_radix(&hex, 16).ok()
}

/// Parse every word out of a multi-word `xp /Nwx` reply. Each line is an
/// address, a colon, then up to four `0x` words:
/// `000000003ff44530: 0x00000100 0x00000100 0x00000100 0x00000100`.
/// The address before the colon carries no `0x`, and text without the colon is
/// ignored, so a stray prompt or error line contributes no words.
fn parse_xp_words(out: &str) -> Vec<u32> {
    out.lines()
        .filter_map(|line| line.split_once(':').map(|(_, words)| words))
        .flat_map(str::split_whitespace)
        .filter_map(|tok| tok.strip_prefix("0x"))
        .filter_map(|hex| u32::from_str_radix(hex, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multi_word_xp_replies() {
        let out = "000000003ff44530: 0x00000100 0x00000047 0x00000100 0x00000400\r\n\
                   000000003ff44540: 0x00000100 0xdeadbeef 0x00000100 0x00000100\r\n";
        assert_eq!(
            parse_xp_words(out),
            [0x100, 0x47, 0x100, 0x400, 0x100, 0xdead_beef, 0x100, 0x100]
        );
        // A ragged last line and noise lines parse as what they hold.
        assert_eq!(parse_xp_words("(qemu)\n3ff44530: 0x1 0x2\n"), [1, 2]);
        assert!(parse_xp_words("no words here").is_empty());
    }

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
            Some(0xdead_beef)
        );
        assert_eq!(parse_xp_word("no hex here"), None);
    }

    /// A fake QEMU with QEMU's stop ordering: freeze the virtual clock on
    /// `stop`, THEN spend `pause` parking vCPUs, THEN emit `STOP` and reply.
    /// Returns (credited, virtual time the fake actually ran).
    fn run_against_slow_pause(window: Duration, pause: Duration) -> (Duration, Duration) {
        use std::io::{BufRead, BufReader, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let fake = std::thread::spawn(move || {
            let (mut out, _) = listener.accept().unwrap();
            let lines = BufReader::new(out.try_clone().unwrap()).lines();
            let stamp = |name: &str| {
                let t = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap();
                format!("{{\"timestamp\": {{\"seconds\": {}, \"microseconds\": {}}}, \"event\": \"{name}\"}}\n", t.as_secs(), t.subsec_micros())
            };
            out.write_all(b"{\"QMP\": {}}\n").unwrap();
            let (mut thawed, mut ran) = (Instant::now(), Duration::ZERO);
            for line in lines.map(Result::unwrap) {
                if line.contains("\"cont\"") {
                    thawed = Instant::now();
                    out.write_all(stamp("RESUME").as_bytes()).unwrap();
                } else if line.contains("\"stop\"") {
                    ran = thawed.elapsed();
                    std::thread::sleep(pause);
                    out.write_all(stamp("STOP").as_bytes()).unwrap();
                }
                out.write_all(b"{\"return\": {}}\n").unwrap();
            }
            ran
        });
        let mut qmp = Qmp::connect(port, Duration::from_secs(5)).unwrap();
        let credited = qmp.run_for(window).unwrap();
        drop(qmp);
        (credited, fake.join().unwrap())
    }

    /// Regression for the Windows CI 0.489 ratio: a vCPU-parking stall after
    /// the freeze is host load, not guest time. The RESUME→STOP event span
    /// credits ~`window + pause` here and fails deterministically.
    #[test]
    fn a_slow_pause_is_not_credited_as_guest_time() {
        let (window, pause) = (Duration::from_millis(5), Duration::from_millis(300));
        let (credited, ran) = run_against_slow_pause(window, pause);
        assert!(
            credited >= window,
            "credited {credited:?} under the slept window"
        );
        assert!(
            credited.abs_diff(ran) < pause / 3,
            "credited {credited:?} but the fake guest clock ran {ran:?}"
        );
    }
}
