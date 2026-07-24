# Development probes, not sample projects

These are cargo *examples* in the mechanical sense only: standalone binaries
run with `cargo run --release -p hauksbee-engine --example <name>`. They are
the lab instruments used while developing hauksbee (corpus sweeps, solver
diagnostics, the Tarski flagship investigation), kept because they document
how conclusions were reached and stay compiling as the engine moves.

Looking for how to *use* hauksbee? That is [`docs/START_HERE.md`](../../../docs/START_HERE.md)
and the runnable examples index in [`docs/ci/EXAMPLES.md`](../../../docs/ci/EXAMPLES.md);
the CI specs and boards live in [`crates/hauksbee-ci/examples/`](../../hauksbee-ci/examples/).

| Probe | What it measures |
|---|---|
| `bind_sweep` | Every board in the external corpus through extract → bind, tabulated: what hauksbee makes of boards it has never seen. |
| `bug_hunt`, `cc_classify` | Bug-hunt campaign instruments (codegen anomaly dump, USB-C CC classification sweep). |
| `tarski_*` | Flagship-board investigation probes: rail checks, DC operating points, feedforward/oscillation diagnostics, miswire repair experiments. |
| `_probe` | Scratch harness for one-off solver questions; contents change per investigation. |
