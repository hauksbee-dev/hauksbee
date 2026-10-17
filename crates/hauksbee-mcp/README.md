# hauksbee-mcp

A stdio MCP server that exposes hauksbee's engine as structured tools, so an
agent can analyse a PCB, run a check spec against it, and read back a verdict
without ever parsing terminal output.

Transport is stdio: JSON-RPC 2.0, newline-delimited, protocol revision
2025-06-18 (2025-03-26 and 2024-11-05 are also accepted). The server declares
only the `tools` capability.

## The nine tools

| Tool | Does |
|---|---|
| `analyze_board` | Full physics-grounded analysis of a board file, returning the front-door report JSON: headline, `serious`/`total`, per-section findings, `bind` coverage, nets, supplies, notes, and `cosim` when firmware ran. In `cosim`, `timing_coverage` is a measured non-hole bound, `timing_refusals` is strict-invalid, and `fallback_windows` retains second-class method/fidelity/error qualifications. |
| `run_checks` | Runs a `hauksbee-ci` spec against a board, booting optional firmware on the emulated MCU, and returns the machine verdict: `passed`, per-assertion results, `run_valid`, coverage and substitution data. |
| `list_capabilities` | The scope table as data: which report kinds and assertion kinds exist, which board and firmware formats are accepted, and which MCU backends this machine actually has, probed with the engine's own discovery. |
| `model_coverage` | Read-only staged coverage for every connected active device: identity, executable scope, declared completeness, sources, pins/nets, implemented and missing behavior. The returned preparation command is explicitly marked as requiring user approval. |
| `simulate_deck` | Runs a SPICE deck like `hauksbee sim` and returns `{analysis, columns, time, rows, notes}`. A long run is thinned to `max_rows` and says so. A deck the solver cannot answer honestly comes back as the `invalid_for_analysis` refusal. |
| `doctor` | The co-sim backend probe as `hauksbee doctor --json`: `{backends: [{name, status, available, detail, summary}]}`, including the datasheet-extraction backend. |
| `models_lint` | Validates a model TOML file like `hauksbee models lint`: `{clean, checked, findings, warnings, lines}`. Read-only. |
| `run_script` | Code mode: submit one JavaScript program that runs server-side against the same API and returns a composed result, instead of many tool round-trips. |

`model_coverage` is the same shared snapshot used by `hauksbee models coverage`
and the browser's component/trace cards. It does not prepare or install files,
fetch a datasheet, or invoke an LLM.

For bus behavior, agents should prefer an exact model-card
`[models.peripheral] kind = "register_map"` with source-bound spec bytes and
pin roles. The binder then attaches the same generic I²C/SPI interpreter used
by browser and CI scenarios. Direct address or bus-mode straps can be expressed
once with `required_high_roles` / `required_low_roles` and
`address_select_role`; the binder derives their state only from resolved board
supply/ground ties and refuses ambiguity. Use a reviewed `[[sensor]]` scenario
with explicit controller/`cs_net` for wiring that cannot be represented safely.
Neither route needs an LLM; never invent register maps from part names.

Every result arrives both as a `content` text block and as
`structuredContent`, carrying the same object.

A run that cannot be vouched for comes back as `{"status":
"invalid_for_analysis", "reason": ..., "report": ...}` with `isError: false`.
That is an answer, not a malfunction: never read it as pass or fail, and never
retry expecting a different outcome.

## Install

The binary ships in every release bundle as `bin/hauksbee-mcp`, alongside
`hauksbee` and `hauksbee-ci`, and the installer command puts all three on your
`PATH`:

```bash
curl -fsSL https://raw.githubusercontent.com/hauksbee-dev/hauksbee/main/scripts/get-hauksbee.sh | bash
```

To pin the installer code itself, fetch the script at the release's
40-character commit SHA instead of `main` and pass
`--version` with the release tag.

From a checkout, `scripts/install.sh` builds and installs the same three.

## Register it

Claude Code or Codex, one line:

```bash
claude mcp add --transport stdio hauksbee -- hauksbee-mcp
codex mcp add hauksbee -- hauksbee-mcp
```

Any client that reads an `mcpServers` block:

```json
{
  "mcpServers": {
    "hauksbee": {
      "command": "hauksbee-mcp",
      "args": []
    }
  }
}
```

Use an absolute path for `command` if `hauksbee-mcp` is not on the `PATH` the
client inherits. Relative paths in tool arguments resolve against the server's
working directory, so absolute paths are safer.

## Arguments and a worked example

`analyze_board` takes `board_path` and optional `firmware_path` and
`schematic_path`. `run_checks` takes `board_path`, `spec_toml` (the spec
without `board`, `firmware`, or `schematic` keys) and the same optional paths.
`model_coverage` takes `board_path` and optional `models_dir`.
`run_script` takes `source`.
`simulate_deck` takes `deck_path` and optional `print` (probe names),
`analysis` (`op`, `tran`, `ac` or `dc`) and `max_rows`. `models_lint` takes
`file_path`. `list_capabilities` and `doctor` take nothing. The per-tool argument table, the refusal
shape, and a full `run_script` request and response are in
[`agents/AGENTS.md`](../../agents/AGENTS.md#the-mcp-server).

## The full contract

MCP clients discover the current tool schemas through `tools/list`. Treat
refusal results and evidence classifications as part of the response contract;
do not reinterpret an unverified or invalid result as a hardware verdict.
