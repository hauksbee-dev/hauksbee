//! Host side of a Renode UART socket terminal.
//!
//! Inside Renode we run
//! `emulation CreateServerSocketTerminal <port> "term" false` and
//! `connector Connect sysbus.<usart> term`. Renode then listens on `<port>`;
//! bytes the firmware transmits arrive on that socket, and bytes we write are
//! injected into the UART receiver. The trailing `false` disables Renode's
//! terminal config handshake so the stream is raw bytes both ways.

use anyhow::{Context, Result};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// A connected UART socket terminal.
pub struct UartSocket {
    stream: TcpStream,
}

impl UartSocket {
    /// Connect to a Renode socket terminal listening on `127.0.0.1:port`.
    pub fn connect(port: u16, connect_timeout: Duration) -> Result<Self> {
        let deadline = std::time::Instant::now() + connect_timeout;
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
                    if std::time::Instant::now() >= deadline {
                        return Err(e).context("connecting to Renode UART socket");
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
            .context("writing to Renode UART socket")?;
        self.stream.flush().ok();
        Ok(())
    }

    /// Drain any bytes the firmware has transmitted since the last call.
    pub fn drain(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut chunk = [0u8; 1024];
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
