# Datasheet extraction: running the live test

The datasheet-to-model pipeline (`model-extract` binary) has an offline test
that always runs in CI and a manual live-backend test.

## Offline (default, no codex / no network)

```bash
cargo test -p hauksbee-models
cargo test -p hauksbee-engine --test datasheet_validation
```

The offline path drives the full extractor with a canned reply through
`HAUKSBEE_EXTRACT_MOCK_REPLY`, and the engine fixtures simulate canned models and
assert the datasheet numbers. Neither needs codex.

## Live (manual)

The live test is `#[ignore]`d because it calls the configured Codex or API
backend and can take several minutes per part.

Prerequisites:

- `codex` on `PATH` (default backend), **or** `HAUKSBEE_LLM_API_KEY` set for the
  OpenAI-compatible API backend.
- `pdftotext` on `PATH` (required for the API backend; local agent backends can
  read the copied PDF directly).
- The reference datasheet present at `testdata/datasheets/BC847.pdf`. It is the
  Nexperia BC846 series datasheet (BC847 is in that family). Download:

  ```bash
  mkdir -p testdata/datasheets
  curl -L -o testdata/datasheets/BC847.pdf \
      https://assets.nexperia.com/documents/data-sheet/BC846_SER.pdf
  # 1N4148 reference sheet, if you want to extract it too:
  curl -L -o testdata/datasheets/1N4148.pdf \
      https://assets.nexperia.com/documents/data-sheet/1N4148_1N4448.pdf
  ```

  (If a manufacturer CDN blocks curl with an anti-bot page, fetch with a stealth
  client such as scrapling; the URLs above are public.)

Run the live extraction test:

```bash
cargo test -p hauksbee-models --lib extract_bc847_live -- \
    --ignored --nocapture
```

It runs the configured backend against the BC847 datasheet, parses and validates the reply, and
asserts the extracted `bf` lands in the datasheet hFE band (110..450) with VCEO
in `[models.ratings]`.

### Extract any part by hand

```bash
cargo build -p hauksbee-models --bin model-extract
./target/debug/model-extract \
    --pdf testdata/datasheets/1N4148.pdf --part 1N4148 --kind diode \
    --out-dir testdata/extracted
```

The engine's `extracted_*_physical` tests pick up whatever lands in
`testdata/extracted/` and physically validate it (they self-skip when absent, so
CI stays green without codex).

## Backends

- **codex** (default): `codex exec --sandbox workspace-write
  --skip-git-repo-check --cd <pdf_dir>`. Hard 10-minute timeout per run.
- **API**: set `HAUKSBEE_LLM_API_KEY`, optionally `HAUKSBEE_LLM_MODEL` and
  `HAUKSBEE_LLM_BASE_URL` (defaults to `https://api.openai.com/v1`).
- **Claude Code**: pass `--backend claude-code` when invoking the extractor;
  requires a signed-in `claude` CLI.
- **mock** (tests only): `HAUKSBEE_EXTRACT_MOCK_REPLY=<file>` returns the file's
  contents as the backend reply, still parsed and validated.
