//! End-to-end server test: boot the demo engine (real AVR firmware on
//! simavr), connect a raw websocket client, drive the protocol, and verify
//! frames + serial interaction.
//!
//! Whole file is gated on `avr`: the demo engine IS the simavr backend, so in
//! the MIT-clean shape (`--no-default-features --features renode,qemu`) there
//! is nothing to test here and the file compiles to nothing.
#![cfg(feature = "avr")]

use hauksbee_server::engine::McuDemoEngine;
use hauksbee_server::protocol::ServerMessage;
use hauksbee_server::Server;
use std::path::PathBuf;

fn demo_hex() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/firmware/demo/demo.hex");
    p.exists().then_some(p)
}

#[tokio::test]
async fn ws_protocol_roundtrip() {
    let Some(hex) = demo_hex() else {
        eprintln!("SKIP: demo.hex missing");
        return;
    };
    let engine = McuDemoEngine::new(&hex, "demo", "/boards/demo.kicad_pcb").unwrap();
    let server = Server::new(Box::new(engine));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = server.router(None);
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    // Raw websocket handshake without extra deps.
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(
            format!(
                "GET /ws HTTP/1.1\r\nHost: {addr}\r\nUpgrade: websocket\r\n\
                 Connection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                 Sec-WebSocket-Version: 13\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();

    // Read until the end of the HTTP upgrade response; anything beyond
    // "\r\n\r\n" is already websocket data and must be kept.
    let mut handshake = Vec::new();
    let header_end = loop {
        let mut buf = vec![0u8; 4096];
        let n = stream.read(&mut buf).await.unwrap();
        assert!(n > 0, "server closed during handshake");
        handshake.extend_from_slice(&buf[..n]);
        if let Some(pos) = handshake.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let response = String::from_utf8_lossy(&handshake[..header_end]);
    assert!(
        response.contains("101"),
        "websocket upgrade failed: {response}"
    );
    let mut residue: Vec<u8> = handshake[header_end..].to_vec();

    // First server frame must be BoardInfo.
    let payload = read_ws_text(&mut stream, &mut residue).await;
    let msg: ServerMessage = serde_json::from_str(&payload).unwrap();
    match msg {
        ServerMessage::BoardInfo(info) => {
            assert_eq!(info.mcus.len(), 1);
            assert_eq!(info.name, "demo");
        }
        other => panic!("expected BoardInfo first, got {other:?}"),
    }

    // Send Play (client frames must be masked).
    write_ws_text(&mut stream, r#"{"type":"Play"}"#).await;

    // We must receive SimFrames; within ~2s the LED net should be present.
    let mut saw_frame = false;
    for _ in 0..120 {
        let payload = read_ws_text(&mut stream, &mut residue).await;
        if let Ok(ServerMessage::SimFrame(f)) = serde_json::from_str(&payload) {
            assert!(f.net_voltages.contains_key("D13_LED"));
            saw_frame = true;
            break;
        }
    }
    assert!(saw_frame, "no SimFrame received");

    // Serial command: 'i' returns the ident string via frame uart payloads.
    write_ws_text(&mut stream, r#"{"type":"Serial","mcu":"U1","data":[105]}"#).await;
    let mut uart_text = String::new();
    for _ in 0..240 {
        let payload = read_ws_text(&mut stream, &mut residue).await;
        if let Ok(ServerMessage::SimFrame(f)) = serde_json::from_str(&payload) {
            for bytes in f.uart.values() {
                uart_text.push_str(&String::from_utf8_lossy(bytes));
            }
            if uart_text.contains("hauksbee-demo v1") {
                break;
            }
        }
    }
    assert!(
        uart_text.contains("hauksbee-demo v1"),
        "ident not received over virtual serial, got {uart_text:?}"
    );
}

/// Minimal websocket reader: handles server frames (FIN, text, no mask),
/// buffering across reads. Panics on anything unexpected — it's a test.
async fn read_ws_text(stream: &mut tokio::net::TcpStream, residue: &mut Vec<u8>) -> String {
    use tokio::io::AsyncReadExt;
    loop {
        // Try to decode one frame from the residue buffer.
        if residue.len() >= 2 {
            let len_byte = residue[1] & 0x7f;
            let (header, len) = match len_byte {
                126 if residue.len() >= 4 => {
                    (4, u16::from_be_bytes([residue[2], residue[3]]) as usize)
                }
                127 if residue.len() >= 10 => {
                    let mut b = [0u8; 8];
                    b.copy_from_slice(&residue[2..10]);
                    (10, u64::from_be_bytes(b) as usize)
                }
                n if n < 126 => (2, n as usize),
                _ => (0, usize::MAX), // need more bytes for the header
            };
            if header > 0 && residue.len() >= header + len {
                let opcode = residue[0] & 0x0f;
                let payload = residue[header..header + len].to_vec();
                residue.drain(..header + len);
                if opcode == 1 {
                    return String::from_utf8(payload).unwrap();
                }
                continue; // ping/close/etc: skip
            }
        }
        let mut buf = vec![0u8; 16384];
        let n = stream.read(&mut buf).await.unwrap();
        assert!(n > 0, "server closed");
        residue.extend_from_slice(&buf[..n]);
    }
}

async fn write_ws_text(stream: &mut tokio::net::TcpStream, text: &str) {
    use tokio::io::AsyncWriteExt;
    let payload = text.as_bytes();
    let mask = [0x12u8, 0x34, 0x56, 0x78];
    let mut frame = vec![0x81u8];
    if payload.len() < 126 {
        frame.push(0x80 | payload.len() as u8);
    } else {
        frame.push(0x80 | 126);
        frame.extend((payload.len() as u16).to_be_bytes());
    }
    frame.extend(mask);
    frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    stream.write_all(&frame).await.unwrap();
}
