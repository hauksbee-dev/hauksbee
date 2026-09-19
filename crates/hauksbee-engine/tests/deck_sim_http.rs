//! `POST /api/sim` is `hauksbee sim` over HTTP, or it is a second simulator.
//!
//! These cases boot the real front door (the design routes the shipped `serve`
//! mounts, with the engine's own `deck_simulator`), post two real decks at it,
//! and require the `csv` field to be BYTE-IDENTICAL to what the CLI's
//! `sim_output_to_csv` writes for the same deck. No tolerance: identity is a
//! property of the code (one function, two callers), and this is the test that
//! keeps it one function.

#![cfg(feature = "serve")]

use hauksbee_engine::commands::common::deck_simulator;
use hauksbee_engine::commands::sim::{
    sim_output_to_csv, simulate_deck, simulate_deck_within, DECK_TIME_LIMIT,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const DECKS: &str = "../hauksbee-solve/tests/decks";

/// Boot the shipped front door on an ephemeral port and return its address.
async fn spawn() -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        hauksbee_server::serve_frontdoor_on_with_design(
            listener,
            None,
            hauksbee_engine::commands::common::design_analyzer(),
            deck_simulator(),
            None,
            None,
            None,
            "{\"preloaded\":false}".to_string(),
        )
        .await
        .unwrap();
    });
    addr
}

/// POST `deck` to `/api/sim` and return `(status line, parsed JSON body)`.
async fn post_deck(
    addr: std::net::SocketAddr,
    name: &str,
    deck: &str,
) -> (String, serde_json::Value) {
    let request = format!(
        "POST /api/sim HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\
         X-Board-Filename: {name}\r\nContent-Type: application/octet-stream\r\n\
         Content-Length: {}\r\n\r\n",
        deck.len()
    );
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    stream.write_all(deck.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();
    let mut buf = Vec::new();
    let _ = stream.read_to_end(&mut buf).await;
    let response = String::from_utf8_lossy(&buf).into_owned();
    let (head, body) = response
        .split_once("\r\n\r\n")
        .expect("a complete response");
    let status = head.lines().next().unwrap_or_default().to_string();
    (status, serde_json::from_str(body).expect(body))
}

/// The web answer for one deck must carry the CLI's own bytes.
async fn assert_matches_the_cli(addr: std::net::SocketAddr, file: &str, analysis: &str) {
    let deck = std::fs::read_to_string(format!("{DECKS}/{file}")).unwrap();
    let (status, json) = post_deck(addr, file, &deck).await;
    assert!(status.contains("200"), "{file}: {status} {json}");
    let run = simulate_deck(&deck, &[]).expect("the deck simulates");
    assert_eq!(json["analysis"], analysis, "{file} ran the wrong analysis");
    assert_eq!(
        json["csv"].as_str().unwrap(),
        sim_output_to_csv(&run.output),
        "{file}: the browser CSV is not the CLI's bytes"
    );
    // The chart's columns/rows are that same CSV, read back: column 0 is the
    // x axis (time or frequency), and every row is as wide as the header.
    let columns = json["columns"].as_array().unwrap();
    let rows = json["rows"].as_array().unwrap();
    assert_eq!(rows.len(), run.output.rows.len(), "{file}: row count");
    assert!(
        rows.iter()
            .all(|r| r.as_array().unwrap().len() == columns.len()),
        "{file}: a row is not as wide as the header"
    );
}

#[tokio::test]
async fn posted_decks_answer_with_the_cli_csv_byte_for_byte() {
    let addr = spawn().await;
    assert_matches_the_cli(addr, "rc_ac_bode.cir", "ac").await;
    assert_matches_the_cli(addr, "bjt_switch_tail.cir", "tran").await;
}

#[tokio::test]
async fn a_deck_that_needs_a_file_we_do_not_have_is_refused_not_guessed() {
    let addr = spawn().await;
    let deck = "dep\n.include models/bc547.lib\nV1 in 0 1\nR1 in 0 1k\n.op\n.end\n";
    let (status, json) = post_deck(addr, "dep.cir", deck).await;
    assert!(status.contains("422"), "{status} {json}");
    assert_eq!(json["ok"], false);
    let error = json["error"].as_str().unwrap();
    assert!(error.contains(".include"), "{error}");
    assert!(error.contains("self-contained"), "{error}");
}

#[test]
fn an_oversized_sweep_is_refused_before_it_is_solved_and_the_cap_is_named() {
    // 10 s at 1 ns is 10^10 points: no browser request may start that solve.
    let deck = "big\nV1 in 0 PULSE(0 1 0 1n 1n 1u 2u)\nR1 in 0 1k\n.tran 1n 10\n.end\n";
    let error = simulate_deck(deck, &[]).expect_err("must refuse");
    assert!(error.contains("200000"), "the cap must be named: {error}");
    // The same deck with a sane stop time runs.
    let ok = deck.replace(".tran 1n 10\n", ".tran 1n 10u\n");
    assert_eq!(simulate_deck(&ok, &[]).unwrap().analysis, "tran");
}

#[test]
fn a_deck_that_outlasts_the_browser_limit_is_refused_instead_of_holding_the_request() {
    // A limit of zero proves the brake without waiting a minute for it.
    let deck = "slow\nV1 in 0 1\nR1 in out 1k\nC1 out 0 1n\n.tran 1n 1u\n.end\n";
    let error = simulate_deck_within(deck, &[], std::time::Duration::ZERO)
        .expect_err("a deck past the limit is refused");
    assert!(error.contains("still solving"), "{error}");
    assert!(error.contains("hauksbee sim <deck.cir>"), "{error}");
    let run = simulate_deck_within(deck, &[], DECK_TIME_LIMIT).expect("inside the limit");
    assert_eq!(run.analysis, "tran");
}

#[test]
fn a_vendor_card_says_which_of_its_parameters_were_dropped() {
    // The ON Semiconductor 2N3904 card states XTF; hauksbee has no
    // transit-time-modulation equation, so the run must say the value was
    // dropped rather than imply the card was honoured.
    let card = |extra: &str| {
        format!(
            "vendor\nVB b 0 0.7\nVC c 0 5\nQ1 c b 0 Q2N3904\n\
             .model Q2N3904 NPN(IS=6.734f BF=416.4 VAF=74.03 IKF=66.78m \
             CJE=4.493p MJE=.2593 CJC=3.638p TF=301.2p TR=239.5n {extra})\n.op\n.end\n"
        )
    };
    let notes = |extra: &str| {
        simulate_deck(&card(extra), &["V(c)".to_string()])
            .unwrap()
            .notes
    };
    let stated = notes("XTF=3 ITF=.4");
    assert!(
        stated
            .iter()
            .any(|n| n.contains("XTF=3") && n.contains("q2n3904")),
        "{stated:?}"
    );
    // A card restating the SPICE default asks for nothing, so it stays silent,
    // and so does one whose parameters hauksbee now reads (VJE/MJE/FC). What
    // every run states regardless is the basis its numbers rest on.
    let quiet = notes("XTF=0 ITF=0 VJE=0.65 FC=0.4");
    assert_eq!(quiet.len(), 1, "{quiet:?}");
    assert!(
        quiet[0].contains("carry no uncertainty interval"),
        "{quiet:?}"
    );
}
