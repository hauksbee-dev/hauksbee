//! Host side of a QEMU UART socket.
//!
//! QEMU is launched with `-serial tcp:127.0.0.1:<port>,server,nowait`, so it
//! listens on `<port>` and bridges UART0 (the ESP32 console UART) as a raw byte
//! stream: bytes the firmware transmits arrive on the socket, and bytes we write
//! are delivered to the firmware's UART receiver. This is the same bridge shape
//! the Renode backend uses, so the scheduler's UART handling is identical across
//! backends.
//!
//! Long-form how-and-why: docs/how-and-why/hauksbee-mcu/qemu.md.

use anyhow::{Context, Result};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

/// A connected QEMU UART socket.
pub struct UartSocket {
    stream: TcpStream,
}

impl UartSocket {
    /// Connect to QEMU's serial socket on `127.0.0.1:port`.
    pub fn connect(port: u16, connect_timeout: Duration) -> Result<Self> {
        let deadline = Instant::now() + connect_timeout;
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        loop {
            match TcpStream::connect_timeout(&addr, Duration::from_millis(500)) {
                Ok(stream) => {
                    stream
                        .set_read_timeout(Some(Duration::from_millis(20)))
                        .ok();
                    stream.set_nodelay(true).ok();
                    return Ok(UartSocket { stream });
                }
                Err(e) => {
                    if Instant::now() >= deadline {
                        return Err(e).context("connecting to QEMU UART socket");
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }

    /// Inject bytes into the firmware's UART receiver.
    pub fn write_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        self.stream
            .write_all(bytes)
            .context("writing to QEMU UART socket")?;
        self.stream.flush().ok();
        Ok(())
    }

    /// Drain any bytes the firmware has transmitted since the last call.
    pub fn drain(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut chunk = [0u8; 2048];
        loop {
            match self.stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&chunk[..n]),
                Err(ref e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    break
                }
                Err(_) => break,
            }
        }
        out
    }
}
