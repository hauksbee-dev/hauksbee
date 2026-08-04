//! Embedded, always-runnable example inputs for `run` and `sim`.
//!
//! The missing-file suggestions promise a runnable command from a bare
//! installed binary (no hauksbee checkout on disk), so the example board and
//! deck are compiled in and materialized to the system temp dir on demand.
//! `hauksbee-ci run --example blinky` is the spec-level sibling (it embeds
//! the same board plus its spec and firmware).

use std::path::PathBuf;

use anyhow::Context;

/// The example boards `run --example` accepts.
const BOARDS: &[(&str, &str, &[u8])] = &[(
    "blinky",
    "blinky.kicad_pcb",
    include_bytes!("../../../hauksbee-ci/examples/boards/blinky.kicad_pcb"),
)];

/// The example decks `sim --example` accepts.
const DECKS: &[(&str, &str, &[u8])] = &[(
    "rlc_ringdown",
    "rlc_ringdown.cir",
    include_bytes!("../../../../examples/decks/rlc_ringdown.cir"),
)];

fn materialize(table: &[(&str, &str, &[u8])], what: &str, name: &str) -> anyhow::Result<PathBuf> {
    let Some((_, file, bytes)) = table.iter().find(|(n, _, _)| *n == name) else {
        anyhow::bail!(
            "no embedded example {what} named '{name}'. Available: {}",
            table
                .iter()
                .map(|(n, _, _)| *n)
                .collect::<Vec<_>>()
                .join(", ")
        );
    };
    let dir = std::env::temp_dir().join(format!("hauksbee-example-{name}"));
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(file);
    std::fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Materialize the named example board and return its path.
pub fn board(name: &str) -> anyhow::Result<PathBuf> {
    materialize(BOARDS, "board", name)
}

/// Materialize the named example SPICE deck and return its path.
pub fn deck(name: &str) -> anyhow::Result<PathBuf> {
    materialize(DECKS, "deck", name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blinky_board_materializes() {
        let p = board("blinky").unwrap();
        assert!(p.exists());
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("kicad_pcb"), "not a kicad board?");
    }

    #[test]
    fn ringdown_deck_materializes() {
        let p = deck("rlc_ringdown").unwrap();
        assert!(std::fs::read_to_string(&p)
            .unwrap()
            .to_lowercase()
            .contains(".tran"));
    }

    #[test]
    fn unknown_names_list_the_catalog() {
        let err = board("nope").unwrap_err().to_string();
        assert!(err.contains("blinky"), "{err}");
        let err = deck("nope").unwrap_err().to_string();
        assert!(err.contains("rlc_ringdown"), "{err}");
    }
}
