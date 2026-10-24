# KiCad oracle: pads that share a number inside one footprint

What `unrouted_pad` relies on in `drc/connectivity.rs` (`join_footprint_links`) and
`layout_lint.rs::shell_tabs_are_open_exactly_where_kicad_reports_them`.

## Method

`kicad-cli pcb drc --format json --severity-all` (KiCad 10.0.6) on a synthetic board:
one footprint with four pads, all numbered `S1`, all on net `SHELL`; one pad routed
by a track to a 0 Ω link to ground; the other three with no track. The board text is
the one in `shell_board()` in `tests/layout_lint.rs`, with uuids added so KiCad loads it.
Only the footprint flag changes between runs.

## Result

| Footprint flag | `unconnected_items` | Reading |
|---|---|---|
| `(duplicate_pad_numbers_are_jumpers yes)` | 0 | same-numbered pads are one connection |
| `(duplicate_pad_numbers_are_jumpers no)` | 3 (pad/pad, pad/pad, track/pad) | each pad is its own copper island |
| flag absent, file version 20260206 | 3 | as `no` |
| flag absent, file versions 20241229 and 20221018 | 3 | as `no`: KiCad 10 does not read an old file as jumpers |

The same three results hold for plated slot pads (`thru_hole oval`), for pads carrying a
`pinfunction`, and for pads with `pad_prop_mechanical` or `pad_prop_heatsink`: no pad
property or pin function exempts a pad from the unconnected check.

So `no` is a real statement, not KiCad's boilerplate default to read past: KiCad 9 and
later write it on every footprint, and a board with it set reports the unrouted tabs.
The footprint setting (Footprint Properties, "Pads with the same number are jumpers")
is what joins them.

## What hauksbee does

* `yes`: joined, as KiCad.
* `no`: open, as KiCad. The finding names the setting that changes it.
* flag absent: joined. This is the one place hauksbee is quieter than KiCad 10 (which
  reports the pads open); it stays so, because a pre-KiCad-9 board has no way to say
  otherwise and treating every old connector shell as open would add alarms the
  designer cannot have meant.
