# What a real user should experience

Dana has a hardware repo with a board in `hardware/` and a CI pipeline that
currently checks nothing about it. She wants a gate, and she has not read the
assertion catalogue.

## Getting a spec

```
hauksbee-ci init hardware/blinky.kicad_pcb --out ci
```

The scaffold has to arrive with the board already read: the MCU detected, the
supply rail detected and filled in with its real voltage, and the `board = ...`
path written relative to where the spec landed rather than to her shell's
current directory. Everything optional arrives commented, with a line above it
saying what it does.

`init` also has to tell her where the pre-commit hook and the GitHub action look
for specs, because a spec in the wrong directory is a gate that silently never
runs, and that is the worst outcome available.

## Making it gate

Her entire edit is uncommenting the rail block. That is the promise the scaffold
makes with "Uncomment and tune", so the scenario does exactly that and nothing
else, and the spec has to run green. If the scaffold needed a hand-written line
to work, the comment would be a lie.

## The part that earns its keep

Later, someone renames `+5V` to `+5VDC` in the layout and does not touch the
spec. This is the single most common way a hardware gate rots: the spec still
parses, still names a net, and now checks nothing.

The run must fail at exit 2, and the distinction matters more than the message:

- exit 1 would mean "your board is broken", sending her to debug hardware.
- exit 0 would mean the gate silently stopped gating.
- exit 2 means "your spec no longer describes this board", which is the truth.

The message names both places the spec mentioned the missing net, so one edit
fixes it, and suggests `+5VDC`, which is the net that replaced it. Nothing in
the output says PASS, FAIL, RED, or GREEN, because none of those happened: no
assertion was evaluated at all.

## The directory that does not exist yet

The last two steps run the same first command against the far more common repo:
one with no spec directory at all. `--out` accepts "a directory (gets
`<board-stem>.toml` inside it) or a .toml file path", and the tool's own closing
advice is that the hook and the action discover specs in `ci/`. So `--out
checks` on a repo with no `checks/` has to end with a spec at
`checks/blinky.toml`.

If it instead writes a single file *named* `checks`, the user is left holding a
spec that no discovery path will ever find, one line after being told where
specs get discovered. That is a silent no-op gate, which is the exact failure
this whole scenario exists to prevent, so it is asserted rather than assumed.
