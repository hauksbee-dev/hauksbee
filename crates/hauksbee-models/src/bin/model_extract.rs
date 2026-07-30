//! `model-extract`: the standalone entry point for datasheet extraction.
//!
//! The work lives in `hauksbee_models::datasheet` so the engine can offer the
//! same capability from `hauksbee models extract` and from the web UI. This
//! binary stays for anyone who already scripts against it.

fn main() -> anyhow::Result<()> {
    hauksbee_models::datasheet::run(hauksbee_models::datasheet::parse_args()?)
}
