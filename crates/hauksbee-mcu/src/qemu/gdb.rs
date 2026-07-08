//! Minimal GDB Remote Serial Protocol (RSP) client over TCP.
//!
//! QEMU's gdbstub (enabled with `-gdb tcp:...`) speaks RSP: each packet is
//! `$<payload>#<checksum-hex>`, acknowledged with a single `+`. We use it for
//! the two operations the QMP human monitor cannot do cleanly:
//!
//!   - **Memory write** (`M<addr>,<len>:<hexbytes>`): driving the ESP32
//!     `GPIO_IN_REG` so the circuit's solved logic levels reach the firmware.
//!     QEMU's HMP has no physical-word poke; the RSP `M` packet writes guest
//!     memory directly.
//!   - **Memory read** (`m<addr>,<len>`): a second, independent path to read
//!     `GPIO_OUT_REG`. The backend prefers the QMP `xp` read but can fall back
//!     here.
//!
//! Why both QMP and gdbstub? QMP owns run/stop and the icount-budgeted time
//! stepping; gdbstub owns clean word-granular memory writes. They attach to the
//! same running QEMU over separate TCP sockets and do not interfere: the backend
//! only touches the gdbstub while the vCPU is paused (between RunFor chunks),
//! which is exactly when reading/writing peripheral registers is well defined.
//!
//! This is a deliberately tiny RSP implementation: no register access, no
//! breakpoints, just `M`/`m` with ack handling, which is all the GPIO bridge
//! needs. Addresses are physical (the ESP32 GPIO matrix is not behind an MMU
//! remap for these registers, so the gdbstub's address space reaches them).
//!
//! Long-form how-and-why: docs/how-and-why/hauksbee-mcu/qemu.md.

use anyhow::{bail, Context, Result};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

/// A connected gdbstub RSP session.
pub struct GdbStub {
    stream: TcpStream,
    timeout: Duration,
}

impl GdbStub {
    /// Connect to a QEMU gdbstub on `127.0.0.1:port`, retrying until
    /// `connect_timeout` elapses.
    pub fn connect(port: u16, connect_timeout: Duration) -> Result<Self> {
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        let deadline = Instant::now() + connect_timeout;
        let stream = loop {
            match TcpStream::connect_timeout(&addr, Duration::from_millis(500)) {
                Ok(s) => break s,
                Err(e) => {
                    if Instant::now() >= deadline {
                        return Err(e).context("connecting to QEMU gdbstub");
                    }
                    std::thread::sleep(Duration::from_millis(150));
                }
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_millis(200)))
            .ok();
        stream.set_nodelay(true).ok();
        let mut g = GdbStub {
            stream,
            timeout: Duration::from_secs(10),
        };
        // Tell the stub we don't want ack-mode noise beyond the basic `+`. We
        // keep ack-mode on (simpler), so nothing to negotiate; just confirm the
        // link with a no-op query.
        let _ = g.packet("qSupported:")?;
        Ok(g)
    }

    /// Read `len` bytes of guest memory at physical `addr`. Returns the bytes
    /// little-endian as stored.
    pub fn read_mem(&mut self, addr: u32, len: usize) -> Result<Vec<u8>> {
        let resp = self.packet(&format!("m{addr:x},{len:x}"))?;
        if resp.starts_with('E') {
            bail!("gdbstub memory read error at 0x{addr:08x}: {resp}");
        }
        let mut out = Vec::with_capacity(len);
        let bytes = resp.as_bytes();
        let mut i = 0;
        while i + 1 < bytes.len() {
            let hi = hex_val(bytes[i]).context("bad hex in gdb mem read")?;
            let lo = hex_val(bytes[i + 1]).context("bad hex in gdb mem read")?;
            out.push((hi << 4) | lo);
            i += 2;
        }
        Ok(out)
    }

    /// Read one little-endian 32-bit word at physical `addr`.
    pub fn read_u32(&mut self, addr: u32) -> Result<u32> {
        let b = self.read_mem(addr, 4)?;
        if b.len() < 4 {
            bail!("short gdb word read at 0x{addr:08x}: got {} bytes", b.len());
        }
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// Write one little-endian 32-bit word at physical `addr`.
    pub fn write_u32(&mut self, addr: u32, val: u32) -> Result<()> {
        self.write_mem(addr, &val.to_le_bytes())
    }

    /// Write `bytes` to guest memory at physical `addr` (RSP `M` packet).
    pub fn write_mem(&mut self, addr: u32, bytes: &[u8]) -> Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let resp = self.packet(&format!("M{addr:x},{:x}:{hex}", bytes.len()))?;
        if resp != "OK" {
            bail!("gdbstub memory write to 0x{addr:08x} failed: {resp}");
        }
        Ok(())
    }

    /// Send one RSP packet and return its decoded payload (ack handled).
    fn packet(&mut self, payload: &str) -> Result<String> {
        let frame = encode_packet(payload);
        self.stream
            .write_all(frame.as_bytes())
            .context("writing gdb packet")?;
        self.stream.flush().ok();
        // Expect a `+` ack then the response packet. Some stubs interleave; we
        // tolerate a leading `+`/`-` and then read `$...#xx`.
        self.read_packet()
    }

    /// Read a `$payload#cc` packet from the stream, ack it, and return payload.
    fn read_packet(&mut self) -> Result<String> {
        let deadline = Instant::now() + self.timeout;
        let mut buf: Vec<u8> = Vec::new();
        loop {
            if let Some(p) = try_extract_packet(&buf) {
                // Acknowledge receipt.
                let _ = self.stream.write_all(b"+");
                self.stream.flush().ok();
                return Ok(p);
            }
            if Instant::now() >= deadline {
                bail!(
                    "gdb packet read timed out; partial: {:?}",
                    String::from_utf8_lossy(&buf)
                );
            }
            let mut chunk = [0u8; 1024];
            match self.stream.read(&mut chunk) {
                Ok(0) => bail!("gdbstub connection closed"),
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(ref e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    std::thread::sleep(Duration::from_millis(3));
                }
                Err(e) => return Err(e).context("reading gdbstub socket"),
            }
        }
    }
}

/// Encode an RSP packet: `$<payload>#<2-hex checksum>`.
fn encode_packet(payload: &str) -> String {
    let sum: u8 = payload.bytes().fold(0u8, |a, b| a.wrapping_add(b));
    format!("${payload}#{sum:02x}")
}

/// Extract the first complete `$...#cc` packet payload from `buf`, ignoring any
/// leading `+`/`-` acks. Returns `None` if no complete packet is present yet.
fn try_extract_packet(buf: &[u8]) -> Option<String> {
    let dollar = buf.iter().position(|&b| b == b'$')?;
    let hash_rel = buf[dollar..].iter().position(|&b| b == b'#')?;
    let hash = dollar + hash_rel;
    // Need two checksum hex digits after '#'.
    if buf.len() < hash + 3 {
        return None;
    }
    let payload = &buf[dollar + 1..hash];
    Some(String::from_utf8_lossy(payload).into_owned())
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_checksum() {
        // 'm' (0x6d) only: checksum is 0x6d.
        assert_eq!(encode_packet("m"), "$m#6d");
    }

    #[test]
    fn extracts_packet_with_leading_ack() {
        let buf = b"+$OK#9a";
        assert_eq!(try_extract_packet(buf).as_deref(), Some("OK"));
    }

    #[test]
    fn incomplete_packet_is_none() {
        assert!(try_extract_packet(b"$OK#9").is_none());
        assert!(try_extract_packet(b"$OK").is_none());
    }

    #[test]
    fn parses_word_roundtrip_hex() {
        // M packet for 0x20 at addr 0x3ff4403c, 4 bytes LE.
        let p = encode_packet("M3ff4403c,4:20000000");
        assert!(p.starts_with("$M3ff4403c,4:20000000#"));
    }
}
