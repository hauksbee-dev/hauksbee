//! `model-extract`: the standalone entry point for datasheet extraction.
//!
//! The work lives in `hauksbee_models::datasheet` so the engine can offer the
//! same capability from `hauksbee models extract`. This binary stays for anyone
//! who already scripts against it.
//!
//! It holds the same consent contract as every other surface, and it has to.
//! Extraction sends the datasheet's text to an LLM backend, and a second
//! entry point that skipped the notice would make the notice decorative:
//! anybody wanting to avoid it would simply call this instead.

fn main() -> anyhow::Result<()> {
    let args = hauksbee_models::datasheet::parse_args()?;

    eprintln!("{}", hauksbee_models::datasheet::CONSENT_NOTICE);
    eprintln!();

    // HAUKSBEE_EXTRACT_YES is the scripted equivalent of `--yes` on the engine
    // command. An env var rather than a flag because this binary's argument
    // parser is hand-rolled and shared with older scripts, and adding a
    // positional-sensitive flag to it would break them.
    let assumed = std::env::var_os("HAUKSBEE_EXTRACT_YES").is_some();
    if !assumed
        && !hauksbee_models::datasheet::confirm(
            &mut std::io::stderr(),
            "Send the datasheet and draft a model?",
            "refusing to send anything without consent. This is not a terminal, so \
             there is nobody to ask: set HAUKSBEE_EXTRACT_YES=1 if you meant it, or \
             use `hauksbee models extract --yes`.",
        )?
    {
        eprintln!("Nothing was sent.");
        return Ok(());
    }

    hauksbee_models::extract_progress::with_spinner(|| hauksbee_models::datasheet::run(args))
        .map(|_| ())
}
