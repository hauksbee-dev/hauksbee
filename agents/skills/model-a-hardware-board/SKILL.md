---
name: model-a-hardware-board
description: Turn a PCB or schematic into source-bound Hauksbee models and a reproducible co-simulation. Use for model-coverage gaps, onboarding a new board, extending identity-only or partial device models, attaching power, peripherals, or firmware, reproducing a hardware bug or fix, or preparing an evidence-backed model pack through the CLI, browser, or Hauksbee MCP.
---

# Model a hardware board

Make the ordinary human workflow work first. Use the agent to drive, verify,
or explain the same deterministic surfaces; never create a result that only an
LLM can reproduce.

## Workflow

1. Inspect without changing anything.

   ```bash
   hauksbee models coverage BOARD --json
   hauksbee run BOARD --check --json --strict
   ```

   Or call MCP `model_coverage`. Record the board digest, tool revision, active
   denominator, stage per component, implemented capabilities, missing
   capabilities, source references, uncertainty, and invalid/refused results.
   Do not collapse identified, executable-partial, and declared-complete.

2. Choose the claim before filling the model. Examples: rail startup,
   bus-powered sleep current, firmware deadline, protection trip, or thermal
   limit. Require only capabilities causally needed for that claim.

3. Show the exact write plan and ask for approval.

   ```bash
   hauksbee models prepare BOARD --pack-dir model-packs/BOARD
   ```

   Let the command prompt. Never add `--yes` unless the user already approved
   these exact paths. In the browser, click a coverage row and choose Extend;
   opening the reviewed draft is read-only, and Save is the explicit approval.
   Preserve an existing executable card rather than replacing it with a shell.

4. Bind source evidence. Prefer a manufacturer datasheet and pin/function
   table. Retain URL, locator, and exact SHA-256 when bytes are available.
   Mark identity-only, partial behavior, assumptions, and unknown uncertainty
   explicitly. Never promote guessed values to validated behavior.

5. Implement the smallest reusable behavior, not a board-name exception:
   pins and roles; ratings/static contracts; electrical law/current draw;
   power-state gating; protocol/register behavior; then firmware/peripheral
   coupling. Use shared primitives when possible. Keep unsupported modes in
   `coverage.missing` and fail closed if the requested claim needs them.

6. Validate before installing or saving.

   ```bash
   for model in model-packs/BOARD/models/*.toml; do
     hauksbee models lint "$model" || exit
   done
   hauksbee models coverage BOARD --models-dir model-packs/BOARD/models --json
   hauksbee models coverage BOARD --models-dir model-packs/BOARD/models \
     --require REF:CAPABILITY
   ```

   Test exact identities, hostile near-names, disabled and unpowered states,
   malformed transactions, limits, and reset behavior. Ask before `hauksbee
   models add`, browser Save, or datasheet extraction that invokes an LLM.
   Never install or spend credits implicitly.

7. Prove the result dynamically. In the browser, click traces to add live
   probes or assertions, or attach a real waveform/button/switch to the running
   circuit; the browser queues the same typed `[[peripheral]]` block for replay.
   For an I²C/SPI gap, first check the browser's bundled local behavior picker,
   then use exact local/pasted spec bytes when the device is not present. Set
   physical inputs; explicitly attach it live, wait for the correlated
   engine receipt, then retain the same `[[sensor]]` row for replay. Prefer an exact model-card
   `peripheral.kind = "register_map"` when the address/framing are reusable so
   future boards auto-attach it. Express board-selected personalities with
   `required_high_roles` / `required_low_roles`, and address straps with
   `address_select_role` plus `address_when_low` / `address_when_high`; do not
   freeze one board's strap choice into a global spec. Attach firmware, configure detected power
   rails, and rerun. Never treat a net name as a drivable source unless the
   engine explicitly declares it. On the CLI, create a `hauksbee-ci` spec and
   retain JSON. A fixed revision must remove the relevant signal; negative
   controls must stay quiet. Keep INVALID separate from RED and GREEN.

8. Retain a reproducible handoff: commands, exact input/model/datasheet hashes,
   model stages before and after, required-capability gates, dynamic outputs,
   negative controls, remaining gaps, and the exact Hauksbee revision.

## Product surfaces

- Prefer `hauksbee serve` for a human: component and trace selection, coverage,
  model editor, firmware, supplies, peripherals, probes, assertions, and live
  co-sim share one board view.
- Prefer CLI JSON for scripts and evidence regeneration.
- Prefer MCP `model_coverage`, `analyze_board`, and `run_checks` for agents.
  MCP model coverage is read-only and preparation still requires approval.

Read `docs/models/MODELS.md` for the schema and authoring commands,
`docs/ci/CI.md` for behavioral assertions, and `agents/AGENTS.md` for exit-code
and evidence semantics. If a feature is unavailable in one surface, do not
silently fake it in another; report the exact next step.
