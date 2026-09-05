## What this changes

<!-- What behaviour is different after this, and why. -->

## Checks

- [ ] `cargo fmt --all`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo test --workspace`

## If this touches a check or a device model

- [ ] Run against the board corpus (`scripts/fetch-corpus.sh`, then
      `HAUKSBEE_REQUIRE_CORPUS=1 cargo test --workspace --features avr`)
- [ ] It stays quiet on boards that are fine, not just loud on the one that is not

## If this can produce a number

- [ ] A result that cannot be trusted says so, rather than rendering anyway
