# Asserts from observations: prior art

A user who sees an output (a scope trace, a UART line, a pin toggling) can turn
"this is what it does" or "this is what I expected" into an assertion in one
click (the Live Sim's "Assert this" / "Assert what I expected") or one CLI line
(`hauksbee-ci add`, `hauksbee-ci init --firmware .. --suggest-asserts`). Before
building it we looked at the existing tools that do something similar. This
records what was adopted, borrowed and rejected, and why.

| Tool | Licence | Decision | What we took or why not |
|---|---|---|---|
| Wokwi CI scenarios | vendor terms (not vendored) | borrow vocabulary, reject the rest | `wait-serial` is our `uart contains`; `expect-pin` is our `voltage`/`toggle`. Rejected: ordered YAML step lists (our spec is a set of declarative assertions judged over one run, not a script) and the cloud-run CLI. |
| sigrok (`sigrok-cli`, protocol decoders, `wait()` conditions) | GPL (libsigrok), Python decoders | borrow ideas, reject vendoring | Edge and level vocabulary for conditions; decoded annotations ("I2C address-write 0x76 ACK") are the right thing to offer as `bus` assertions, which is the shape of the `bus` kind. Rejected: vendoring, because the code is GPL and Python and our observations come from the engine's own bench record. |
| cocotb | BSD-3 | borrow design, reject vendoring | Count-based against time-based checks (our `min_toggles` against `freq_hz`; a trace too short to time a rate is proposed as a count and says so) and timeout-bounded waits (our `reaches ... deadline_ms`). Rejected: a Python runtime and imperative coroutines, where our specs are declarative TOML that a loader validates and an editor can lint. |

No candidate fitted as a dependency, so the kinds, the scaffold writer and the
frontend entry points are written here. This is the product's own differentiator
(an observed value, filled in from the run, with the margin the engine itself
uses), and none of the tools above can see a board.

Not independently verified: the cocotb documentation page we tried to cite
returned 404 when fetched; the cocotb entry rests on the project's well-known
design, not a re-read of its docs.

## Where the vocabulary lives

Kind names, labels, help text and per-field defaults (`default_from`) are
defined once in `crates/hauksbee-ci/src/catalog.rs` and routed from there to
`hauksbee-ci explain`, the app's catalogue (`/api/ci/catalog`), the CLI's
`add`, and `docs/ci/CI.md`. A test runs every kind's example through the spec
validator.
