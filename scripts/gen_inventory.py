#!/usr/bin/env python3
"""One-shot generator for docs/teach/inventory.toml (W8 doc-coverage lint).

Walks crates/*/src, looks up each module's priority from the explicit
classification map below, and writes the inventory TOML. Errors loudly if a
walked module has no classification (forces a decision) or a map entry names a
file that does not exist (catches typos). The generated TOML is the checked-in
artifact; this generator is a scaffolding aid, not part of the lint.
"""
import os
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

# priority by repo-root-relative path (forward slashes). Rationale for NEW
# modules (not in the 2026-07-01 plan §1.3 inventory) is in the report.
PRI = {
    # ── hauksbee-ir (crate P1) ──────────────────────────────────────────────
    "crates/hauksbee-ir/src/bexpr.rs": "P1",   # NEW: behavioral-source expr AST/eval, stamp-facing
    "crates/hauksbee-ir/src/lib.rs": "P1",
    "crates/hauksbee-ir/src/models.rs": "P1",
    "crates/hauksbee-ir/src/source.rs": "P2",
    "crates/hauksbee-ir/src/spice.rs": "P1",
    # ── hauksbee-solve (the numerical heart) ────────────────────────────────
    "crates/hauksbee-solve/src/ac.rs": "P1",
    "crates/hauksbee-solve/src/alloc_audit.rs": "P2",   # NEW: alloc-hygiene test gate, off hot path
    "crates/hauksbee-solve/src/census.rs": "P2",        # NEW: step census diagnostic
    "crates/hauksbee-solve/src/cmatrix.rs": "P2",
    "crates/hauksbee-solve/src/decompose/conduction.rs": "P0",
    "crates/hauksbee-solve/src/decompose/drivers.rs": "P1",
    "crates/hauksbee-solve/src/decompose/feedforward.rs": "P0",
    "crates/hauksbee-solve/src/decompose/mod.rs": "P0",
    "crates/hauksbee-solve/src/decompose/rails.rs": "P1",
    "crates/hauksbee-solve/src/decompose/stiff.rs": "P1",   # NEW: stiff-node tear detection, core tear logic
    "crates/hauksbee-solve/src/decompose/verify.rs": "P0",
    "crates/hauksbee-solve/src/diagnostics.rs": "P2",   # NEW: which ladder strategies fired
    "crates/hauksbee-solve/src/lib.rs": "P0",
    "crates/hauksbee-solve/src/linear.rs": "P0",
    "crates/hauksbee-solve/src/loop_stability.rs": "P1",
    "crates/hauksbee-solve/src/newton.rs": "P0",
    "crates/hauksbee-solve/src/options.rs": "P2",
    "crates/hauksbee-solve/src/orchestrate/balance.rs": "P1",
    "crates/hauksbee-solve/src/orchestrate/capture.rs": "P1",   # NEW: stiff-tear capture/replay executor
    "crates/hauksbee-solve/src/orchestrate/mod.rs": "P1",       # NEW: orchestration layer root
    "crates/hauksbee-solve/src/orchestrate/staged.rs": "P1",
    "crates/hauksbee-solve/src/partition.rs": "P0",
    "crates/hauksbee-solve/src/partitioned.rs": "P1",
    "crates/hauksbee-solve/src/plan.rs": "P1",
    "crates/hauksbee-solve/src/rawfile.rs": "P2",       # NEW: ngspice ASCII rawfile writer (I/O)
    "crates/hauksbee-solve/src/sim.rs": "P2",           # NEW: deck-to-results CLI/harness glue
    "crates/hauksbee-solve/src/sparse.rs": "P0",
    "crates/hauksbee-solve/src/stamp.rs": "P0",
    "crates/hauksbee-solve/src/system.rs": "P0",
    "crates/hauksbee-solve/src/transient.rs": "P0",
    # ── hauksbee-extract (copper -> netlist) ────────────────────────────────
    "crates/hauksbee-extract/src/altium.rs": "P2",
    "crates/hauksbee-extract/src/drc.rs": "P2",
    "crates/hauksbee-extract/src/eagle.rs": "P2",
    "crates/hauksbee-extract/src/gerber/connect.rs": "P1",
    "crates/hauksbee-extract/src/gerber/excellon.rs": "P1",
    "crates/hauksbee-extract/src/gerber/geo.rs": "P2",       # NEW: geometry primitives, supporting math
    "crates/hauksbee-extract/src/gerber/layers.rs": "P2",    # NEW: layer-role inference from filenames
    "crates/hauksbee-extract/src/gerber/macros.rs": "P2",    # NEW: aperture-macro expansion, parser detail
    "crates/hauksbee-extract/src/gerber/mod.rs": "P1",
    "crates/hauksbee-extract/src/gerber/placement.rs": "P2", # NEW: pick-and-place/BOM CSV readers
    "crates/hauksbee-extract/src/gerber/rs274x.rs": "P1",
    "crates/hauksbee-extract/src/ipc356.rs": "P2",
    "crates/hauksbee-extract/src/lib.rs": "P0",
    "crates/hauksbee-extract/src/netlint.rs": "P2",
    "crates/hauksbee-extract/src/netlist.rs": "P0",
    "crates/hauksbee-extract/src/pcb.rs": "P1",
    "crates/hauksbee-extract/src/reader.rs": "P1",           # NEW: board-format reader registry / front-end extension point
    "crates/hauksbee-extract/src/resource_conflict.rs": "P2",
    "crates/hauksbee-extract/src/schematic.rs": "P1",
    "crates/hauksbee-extract/src/si.rs": "P1",
    "crates/hauksbee-extract/src/si/impedance.rs": "P1",
    "crates/hauksbee-extract/src/trace_current.rs": "P2",
    # ── hauksbee-models (declarative models, crate P1/P2) ───────────────────
    "crates/hauksbee-models/src/behavioral.rs": "P1",   # NEW: declarative behavioural model layer (extension surface)
    "crates/hauksbee-models/src/lib.rs": "P1",
    "crates/hauksbee-models/src/logic_spec.rs": "P1",   # NEW: declarative logic-IC spec (logic as data)
    "crates/hauksbee-models/src/matcher.rs": "P1",
    "crates/hauksbee-models/src/pack.rs": "P1",         # NEW: versioned model packs (distribution surface)
    "crates/hauksbee-models/src/pin_rules.rs": "P1",    # NEW: declarative pin-role inference rules
    "crates/hauksbee-models/src/profile.rs": "P1",
    "crates/hauksbee-models/src/schema.rs": "P1",
    "crates/hauksbee-models/src/sensor_spec.rs": "P1",
    "crates/hauksbee-models/src/spice_input.rs": "P2",
    "crates/hauksbee-models/src/validation.rs": "P2",
    "crates/hauksbee-models/src/value.rs": "P2",
    # ── hauksbee-mcu (firmware co-sim, crate P0 for the chapter) ────────────
    "crates/hauksbee-mcu/src/avr.rs": "P1",
    "crates/hauksbee-mcu/src/elf.rs": "P2",
    "crates/hauksbee-mcu/src/lib.rs": "P0",
    "crates/hauksbee-mcu/src/qemu/gdb.rs": "P1",
    "crates/hauksbee-mcu/src/qemu/mod.rs": "P1",
    "crates/hauksbee-mcu/src/qemu/process.rs": "P1",
    "crates/hauksbee-mcu/src/qemu/qmp.rs": "P1",
    "crates/hauksbee-mcu/src/qemu/uart.rs": "P1",
    "crates/hauksbee-mcu/src/renode/mod.rs": "P1",
    "crates/hauksbee-mcu/src/renode/monitor.rs": "P1",
    "crates/hauksbee-mcu/src/renode/process.rs": "P1",
    "crates/hauksbee-mcu/src/renode/uart.rs": "P1",
    "crates/hauksbee-mcu/src/soc.rs": "P1",             # NEW: data-driven MCU/SoC descriptors (add-an-MCU-as-data surface)
    "crates/hauksbee-mcu/src/traits.rs": "P0",
    # ── hauksbee-server (crate P2) ──────────────────────────────────────────
    "crates/hauksbee-server/src/engine.rs": "P2",
    "crates/hauksbee-server/src/frontdoor.rs": "P2",
    "crates/hauksbee-server/src/lib.rs": "P2",
    "crates/hauksbee-server/src/main.rs": "P2",         # binary entry point
    "crates/hauksbee-server/src/protocol.rs": "P2",
    # ── hauksbee-engine (orchestration/checks/co-sim, mixed) ────────────────
    "crates/hauksbee-engine/src/behavioral.rs": "P1",
    "crates/hauksbee-engine/src/binder.rs": "P0",
    "crates/hauksbee-engine/src/boardcode.rs": "P2",
    "crates/hauksbee-engine/src/checks/ampacity.rs": "P1",       # NEW: ampacity check (calibrated suite)
    "crates/hauksbee-engine/src/checks/boot.rs": "P1",           # NEW: boot check
    "crates/hauksbee-engine/src/checks/converter.rs": "P1",      # NEW: DC-DC converter check
    "crates/hauksbee-engine/src/checks/device_decode.rs": "P1",  # NEW: device-decode check
    "crates/hauksbee-engine/src/checks/mcu_coverage.rs": "P1",
    "crates/hauksbee-engine/src/checks/mod.rs": "P1",
    "crates/hauksbee-engine/src/checks/ripple.rs": "P1",         # NEW: ripple check
    "crates/hauksbee-engine/src/checks/straps.rs": "P1",
    "crates/hauksbee-engine/src/checks/usb_c.rs": "P1",
    "crates/hauksbee-engine/src/commands/boardcode.rs": "P2",    # NEW: CLI subcommand glue
    "crates/hauksbee-engine/src/commands/common.rs": "P2",       # NEW
    "crates/hauksbee-engine/src/commands/doctor.rs": "P2",       # NEW
    "crates/hauksbee-engine/src/commands/mod.rs": "P2",          # NEW
    "crates/hauksbee-engine/src/commands/models.rs": "P2",       # NEW
    "crates/hauksbee-engine/src/commands/run.rs": "P2",          # NEW
    "crates/hauksbee-engine/src/commands/serve.rs": "P2",        # NEW
    "crates/hauksbee-engine/src/commands/sim.rs": "P2",          # NEW
    "crates/hauksbee-engine/src/commands/watch.rs": "P2",        # NEW: watch-mode re-sim loop (UX glue)
    "crates/hauksbee-engine/src/decoupling.rs": "P1",
    "crates/hauksbee-engine/src/digital.rs": "P1",
    "crates/hauksbee-engine/src/drivers.rs": "P1",
    "crates/hauksbee-engine/src/engine.rs": "P1",       # NEW-ish: HauksbeeEngine, concrete engine behind server protocol
    "crates/hauksbee-engine/src/frontdoor.rs": "P2",
    "crates/hauksbee-engine/src/lib.rs": "P0",
    "crates/hauksbee-engine/src/logic.rs": "P1",        # NEW: declarative digital-logic evaluator (co-sim)
    "crates/hauksbee-engine/src/main.rs": "P2",         # binary entry point
    "crates/hauksbee-engine/src/peripherals/controls.rs": "P1",
    "crates/hauksbee-engine/src/peripherals/i2c.rs": "P1",
    "crates/hauksbee-engine/src/peripherals/load.rs": "P1",
    "crates/hauksbee-engine/src/peripherals/mod.rs": "P1",
    "crates/hauksbee-engine/src/peripherals/register_map.rs": "P1",
    "crates/hauksbee-engine/src/peripherals/sink.rs": "P1",
    "crates/hauksbee-engine/src/peripherals/spi.rs": "P1",
    "crates/hauksbee-engine/src/plain.rs": "P2",
    "crates/hauksbee-engine/src/power_supply.rs": "P1",
    "crates/hauksbee-engine/src/report.rs": "P2",
    "crates/hauksbee-engine/src/reports/ac.rs": "P2",       # NEW: per-analysis report formatting (presentation)
    "crates/hauksbee-engine/src/reports/ampacity.rs": "P2", # NEW
    "crates/hauksbee-engine/src/reports/bind.rs": "P2",     # NEW
    "crates/hauksbee-engine/src/reports/check.rs": "P2",    # NEW
    "crates/hauksbee-engine/src/reports/cosim.rs": "P2",    # NEW
    "crates/hauksbee-engine/src/reports/drc.rs": "P2",      # NEW
    "crates/hauksbee-engine/src/reports/lint.rs": "P2",     # NEW
    "crates/hauksbee-engine/src/reports/mod.rs": "P2",      # NEW
    "crates/hauksbee-engine/src/reports/si.rs": "P2",       # NEW
    "crates/hauksbee-engine/src/reports/thermal.rs": "P2",  # NEW
    "crates/hauksbee-engine/src/reports/usb_c.rs": "P2",    # NEW
    "crates/hauksbee-engine/src/responders.rs": "P1",       # NEW: synchronous MCU input-responder registry (co-sim)
    "crates/hauksbee-engine/src/result.rs": "P2",
    "crates/hauksbee-engine/src/scheduler.rs": "P1",
    "crates/hauksbee-engine/src/shorts.rs": "P1",
    "crates/hauksbee-engine/src/stress.rs": "P1",
    "crates/hauksbee-engine/src/tarski_decomp.rs": "P1",    # NEW: engine driver for the flagship tear (theory lives in solve/decompose P0)
    "crates/hauksbee-engine/src/tarski_prep.rs": "P1",      # NEW: Tarski board preparation
    "crates/hauksbee-engine/src/thermal.rs": "P1",
    "crates/hauksbee-engine/src/tui/app.rs": "P2",
    "crates/hauksbee-engine/src/tui/cosim.rs": "P2",
    "crates/hauksbee-engine/src/tui/mod.rs": "P2",
    "crates/hauksbee-engine/src/tui/render.rs": "P2",
    "crates/hauksbee-engine/src/tui/state.rs": "P2",
    # ── hauksbee-ci (hardware-CI, crate P1/P2) ──────────────────────────────
    "crates/hauksbee-ci/src/assertions.rs": "P2",
    "crates/hauksbee-ci/src/error.rs": "P2",           # NEW
    "crates/hauksbee-ci/src/init.rs": "P2",            # NEW
    "crates/hauksbee-ci/src/lib.rs": "P1",
    "crates/hauksbee-ci/src/main.rs": "P2",            # binary entry point
    "crates/hauksbee-ci/src/report.rs": "P2",
    "crates/hauksbee-ci/src/runner.rs": "P2",
    "crates/hauksbee-ci/src/scenarios.rs": "P2",
    "crates/hauksbee-ci/src/spec.rs": "P2",
}


def is_excluded(relpath):
    parts = relpath.split("/")
    if "bin" in parts:
        return True
    if "tests" in parts or parts[-1] == "tests.rs":
        return True
    return False


def walk():
    found = []
    crates_dir = os.path.join(ROOT, "crates")
    for crate in sorted(os.listdir(crates_dir)):
        src = os.path.join(crates_dir, crate, "src")
        if not os.path.isdir(src):
            continue
        for dirpath, _dirs, files in os.walk(src):
            for fn in files:
                if not fn.endswith(".rs"):
                    continue
                full = os.path.join(dirpath, fn)
                rel = os.path.relpath(full, ROOT).replace(os.sep, "/")
                if is_excluded(rel):
                    continue
                found.append(rel)
    return sorted(found)


def main():
    found = walk()
    found_set = set(found)
    unclassified = [f for f in found if f not in PRI]
    stale = [p for p in PRI if p not in found_set]
    if unclassified:
        print("ERROR: walked modules with no classification (add a row):", file=sys.stderr)
        for f in unclassified:
            print("  " + f, file=sys.stderr)
    if stale:
        print("ERROR: classification entries for files that do not exist:", file=sys.stderr)
        for f in stale:
            print("  " + f, file=sys.stderr)
    if unclassified or stale:
        sys.exit(1)

    lines = []
    lines.append("# Doc-coverage inventory (W8 teachability, docs/dev-plans/09-teachability.md §1.3/§6.2).")
    lines.append("#")
    lines.append("# Every non-test, non-bin module under crates/*/src MUST have a row here.")
    lines.append("# A new src module with no row FAILS the lint (scripts/lint-doc-coverage.sh /")
    lines.append("# `cargo test -p hauksbee-ci --test doc_coverage`) -- that failure is the forcing")
    lines.append("# function: adding code forces a priority decision. Paths are repo-root-relative.")
    lines.append("#")
    lines.append("# Priority (from the plan's own logic): P0 = pedagogical spine / solver hot path /")
    lines.append("# front-door public surface; P1 = substantial subsystem or public extension surface a")
    lines.append("# contributor will touch; P2 = supporting / glue / presentation / entry-point.")
    lines.append("# P0 and P1 modules FAIL the lint without a substantive module //! doc comment;")
    lines.append("# P2 modules only WARN (ramp-up mode). This file is generated by scripts/gen_inventory.py.")
    lines.append("")
    for f in found:
        lines.append("[[module]]")
        lines.append('path = "%s"' % f)
        lines.append('pri = "%s"' % PRI[f])
        lines.append("")

    out = os.path.join(ROOT, "docs", "teach", "inventory.toml")
    os.makedirs(os.path.dirname(out), exist_ok=True)
    with open(out, "w") as fh:
        fh.write("\n".join(lines).rstrip() + "\n")
    print("wrote %s with %d module rows" % (out, len(found)))


if __name__ == "__main__":
    main()
