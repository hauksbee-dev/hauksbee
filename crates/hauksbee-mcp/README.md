# hauksbee-mcp

A stdio MCP server that exposes hauksbee's engine as structured tools, so an
agent can analyse a PCB, run a check spec against it, and read back a verdict
without ever parsing terminal output.

Transport is stdio: JSON-RPC 2.0, newline-delimited, protocol revision
2025-06-18 (2025-03-26 and 2024-11-05 are also accepted). The server declares
only the `tools` capability.

## The five tools

| Tool | Does |
|---|---|
| `analyze_board` | Full physics-grounded analysis of a board file, returning the front-door report JSON: headline, `serious`/`total`, per-section findings, `bind` coverage, nets, supplies, notes, and `cosim` when firmware ran. In `cosim`, `timing_coverage` is a measured non-hole bound, `timing_refusals` is strict-invalid, and `fallback_windows` retains second-class method/fidelity/error qualifications. |
| `run_checks` | Runs a `hauksbee-ci` spec against a board, booting optional firmware on the emulated MCU, and returns the machine verdict: `passed`, per-assertion results, `run_valid`, coverage and substitution data. |
| `list_capabilities` | The scope table as data: which report kinds and assertion kinds exist, which board and firmware formats are accepted, and which MCU backends this machine actually has, probed with the engine's own discovery. |
| `board_to_code` | Decompiles a text-format board (KiCad `.kicad_pcb`, Eagle `.brd`) into the editable Board-as-Code text form. |
| `run_script` | Code mode: submit one JavaScript program that runs server-side against the same API and returns a composed result, instead of many tool round-trips. |

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
(
  set -o pipefail
  set +x
  export HAUKSBEE_GITHUB_TOKEN="$(gh auth token)"
  export HAUKSBEE_INSTALLER_COMMIT=REPLACE_WITH_RELEASE_COMMIT_SHA
  export HAUKSBEE_INSTALLER_VERSION=REPLACE_WITH_RELEASE_TAG
  printf 'header = "Authorization: Bearer %s"\n' "$HAUKSBEE_GITHUB_TOKEN" \
    | curl -q --config - -fsSL "https://api.github.com/repos/hauksbee-dev/hauksbee/contents/scripts/get-hauksbee.sh?ref=$HAUKSBEE_INSTALLER_COMMIT" \
    | python3 -c 'import base64,json,sys; sys.stdout.write(base64.b64decode(json.load(sys.stdin)["content"]).decode())' \
    | bash -s -- --version "$HAUKSBEE_INSTALLER_VERSION"
)
```

The token must have `Contents: read` access to the private repository. Passing
the header through curl's stdin keeps the credential out of the process argv;
the subshell removes it afterward. Replace the commit placeholder with the
40-character SHA printed by the release so installer code is immutable too.

From a checkout, `scripts/install.sh` builds and installs the same three.

## Register it

Claude Code, one line:

```bash
claude mcp add --transport stdio hauksbee -- hauksbee-mcp
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
client inherits.

## The full contract

[`agents/AGENTS.md`](../../agents/AGENTS.md) is the agent-facing contract: the
complete tool schemas, the refusal shape, the code-mode sandbox and its limits,
a worked request/response pair, and the ground rules for reading hauksbee's
output. Read it before writing an agent against this server.
