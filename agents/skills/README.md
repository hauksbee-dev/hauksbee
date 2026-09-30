# Hauksbee agent skills

Each directory is one skill: a `SKILL.md` with name and description
front matter, plus `agents/openai.yaml` for Codex's skill picker. The skills
drive the same `hauksbee`, `hauksbee-ci`, and `hauksbee-mcp` binaries a
person uses. None of them needs an LLM to reproduce a result.

| Skill | Use it to |
|---|---|
| [`check-a-board`](check-a-board/SKILL.md) | import a board, see what bound, run every static check, and report the verdict and its evidence |
| [`write-a-ci-spec`](write-a-ci-spec/SKILL.md) | scaffold, validate, and run a `hauksbee-ci` spec, with JUnit or JSON output |
| [`debug-a-red-run`](debug-a-red-run/SKILL.md) | classify a RED or INVALID run and pick the next action |
| [`firmware-cosim`](firmware-cosim/SKILL.md) | boot firmware on the emulated MCU and decide whether the result is a real pass |
| [`model-a-hardware-board`](model-a-hardware-board/SKILL.md) | close model-coverage gaps with source-bound device models |

## Install

The skills need `hauksbee` and `hauksbee-ci` on `PATH` (the installer or
`scripts/install.sh` puts them there). Then link the skill directories where
your agent looks for skills. From a checkout:

```bash
# Claude Code, for every project
mkdir -p ~/.claude/skills
for s in agents/skills/*/; do ln -s "$PWD/$s" ~/.claude/skills/; done

# Claude Code, for one hardware repo: use <repo>/.claude/skills/ instead

# Codex
mkdir -p ~/.codex/skills
for s in agents/skills/*/; do ln -s "$PWD/$s" ~/.codex/skills/; done
```

Symlinks keep the skills in step with the checkout. Copy the directories
instead if the checkout will move. Start a new agent session after linking so
the skill list is reloaded.

## Use

Ask for the task in plain words ("check this board before I order it") and
the agent picks the skill from its description, or name it: `$check-a-board`
in Codex, `/check-a-board` in Claude Code.

For MCP tools as well as the CLI, register `hauksbee-mcp` too. See "The MCP
server" in [`../AGENTS.md`](../AGENTS.md).
