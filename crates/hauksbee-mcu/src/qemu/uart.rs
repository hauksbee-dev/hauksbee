//! Host side of a QEMU UART socket.
//!
//! QEMU is launched with `-serial tcp:127.0.0.1:<port>,server,nowait`, so it
//! listens on `<port>` and bridges UART0 (the ESP32 console UART) as a raw byte
//! stream: bytes the firmware transmits arrive on the socket, and bytes we write
//! are delivered to the firmware's UART receiver. This is the same bridge shape
//! the Renode backend uses, so the scheduler's UART handling is identical across
//! backends.
//!

use anyhow::{Context, Result};
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
                        .context("setting QEMU UART read timeout")?;
                    stream
                        .set_write_timeout(Some(Duration::from_secs(2)))
                        .context("setting QEMU UART write timeout")?;
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
    pub fn write_bytes(&mut self, bytes: &[u8]) -> Result<usize> {
        crate::traits::write_uart_bytes_counted(&mut self.stream, bytes)
            .context("writing to QEMU UART socket")
    }

    /// Drain any bytes the firmware has transmitted since the last call.
    pub fn drain(&mut self) -> Result<Vec<u8>> {
        crate::traits::drain_uart_bytes(&mut self.stream, "QEMU")
    }
}
