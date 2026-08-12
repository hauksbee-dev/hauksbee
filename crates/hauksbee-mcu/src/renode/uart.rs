//! Host side of a Renode UART socket terminal.
//!
//! Inside Renode we run
//! `emulation CreateServerSocketTerminal <port> "term" false` and
//! `connector Connect sysbus.<usart> term`. Renode then listens on `<port>`;
//! bytes the firmware transmits arrive on that socket, and bytes we write are
//! injected into the UART receiver. The trailing `false` disables Renode's
//! terminal config handshake so the stream is raw bytes both ways.
//!
//! Long-form how-and-why: docs/how-and-why/hauksbee-mcu/renode.md.

use anyhow::{Context, Result};
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
                        .context("setting Renode UART read timeout")?;
                    stream
                        .set_write_timeout(Some(Duration::from_secs(2)))
                        .context("setting Renode UART write timeout")?;
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
    pub fn write_bytes(&mut self, bytes: &[u8]) -> Result<usize> {
        crate::traits::write_uart_bytes_counted(&mut self.stream, bytes)
            .context("writing to Renode UART socket")
    }

    /// Drain any bytes the firmware has transmitted since the last call.
    pub fn drain(&mut self) -> Result<Vec<u8>> {
        crate::traits::drain_uart_bytes(&mut self.stream, "Renode")
    }
}
