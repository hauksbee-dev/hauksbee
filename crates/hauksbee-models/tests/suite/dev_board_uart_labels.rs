//! A dev board's header pad that its own silkscreen calls a serial line reads
//! as that line to the checks.
//!
//! A carrier that plugs in a dev board binds its header by label
//! (`[models.header_labels]`). The contention and swapped-line checks read a
//! pad's UART direction from the pad role (`uart_role_direction`), so a card
//! that maps `TX` to a bare `gpio1` left those checks blind to a dev-board UART
//! pin, while the chip's own card (`p01_txd`) was not. The role keeps its GPIO
//! prefix, so the co-sim backend still routes the pin.

use hauksbee_ir::pin_functions::{uart_role_direction, uart_token_direction};
use hauksbee_models::schema::DbFile;

fn dev_board_cards() -> Vec<hauksbee_models::ModelEntry> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("db/mcu_boards.toml");
    let text = std::fs::read_to_string(path).expect("mcu_boards.toml");
    toml::from_str::<DbFile>(&text).expect("parses").models
}

#[test]
fn every_header_label_that_names_a_uart_line_gives_a_role_with_that_direction() {
    let cards = dev_board_cards();
    assert!(cards.len() >= 7, "the seven dev-board cards");
    let mut seen = 0;
    for card in &cards {
        for (label, role) in &card.header_labels {
            let Some(want) = uart_token_direction(label) else {
                continue;
            };
            seen += 1;
            assert_eq!(
                uart_role_direction(role),
                Some(want),
                "{}: header label '{label}' names a UART line but its role '{role}' does not \
                 (name it as the chip card does: `p01_txd`, `gpio43_txd0`)",
                card.id
            );
        }
    }
    assert!(seen >= 14, "every card has TX and RX labels: {seen}");
}

#[test]
fn every_pad_that_a_label_names_a_uart_line_takes_the_same_role_as_the_label() {
    // A pad's number role and its label role are one pin, so they must agree.
    for card in dev_board_cards() {
        for (label, role) in &card.header_labels {
            let bare = label.to_ascii_lowercase();
            for (pad, pad_role) in &card.pins {
                let same_gpio = bare.strip_prefix("gpio").is_some_and(|n| {
                    pad_role
                        .to_ascii_lowercase()
                        .strip_prefix("gpio")
                        .is_some_and(|rest| rest.split('_').next() == Some(n))
                });
                if same_gpio {
                    assert_eq!(
                        uart_role_direction(pad_role),
                        uart_role_direction(role),
                        "{}: pad {pad} '{pad_role}' and label '{label}' '{role}' are one GPIO",
                        card.id
                    );
                }
            }
        }
    }
}

#[test]
fn a_bare_gpio_role_is_no_uart_line_and_the_tagged_roles_are() {
    // The reason the cards tag the role: a bare GPIO has no direction.
    for bare in ["gpio1", "gpio21", "gpio43"] {
        assert_eq!(uart_role_direction(bare), None, "{bare}");
    }
    for (role, tx) in [
        ("p01_txd", true),
        ("p03_rxd", false),
        ("gpio21_txd0", true),
        ("gpio20_rxd0", false),
        ("gpio43_txd0", true),
        ("gpio44_rxd0", false),
        ("gpio17_txd", true),
        ("gpio0_txd", true),
        ("gpio1_rxd", false),
    ] {
        assert_eq!(uart_role_direction(role), Some(tx), "{role}");
    }
}
