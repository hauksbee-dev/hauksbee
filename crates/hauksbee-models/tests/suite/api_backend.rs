//! The api backend's request shape, proven against a local mock endpoint.
//!
//! A stub chat-completions server on a loopback `TcpListener` captures exactly
//! what `--backend api` sends: the method and path, the Authorization header
//! built from the named env var, the model field, and the prompt inside
//! `messages`. No real network, no SDK, and the key never appears anywhere the
//! test could not see it arrive.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;

/// A syntactically and physically valid diode card for the stub to answer
/// with, so the full parse + validate + write path runs.
const DIODE_TOML: &str = r#"[[models]]
id = "1n914test"
kind = "diode"
description = "stub reply"
[models.match]
value_re = "(?i)^1N914TEST"
[models.params]
is = 2.5e-9
n = 1.75
rs = 0.6
"#;

/// Serve exactly one HTTP request, sending back a chat-completions reply
/// whose content is `DIODE_TOML`, and hand the raw request to the test.
fn one_shot_server() -> (String, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        // Read headers, then exactly Content-Length body bytes.
        let body_len = loop {
            let n = stream.read(&mut chunk).expect("read request");
            buf.extend_from_slice(&chunk[..n]);
            let text = String::from_utf8_lossy(&buf);
            if let Some(head_end) = text.find("\r\n\r\n") {
                let len = text
                    .lines()
                    .find_map(|l| l.strip_prefix("Content-Length: "))
                    .or_else(|| {
                        text.lines()
                            .find_map(|l| l.strip_prefix("content-length: "))
                    })
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                break (head_end + 4, len);
            }
        };
        while buf.len() < body_len.0 + body_len.1 {
            let n = stream.read(&mut chunk).expect("read body");
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        let reply_body = serde_json::json!({
            "choices": [{ "message": { "role": "assistant", "content": DIODE_TOML } }]
        })
        .to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{}",
            reply_body.len(),
            reply_body
        );
        stream.write_all(response.as_bytes()).expect("write reply");
        tx.send(String::from_utf8_lossy(&buf).into_owned()).ok();
    });
    (base, rx)
}

#[test]
fn api_backend_sends_the_documented_request_shape() {
    let (base, rx) = one_shot_server();

    let dir = tempfile::tempdir().unwrap();
    // A text stand-in for the PDF: the backend under test never renders it,
    // and extract_pdf_text degrades to a placeholder when it cannot. It must
    // still carry the %PDF magic, because `run` prechecks the magic bytes
    // before choosing a backend so a renamed HTML page is never shipped to an
    // LLM; a header-less stand-in is rejected before any request goes out.
    let pdf = dir.path().join("1n914test.pdf");
    std::fs::write(&pdf, "%PDF-1.4\nIF=200mA VRRM=100V test datasheet body").unwrap();

    let key_env = "HAUKSBEE_API_BACKEND_TEST_KEY";
    // SAFETY: this variable is unique to this test; nothing else reads it.
    unsafe { std::env::set_var(key_env, "test-key-123") };

    let args =
        hauksbee_models::datasheet::Args::new(pdf, "1N914TEST".to_string(), "diode".to_string())
            .out_dir(Some(dir.path().to_path_buf()))
            .model(Some("test-model".to_string()))
            .backend(Some(hauksbee_models::datasheet::Backend::Api))
            .api_base(Some(base.clone()))
            .api_key_env(Some(key_env.to_string()));

    hauksbee_models::datasheet::run(args).expect("extraction against the stub endpoint");
    unsafe { std::env::remove_var(key_env) };

    let request = rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();

    // Method and path: POST to <base>/chat/completions.
    assert!(
        request.starts_with("POST /chat/completions HTTP/1.1"),
        "request line wrong:\n{}",
        request.lines().next().unwrap_or("")
    );
    // The Authorization header carries the key read from the named env var.
    assert!(
        request.contains("Authorization: Bearer test-key-123"),
        "missing/wrong Authorization header"
    );
    assert!(request.contains("Content-Type: application/json"));

    // The JSON body: the chosen model, and the prompt inside messages.
    let body_start = request.find("\r\n\r\n").expect("body present") + 4;
    let body: serde_json::Value =
        serde_json::from_str(&request[body_start..]).expect("body is JSON");
    assert_eq!(body["model"], "test-model");
    let messages = body["messages"].as_array().expect("messages array");
    assert_eq!(messages.len(), 2, "system + user");
    let user = messages[1]["content"].as_str().expect("user content");
    assert!(
        user.contains("1N914TEST") && user.contains("DATASHEET TEXT"),
        "the extraction prompt must land in messages[1]"
    );

    // And the reply flowed through parse + validate to the output file.
    let written =
        std::fs::read_to_string(dir.path().join("1N914TEST.toml")).expect("model card written");
    assert!(written.starts_with("[[models]]"));
    assert!(written.contains("1n914test"));
}
