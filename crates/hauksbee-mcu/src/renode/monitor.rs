//! Line-oriented TCP client for the Renode Monitor.
//!
//! Renode, launched with `--disable-xwt -P <port>`, listens on a TCP socket
//! and speaks a plain ASCII protocol: write a newline-terminated command, read
//! back the echoed command, any output lines, and finally a prompt of the form
//! `(machine-name) `. We frame each request/response on that trailing prompt.
//!
//! Launching with `-p` (plain) strips ANSI colour codes, which keeps parsing
//! simple. We still tolerate stray escape sequences defensively.
//!

use crate::external::{connect_loopback, read_step};
use anyhow::{bail, Context, Result};
use std::io::Write;
use std::net::TcpStream;
use std::time::{Duration, Instant};

/// A connected Renode Monitor session.
pub struct Monitor {
    stream: TcpStream,
    /// Bytes read past the previous prompt, kept for the next read.
    carry: String,
    /// Default per-command timeout.
    timeout: Duration,
}

impl Monitor {
    /// Connect to a Renode Monitor listening on `127.0.0.1:port`, retrying
    /// until `connect_timeout` elapses (Renode takes several seconds to bind).
    /// `dead` short-circuits the wait once the Renode process has exited (see
    /// [`connect_loopback`]).
    pub fn connect(
        port: u16,
        connect_timeout: Duration,
        dead: impl FnMut() -> Option<String>,
    ) -> Result<Self> {
        let stream = connect_loopback("Renode monitor", port, connect_timeout, dead)?;
        stream
            .set_read_timeout(Some(Duration::from_millis(200)))
            .ok();
        let mut m = Monitor {
            stream,
            carry: String::new(),
            timeout: Duration::from_secs(30),
        };
        // Drain the startup banner / first prompt.
        let _ = m.read_until_prompt(Duration::from_secs(5));
        Ok(m)
    }

    /// Set the default per-command response timeout.
    pub fn set_timeout(&mut self, t: Duration) {
        self.timeout = t;
    }

    /// Send a command and return its output (echo and trailing prompt stripped).
    pub fn command(&mut self, cmd: &str) -> Result<String> {
        let timeout = self.timeout;
        self.command_with_timeout(cmd, timeout)
    }

    /// Send a command with an explicit timeout.
    pub fn command_with_timeout(&mut self, cmd: &str, timeout: Duration) -> Result<String> {
        self.stream
            .write_all(cmd.as_bytes())
            .context("writing command to Renode monitor")?;
        // The newline matters as much as the command: a partial line leaves the
        // Monitor waiting forever, so propagate any failure rather than swallow.
        self.stream
            .write_all(b"\n")
            .context("writing newline to Renode monitor")?;
        self.stream.flush().ok();

        let raw = self.read_until_prompt(timeout)?;
        Ok(clean_response(cmd, &raw))
    }

    /// Send a command and refuse a response that reports a Renode-side
    /// failure.
    ///
    /// The Monitor carries no exit status: a rejected command answers with
    /// prose on the same channel as a successful one, so the words `error`,
    /// `exception` and `failed` are the only failure signal there is. Callers
    /// that must not silently proceed on a rejected command use this instead of
    /// [`Monitor::command`].
    pub fn run(&mut self, cmd: &str) -> Result<String> {
        let resp = self.command(cmd)?;
        Self::refuse_failure(cmd, resp)
    }

    /// [`Monitor::run`] with an explicit per-command timeout.
    pub fn run_with_timeout(&mut self, cmd: &str, timeout: Duration) -> Result<String> {
        let resp = self.command_with_timeout(cmd, timeout)?;
        Self::refuse_failure(cmd, resp)
    }

    fn refuse_failure(cmd: &str, resp: String) -> Result<String> {
        if response_failed(&resp) {
            bail!("Renode command `{cmd}` failed: {resp}");
        }
        Ok(resp)
    }

    /// Read from the socket until a trailing prompt `(name) ` is seen.
    ///
    /// On timeout without a prompt this returns an error rather than a partial
    /// response: a half-read response would desynchronise every subsequent
    /// command, so it is better to fail loudly and let the backend be torn down.
    fn read_until_prompt(&mut self, timeout: Duration) -> Result<String> {
        let deadline = Instant::now() + timeout;
        let mut buf = std::mem::take(&mut self.carry).into_bytes();
        let end = loop {
            // Lossy on each check: the prompt is ASCII, so a split multibyte
            // sequence at the tail can never hide or fake one.
            if let Some(end) = prompt_index(&String::from_utf8_lossy(&buf)) {
                break end;
            }
            if !read_step(&mut self.stream, &mut buf, deadline, "Renode monitor")? {
                bail!(
                    "Renode monitor command timed out after {:?} with no prompt; \
                     partial buffer: {:?}",
                    timeout,
                    String::from_utf8_lossy(&buf)
                        .chars()
                        .take(200)
                        .collect::<String>()
                );
            }
        };
        // Split at the prompt: everything up to the prompt is this response;
        // anything after the prompt suffix is carried to the next read.
        let text = String::from_utf8_lossy(&buf).into_owned();
        let (resp, rest) = text.split_at(end);
        self.carry = rest
            .find(") ")
            .map(|i| rest[i + 2..].to_string())
            .unwrap_or_default();
        Ok(resp.to_string())
    }
}

/// Find the byte index where a trailing prompt `(name) ` begins, if the buffer
/// currently ends in one.
///
/// The Renode Monitor prompt is `(<machine-name>) ` with a trailing space and
/// is always the last thing on the wire for a completed command (no newline
/// after it). We require that exact shape: a `) ` suffix (ignoring trailing
/// spaces only, never newlines), with a simple identifier inside the parens.
/// This deliberately rejects exception messages like `(FileNotFoundException)\n`
/// because those are followed by a newline and more output, not an end-of-buffer
/// space, so they never masquerade as a prompt.
fn prompt_index(buf: &str) -> Option<usize> {
    // Only spaces may trail the prompt; a trailing newline means this is not a
    // settled prompt (more output is coming).
    let stripped = buf.trim_end_matches(' ');
    if !stripped.ends_with(')') {
        return None;
    }
    let open = stripped.rfind('(')?;
    let close = stripped.len() - 1; // index of the ')'
    let inner = &stripped[open + 1..close];
    // The prompt's name is a plain identifier (letters, digits, '-', '_'); an
    // empty name or one with spaces/punctuation is not a prompt.
    if inner.is_empty()
        || !inner
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    // The '(' must start a line (preceded by a newline or be at the buffer head).
    let preceding = stripped[..open].chars().last();
    if preceding.is_none() || preceding == Some('\n') || preceding == Some('\r') {
        Some(open)
    } else {
        None
    }
}

/// Strip ANSI escapes, the echoed command line, and trailing whitespace.
fn clean_response(cmd: &str, raw: &str) -> String {
    let no_ansi = strip_ansi(raw);
    let mut lines: Vec<&str> = no_ansi.split('\n').collect();
    // Drop a leading echo of the command. Renode echoes the exact command we
    // typed as the first line; match it precisely (after trimming a possible
    // leading prompt fragment) rather than by a loose substring, so a genuine
    // first output line that merely mentions the command is not eaten.
    let needle = cmd.trim();
    if let Some(first) = lines.first() {
        let f = first.trim_start_matches(|c: char| c != '(' && !c.is_alphanumeric());
        if f.trim() == needle || first.trim() == needle {
            lines.remove(0);
        }
    }
    lines
        .join("\n")
        .trim_matches(|c| c == '\r' || c == '\n' || c == ' ')
        .to_string()
}

/// Remove ANSI CSI escape sequences (`ESC [ ... m` and friends).
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip until a letter (the final byte of a CSI sequence).
            if chars.peek() == Some(&'[') {
                chars.next();
                for cc in chars.by_ref() {
                    if cc.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_detected_at_end() {
        // Real prompts: `(name) ` with a trailing space, at buffer end.
        assert!(prompt_index("foo\n(machine-0) ").is_some());
        assert!(prompt_index("0x0000000C\r\n(f103) ").is_some());
        // No-space variant at the very end is still accepted.
        assert!(prompt_index("0x0000000C\r\n(f103)").is_some());
        assert!(prompt_index("no prompt here").is_none());
        // A parenthesised value mid-line is not a prompt.
        assert!(prompt_index("value (3) more").is_none());
    }

    #[test]
    fn exception_message_is_not_a_prompt() {
        // Renode error text ends in `(SomeException)` but is followed by a
        // newline and the real prompt; the bare exception must not be framed.
        assert!(prompt_index("Could not load\n(FileNotFoundException)\n").is_none());
        // ...but once the real prompt arrives it frames correctly.
        assert!(prompt_index("Could not load\n(FileNotFoundException)\n(f103) ").is_some());
    }

    #[test]
    fn ansi_stripped() {
        let s = "\x1b[33;1m(f103) \x1b[0m0x00";
        assert_eq!(strip_ansi(s), "(f103) 0x00");
    }

    #[test]
    fn echo_and_prompt_removed() {
        let raw = "sysbus.gpioPortC ReadDoubleWord 0xC\n\r0x00000000\r\r\n";
        let cleaned = clean_response("sysbus.gpioPortC ReadDoubleWord 0xC", raw);
        assert_eq!(cleaned, "0x00000000");
    }

    #[test]
    fn echo_not_eaten_when_only_mentioned() {
        // A real output line that merely contains the command text is kept.
        let raw = "start\nStarting emulation...\r\n";
        let cleaned = clean_response("start", raw);
        assert_eq!(cleaned, "Starting emulation...");
    }
}

/// True if a Monitor response reports a Renode-side failure. See
/// [`Monitor::run`] for why prose is the only signal available.
pub fn response_failed(resp: &str) -> bool {
    let lower = resp.to_lowercase();
    lower.contains("error") || lower.contains("exception") || lower.contains("failed")
}
