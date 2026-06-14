# Hauksbee Board DSL - VS Code Extension

Syntax highlighting for hauksbee Board-as-Code `.board` files (hauksbee board DSL v1), the human- and AI-editable circuit board description language used by the Galvani/hauksbee toolchain.

## What it highlights

- **Comments** (`#` line comments, including inline comments after code)
- **Block definitions** (`fn <name> { ... }`) - block name shown as an entity/function
- **Block instances** (`instance <block_name> { ... }`)
- **Component placement** (`comp <RefDes> lib "..." val "..." layer "..." at X Y rot R { ... }`) - reference designator distinguished from keywords
- **Slot templates** (`slot N lib "..." val "..." pads N`)
- **Pad declarations** (`pad "N" smd roundrect at X Y size W H layers [F.Cu ...] net "..."`)
- **Net declarations** (`net "name"`)
- **Board directives** (`board version`, `board outline`, `board size`)
- **Placement constraints** (`pin <ref> edge left|right|top|bottom`, `lock <ref>`)
- **Spacing hints** (`space fn <block> <dist>`, `space <dist>` inside comp)
- **Quoted strings** (footprint lib IDs, values, net names, layer strings)
- **Numbers** (coordinates, rotations, counts - including negative floats)
- **Layer bracket lists** (`[F.Cu F.Mask F.Paste]`)
- **Pad kinds** (`smd`, `thru_hole`, `np_thru_hole`, `connect`) and shapes (`rect`, `roundrect`, `oval`, etc.)

## Keyword inventory (confirmed from the parser)

Top-level structural: `board`, `fn`, `instance`, `comp`, `slot`, `net`, `pad`, `space`, `pin`, `lock`

Board sub-directives: `version`, `outline`, `size`

Inline property keys: `lib`, `val`, `pads`, `layer`, `at`, `rot`, `size`, `drill`, `layers`, `nonet`, `edge`

Placement values: `left`, `right`, `top`, `bottom`

Pad kinds: `smd`, `thru_hole`, `np_thru_hole`, `connect`

Pad shapes: `rect`, `roundrect`, `oval`, `circle`, `trapezoid`, `custom`

## Installation

### Option 1 - package and install with vsce (recommended for development)

```sh
npm install -g vsce        # or: bun add -g vsce
cd editors/vscode-hauksbee-board
vsce package
code --install-extension hauksbee-board-0.1.0.vsix
```

### Option 2 - copy directly into VS Code extensions folder (quickest, no build step)

```sh
cp -r editors/vscode-hauksbee-board \
    ~/.vscode/extensions/tarski.hauksbee-board-0.1.0
```

Then restart VS Code (or run "Developer: Reload Window").

### Option 3 - development/symlink (live edits)

```sh
ln -s "$(pwd)/editors/vscode-hauksbee-board" \
    ~/.vscode/extensions/tarski.hauksbee-board-dev
```

Reload VS Code after making grammar changes.

## Language features

- Line comment toggle with `#` (Ctrl+/)
- Auto-close for `{}`, `[]`, and `""`
- Code folding on `{ ... }` blocks
- `#region` / `#endregion` markers for manual folding sections

## File format

`.board` files are produced by `galvani decompile` and consumed by `galvani check` / `galvani build`. They describe a PCB as executable code: function blocks for repeated clusters, explicit component placement with pad-level net connectivity, and board outline/constraint statements.
