# Datasheet extraction: running the live test

The datasheet-to-model pipeline (`model-extract` binary) has an offline test
that always runs in CI, and a live test that shells out to real codex.

## Offline (default, no codex / no network)

```bash
cargo test -p galvani-models
cargo test -p galvani-engine --test datasheet_validation
```

The offline path drives the full extractor with a canned reply through
`GALVANI_EXTRACT_MOCK_REPLY`, and the engine fixtures simulate canned models and
assert the datasheet numbers. Neither needs codex.

## Live (manual, runs real codex)

The live test is `#[ignore]`d because it shells out to `codex exec` and takes
about one to two minutes per part.

Prerequisites:

- `codex` on `PATH` (default backend), **or** `GALVANI_LLM_API_KEY` set for the
  OpenAI-compatible API backend.
- `pdftotext` on `PATH` (optional; improves extraction, otherwise codex reads
  the PDF directly).
- The reference datasheet present at `testdata/datasheets/BC847.pdf`. It is the
  Nexperia BC846 series datasheet (BC847 is in that family). Download:

  ```bash
  mkdir -p testdata/datasheets
  curl -L -o testdata/datasheets/BC847.pdf \
      https://assets.nexperia.com/documents/data-sheet/BC846_SER.pdf
  # 1N4148 and AMS1117 reference sheets, if you want to extract those too:
  curl -L -o testdata/datasheets/1N4148.pdf \
      https://assets.nexperia.com/documents/data-sheet/1N4148_1N4448.pdf
  curl -L -o testdata/datasheets/AMS1117.pdf \
      http://www.advanced-monolithic.com/pdf/ds1117.pdf
  ```

  (If a manufacturer CDN blocks curl with an anti-bot page, fetch with a stealth
  client such as scrapling; the URLs above are public.)

Run the live extraction test:

```bash
cargo test -p galvani-models --bin model-extract -- \
    extract_bc847_live --ignored --nocapture
```

It runs codex against the BC847 datasheet, parses and validates the reply, and
asserts the extracted `bf` lands in the datasheet hFE band (110..450) with VCEO
in `[models.ratings]`.

### Extract any part by hand

```bash
cargo build -p galvani-models --bin model-extract
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
- **API**: set `GALVANI_LLM_API_KEY`, optionally `GALVANI_LLM_MODEL` and
  `GALVANI_LLM_BASE_URL` (defaults to `https://api.openai.com/v1`).
- **mock** (tests only): `GALVANI_EXTRACT_MOCK_REPLY=<file>` returns the file's
  contents as the backend reply, still parsed and validated.
