# The board corpus: what it is, and which gate reads which part of it

`corpus.toml` pins 53 upstream boards and `scripts/fetch-corpus.sh` materialises
them. This records what a clean fetch lands, measured rather than asserted, and
which gate reads each entry, because an entry no gate reads is a board that costs
bandwidth and buys nothing.

Everything below was measured on a fetch into an empty directory on 2026-08-14, on
macOS, with the manifest at the revision this file ships with. Re-measure rather
than adjust arithmetic: `scripts/fetch-corpus.sh --dir <empty>` prints the totals,
and `python3 scripts/check-corpus.py --dir <that>` confirms the manifest still
describes it.

## What a clean fetch lands

| | |
| --- | --- |
| Manifest entries | 53 |
| Fetched by default | 50 |
| Skipped, licence not established | 3 |
| Board directories on disk | 50 |
| Layout files (`.kicad_pcb`, `.brd`, `.PcbDoc`) | 305 |
| Schematic files (`.kicad_sch`, `.sch`, `.SchDoc`) | 514 |
| Netlists (`.net`) | 41 |
| Gerber film files | 615 |
| Total size | 541 MB |

The three skipped entries are the two ClockworkPi uConsole gerber sets and the
SparkFun MicroMod nRF52840. `--include-unconfirmed` fetches them.

## Formats, measured from the files

Read out of the files themselves rather than from the vendor's name for them: the
`(version ...)` token on a KiCad file, the `<eagle version=...>` attribute on an
Eagle XML file, the OLE2 signature on an Altium one.

| Format | Files |
| --- | --- |
| KiCad, file version 4 and 2016-2017 (KiCad 4 and 5 era) | 55 layouts |
| KiCad, 2021 (KiCad 6) | 8 layouts, 23 schematics |
| KiCad, 2022-2023 (KiCad 6 and 7) | 47 layouts, 80 schematics |
| KiCad, 2023-2024 (KiCad 7 and 8) | 14 layouts, 45 schematics |
| KiCad, 2024-2025 (KiCad 8 and 9) | 24 layouts, 77 schematics |
| KiCad, 2026 (KiCad 10) | 1 layout, 1 schematic |
| KiCad legacy `.sch` | 131 schematics |
| Eagle XML, 6.4 through 9.6.2 | 22 layouts, 18 schematics |
| Eagle binary, pre-6 | 35 layouts, 35 schematics |
| Altium OLE2 | 3 layouts, 11 schematics |

The one KiCad 10 board is Raspberry Pi's RP2040 minimal R3 package, and it is the
only place the newest-format reader meets a real file. The Eagle binaries are the
Mutable Instruments modules, and they are there to be REFUSED: `docs/ingest/EAGLE.md`
says hauksbee does not read pre-Eagle-6 binary `.brd`, and 35 real files hold that
statement to its word. They are not counted as board coverage anywhere.

## Which gate reads what

Two kinds of gate. A **sweep** walks the corpus and covers every entry it can
parse; a **named gate** asks for one board by path and covers only that.

| Gate | Reads | Boards covered |
| --- | --- | --- |
| `drc_corpus::corpus_boards_have_no_true_shorts` | sweep, layouts | 116 |
| `placeholder_lint_corpus` | sweep, four extraction paths | 328 known-good board files |
| `models::corpus_coverage_ratchet` | sweep, layouts | bind rate over the whole set |
| `si_ampacity_ripple::famous_corpus_has_no_ampacity_or_ripple_findings` | sweep, layouts | whole known-good set |
| `altium_corpus::fetched_altium_boards_extract_and_are_short_clean` | sweep, `.PcbDoc` | 3 |
| `erc_contention_corpus` | named | 14 schematics |
| `subsheet_hierarchy` | named, MNT Reform | 33 |
| `known_faults` | named, ZSWatch DevKit and mainboard | 3 |
| `trace_current_corpus` | named, LumenPnP | 1 |
| `gerber_closedloop::corpus_sweep_partition_floor` | named, 7 boards | 7 |
| `gerber_inkplate` | named, Inkplate 6 films | 1 |
| `strap_lint_corpus`, `behavioral_faults`, `usb_c_double_termination`, `powerup_state_fuzz` | named | Olimex, Watchy, ZSWatch, MNT Reform, Lily58, LumenPnP |

Every entry is read by at least one sweep. What no NAMED gate asks for, and so
contributes only parse-and-stay-quiet coverage: the two Adafruit nRF52840 boards,
the three MicroMod processor boards, the Qwiic HAT, Olimex ESP32-PoE and RVPC, the
three added LibreSolar boards, moco, Duet 2, HackRF One, the CATs Eurosynth
modules, the apfaudio front end, and the Gekkio boards. That is the intended state
for a board added for format or class diversity; it becomes a problem only if a
check that should have something to say about them is never pointed at them.

`gerber_uconsole` and the `hunt/` project-rule regression report `NOT RUN` on a
public fetch: the uConsole films are licence-unconfirmed, and the hunt set is not
redistributable. Both print what is missing and neither passes quietly. The
maintainer-only Altium family behaves the same way, gated on
`HAUKSBEE_REQUIRE_ALTIUM_CORPUS`.

## What the silence gates decline to grade themselves on

A silence gate's claim is "the checks stay quiet on hardware that is fine", so its
input set is hardware known to be fine, which is narrower than the corpus. Eight
entries carry `known_good = false` in `corpus.toml` with the reason recorded per
entry. They are still fetched, still parsed, still counted for format coverage.
Every exclusion prints a `NOT KNOWN-GOOD` line beside the `SCANNED` counts.

| Entry | Why it is not in the known-good set |
| --- | --- |
| `kicad_demos` | Reference designs to demonstrate KiCad, never manufactured. The trace-ampacity check fires on `demos/pic_programmer`: `Net-(D1-K)` is 0.50 mm of copper under the 1.50 A its 7805 is rated for, where IPC-2221 wants 0.53 mm at a 10 C rise. Right about the file; nobody ever built the board. |
| `cats_eurosynth` | DIY community modules by one maintainer. Copper contacts on three of the 88: `GND` to `RESET` on Baby 8 at -0.125 to -0.200 mm, `GND` to `Net-(D4-Pad1)` on Envelope Follower Main at seven places across both layers, `GND` to `Net-(R14-Pad2)` on HAGIWO 4Ch Sampler at -0.058 mm. Unadjudicated. |
| `olimex_esp32_poe` | Shipped for years. Rev L1: three `+3V3` to `+3.3V` contacts on B.Cu clustered at x=95.7-96.3, y=123.4-123.9, tightest -0.011 mm, where a 1.016 mm `+3.3V` track crosses the filled `+3V3` pour, and no part joins the two nets anywhere on the board. Rev M2: `/+5V_USB` to `Net-(D7-A)` at -0.508 mm. Read from KiCad's own stored fill polygons, so the geometry in the file is real; whether the fabricated board has it turns on whether that stored fill is current with the tracks. Unadjudicated. |
| `duet2` | Shipped for years. `FAN2-` to `5V_EXT` on B.Cu at -0.254 mm on Duet2 v1.05. A fan return meeting the external 5 V input would be a real defect rather than a naming artefact, which is why it needs adjudication and not an exception. |
| `emontx3` | Both GND/AGND contacts are real copper and the schematic declares the tie, but it carries no board-local coordinate authority for either contact. They remain serious; the entry is owned by the two-sided known-fault gate rather than a silence sweep. |
| `emontx3_v340` | The shipped Gerber export disproves the top-layer GND/AGND merge inferred from overlapping near-zero-isolate pour outlines; the bottom-layer contact is real. This measured false positive stays as a regression input. |
| `odrive_v2` | The directory contains an abandoned Altium attempt with a real GND/AGND short alongside the final board. It remains format coverage, but a directory-level silence claim would be false. |
| `mwgen_g1` | KiCad's own DRC confirms six pad-overlap shorts forbidden by the project's rules. It remains a useful RF/placement-collision input, not silence evidence. |

Excluding a board is not a way to make a gate green. It is a claim about the board,
it has to be stated, and `scripts/check-corpus.py` fails on `known_good = false`
with no reason attached.

## The findings the corpus expansion surfaced

Recorded here rather than fixed quietly, because each needs a decision by somebody
who owns the check:

1. **`+3V3` / `+3.3V` on Olimex ESP32-PoE rev L1, and `/+5V_USB` / `Net-(D7-A)` on
   rev M2.** Geometry above. The decisive test is whether the repository's own
   gerbers for those revisions carry the same overlap: if they do, the board has it;
   if they do not, the `.kicad_pcb` was saved with a stale zone fill and the check
   is right about the file and wrong about the hardware.
2. **`FAN2-` / `5V_EXT` on Duet 2 v1.05.** Same shape, different board.
3. **Three `GND` contacts across the CATs Eurosynth modules.** A board class the
   project's own hunts describe as where real defects survive, so these are hunt
   candidates rather than calibration noise.
4. **Trace ampacity on `demos/pic_programmer`.** Marginal, and on a demo.
5. **`L3` on the LibreSolar BMS 5s50 control PCB.** Its layout gives L3 the literal
   value `L` and carries no part-number property at all; the schematic specifies a
   Murata BLM18AG601SN1D ferrite bead. The placeholder check is right about the
   layout. Recorded as a dated exception in `placeholder_lint_corpus`, expiring
   first among the exceptions there, because the fix is real and specific: accept a
   manufacturer part number as specification, and consult the schematic's fields
   when a layout has none.
6. **The LumenPnP boot-state fuzz names nets that do not exist.**
   `powerup_state_fuzz::lumenpnp_motor_gate_boot_states_are_safe` fuzzes
   `Net-(Q1-Pad1)`, `Net-(Q3-Pad1)` and `Net-(Q4-Pad1)`; the extractor produces
   `Net-(Q2-Pad1)`, `Net-(Q5-Pad1)` and `Net-(Q6-Pad1)` from the same file, so the
   run ends in `UnknownNets`. The mobo's MOSFET sheet is instantiated several times
   and its symbol references are the template's (`Q1`, `R41`, `R42`), so the
   designators the test wants exist only after hierarchical instance resolution.
   This test has never run: it addressed the board through a `famous/` level the
   fetch does not produce, and after that was fixed the corpus resolver could not
   see a corpus from a worktree. It is a hierarchical-annotation question in the
   schematic reader, not a corpus one.

## Reproducing this

```bash
scripts/fetch-corpus.sh --dir /tmp/corpus
python3 scripts/check-corpus.py --dir /tmp/corpus
export HAUKSBEE_CORPUS_DIR=/tmp/corpus
HAUKSBEE_REQUIRE_CORPUS=1 cargo test --workspace -- --nocapture
```

Read the `SCANNED`, `NOT KNOWN-GOOD`, `EXCEPTION` and `NOT RUN` lines, not the
green tick. The tick says the assertions held; those lines say what they held over.
