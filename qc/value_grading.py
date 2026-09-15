"""Grade what a release board journey DELIVERED, not merely whether it was honest.

A journey that loads a board, prints a report, exports matching JSON and refuses
nothing can still be worthless: the ardep automotive Gerber archive passed every
honesty check in iteration ``2026-08-09-external-01`` while reconstructing one
net ("GND") out of four copper layers and 2575 aperture flashes. Nobody could
take that to a bench. This module is the part of the gate that says so.

Four grades, the last of which is the refusal contract that already existed:

``delivered``
    Bench-grade. A user could drop this input and work from what came back.

``degraded``
    The tool did less than a bench needs AND said so, naming the upload that
    unlocks more. A warning, enumerated in the run summary, never hidden.

``failed``
    Either the reconstruction or the binding collapsed on information the input
    actually contained, or the output degraded silently. Fails the gate run.

``refused-honest``
    An ``unreadable-by-design`` input that refused correctly. Unchanged, and
    still a pass: refusing an input that genuinely carries nothing readable is
    the right answer, and this module must never create pressure to fake a
    report out of one.

Two design constraints shape every threshold below.

First, no perverse incentive. The numbers that decide a grade are anchored to
something the tool under test does not write: the placement records in the input
file, the copper layers and aperture flashes in the staged archive, and the
report's agreement with itself (an absent inventory, a positioned component list
longer than the component total, a section with no verdict, a binding fraction
whose numerator exceeds its denominator). Reconstruction is bounded from ABOVE as
well as below: a package cannot report more nets than it has flashed features to
put them on. That upper bound is deliberately the weakest form nobody can argue
with rather than a tight one, so it catches shattering rather than shading.

Be precise about what the Gerber cap does and does not close. It bounds the grade
from above, so no count can reach ``delivered``. It does NOT close the step that
decides a run's exit code: twelve invented net names lift the ardep archive from
``failed`` to ``degraded``, because the floor reads a count and cannot see a
partition. That residual is engine follow-up 2 in
the private release-gate notes.

One dimension has no such anchor and is graded on the tool's own words: binding.
``critical_parts_bound`` and ``open_parts`` are both written by the report, so a
board that publishes an empty ``open_parts`` grades better than one that honestly
lists what it left open. That is a deliberate trade, because the alternative is
to punish the honesty that gets 61 real boards to ``degraded``, and it is why the
per-part rules demand that the list and the unlocks AGREE with each other, and
why a board with no list at all is disclosed under ``unverified_binding``.
Closing it properly needs a binding fact the engine derives rather than reports.

Three things this module cannot settle are named rather than approximated, all in
the private release-gate notes and all one engine field each: whether a
reconstructed net *partition* matches the copper it came from (a net count alone
cannot say), the placement total for formats with no exact placement token, and
the binding anchor above. Where a board falls into one of those, it records the
gap in its signals and the run summary lists it.

Second, no punishing honest limits. A missing SPICE model is not a defect in the
reader: the model was never in the input. Those cases grade ``degraded`` as long
as the report names, per part, the file that would fix them, and the excuse has
to be of a kind that addresses the gap.
"""

from __future__ import annotations

import html
import math
import re
import zipfile
from pathlib import Path
from typing import Iterable

# --- Thresholds ------------------------------------------------------------
#
# Every constant here is derived from a property of real boards, and each one
# is stated as the weakest form that still catches a collapse, so a board that
# is merely sparse or merely unmodelled cannot trip it.

# There is deliberately NO "acceptable fraction of parts bound". The
# `critical_parts_bound` ratio is read in exactly one place: where it says parts
# are unbound and the open-parts list names none of them, something must still be
# offered that would bind them. Nothing else consults it.
#
# Two reasons. A ratio invites an arbitrary line: every part left OPEN makes the
# nets through it untrustworthy, so 4 of 5 bound is not bench-grade, it is
# bench-grade on most of the board. And the ratio's denominator is active ICs
# only, so an open power MOSFET, whose own consequence line says the nets through
# it are isolated in simulation, would not appear in it at all.
#
# The rule keys on the report's `open_parts` list instead:
#
#   * no open parts -> eligible for `delivered`;
#   * any open part -> `degraded`, but ONLY when a model-kind unlock names that
#     same part, so the shortfall is one a person can look up, model and re-run;
#   * an open part with nothing offered for it -> `failed`.
#
# The list is also the fair thing to demand: the engine deliberately omits a part
# from `open_parts` when it sits off the connected path, and a rule counting
# names against the ratio would fail that honest report. Where the ratio does say
# parts are unbound and the list names none, something must still be offered that
# would bind them.
#
# Measured, not hoped for: across the 72 boards retained in this repository's
# evidence, every one of the 60-odd boards with open parts carries a per-part
# unlock for every one of them, and nothing fails this rule.

#: Nets any layout must carry. Two, flat, with no scaling by component count: a
#: bank of a hundred capacitors across two rails is a real design with many parts
#: and exactly two nets, so "more parts implies more nets" is not true and a
#: ratio built on it would fail good boards. Two still kills the degenerate "one
#: component, one net" report that no layout produces, and extraction coverage is
#: guarded by the placement count rather than by inference from connectivity.
MIN_NETS_FOR_A_LAYOUT = 2

#: The fewest check sections a report may carry and still be worth a bench.
#:
#: One section with a title and a verdict satisfied "the report ran checks", so an
#: engine regression that dropped DRC, thermal and signal integrity and kept a
#: single summary section graded `delivered`. Measured over the retained
#: evidence: all 69 successful reports carry 3 sections (63) or 4 (6), with no
#: spread below three. A floor of 2 therefore sits a full section below every
#: real board, which keeps a future format that legitimately reaches fewer
#: conclusions out of trouble, while a collapse to a lone placeholder fails.
MIN_CHECK_SECTIONS = 2

#: What a set of fabrication films can never carry, whatever the reader does with
#: it. The cap to `degraded` on a Gerber package is the gate's own structural
#: statement about the format, so it comes with its own unlock rather than
#: demanding one from the report.
#:
#: This is not a sentence invented about the RUN: it is a fact about the input
#: format, true of every Gerber package before the tool touches it. Requiring the
#: report to supply one instead would fail a package for which the reader emits no
#: coverage note, and would reward leaving flashes unmatched, since the note is
#: what the unlock is derived from. Both retained Gerber packages do carry such a
#: note, so this is a structural argument rather than an observed failure.
GERBER_STRUCTURAL_UNLOCK = (
    "A set of fabrication films cannot carry design rules or a BOM: supply the "
    "original native layout for clearance DRC and trace-geometry SI, and a "
    "pick-and-place file to place components."
)

#: What the gate needs before it will call a board bench-grade on formats it
#: cannot read. Altium's binary `.PcbDoc`, ODB++, IPC-2581 and the Board-as-Code
#: DSL carry neither an exact placement token nor a declared-net record, so every
#: number for those boards comes from the tool under test. They pass, they are
#: enumerated, and they stop one grade short of `delivered`.
UNANCHORED_INPUT_UNLOCK = (
    "Nothing in this board's format could be checked against its own bytes: the "
    "grade rests entirely on figures the tool reported. Publishing "
    "`num_input_placements` in the web report would anchor it."
)

#: Flashed features a reconstructed net needs at minimum, which sets the ceiling
#: on how many nets a package can report. One, not two: a single unconnected pad
#: or a lone copper island is a net some readers legitimately count, and demanding
#: two would reject it. One is the hard constraint that cannot be argued with, and
#: it still means a package cannot report more nets than it has flashed features
#: to put them on.
MIN_FLASHES_PER_NET = 1

#: Below this many aperture flashes a Gerber package is a coupon, a panel
#: stencil or a single-part breakout, and net-count arithmetic says nothing
#: useful. The reconstruction floor does not apply to those at all.
GERBER_MIN_FLASHES_FOR_FLOOR = 500

#: The one axis that exempts a package from the reconstruction floor.
#:
#: A flash count alone cannot tell a collapsed reconstruction from a board that
#: genuinely has two nets: three hundred capacitors across two rails is six
#: hundred flashed pads and exactly two nets, and a power-distribution board or an
#: LED backplane is the same shape. What rules those out is not the copper but
#: what the board IS, and the manifest states that independently of the tool under
#: test.
#:
#: Stated as the EXEMPTION rather than as an allowlist of MCU families, so the
#: guard is fail-closed. An allowlist is fail-open: the external pool that
#: produced the ardep collapse also carries an `efm32` board, a family no
#: hand-written list happened to include, and every such spelling would have
#: switched the floor off silently. A board is held to the floor unless its
#: manifest says outright that it has no microcontroller.
NO_MCU_AXIS = "no-mcu"

#: A single copper layer can legitimately be one plane. Two or more copper
#: layers plus the flash count above is a real board, and a real board carries at
#: minimum a supply and a return.
GERBER_MIN_LAYERS_FOR_FLOOR = 2

#: Flashes per net used to derive the reconstruction floor.
#:
#: A bound, not a fit, and the two real Gerber packages behind this contract sit
#: on opposite sides of it by a wide margin. Both figures below are the GATE's own
#: count of the copper films, not the reader's:
#:
#:   inkplate6_gerber  2 copper films, 1731 flashes -> 18 nets, floor  8  (passes)
#:   ardep mainboard   4 copper films, 2575 flashes ->  1 net,  floor 12  (fails)
#:
#: One package reconstructing more than twice its floor, the other a twelfth of
#: it. That is the discrimination the divisor is for, and it is measured rather
#: than asserted.
#:
#: The reasoning behind the number: a net needs at least one flashed feature to
#: exist on, which alone would give a floor of `flashes`. Real boards average well
#: under ten flashed features per net, and even a dense ground pour rarely exceeds
#: a few hundred flashed lands while remaining ONE net among many. 200 is
#: therefore roughly a twenty-fold margin over observed behaviour, which is the
#: point: it fires on collapse and on nothing else.
GERBER_FLASHES_PER_NET = 200

#: The floor never asks for more than this many nets, so a very large package
#: cannot manufacture an unreachable demand.
GERBER_MAX_EXPECTED_NETS = 32

#: The floor never asks for fewer than this. Two Gerber layers and 500+ flashes
#: is at minimum a supply and a return.
GERBER_MIN_EXPECTED_NETS = 2

#: How far the reader's own flash count may diverge from the gate's before the
#: run discloses that the two are not describing the same copper. The floor still
#: runs on the GATE's count, which is the authority here because it comes from the
#: bytes rather than from the tool under test; the disclosure exists so a reader
#: of the evidence can see the disagreement and judge it, not to soften the grade.
FLASH_COUNT_DIVERGENCE = 2.0

#: Fraction of the placements the gate counted in the input file itself that
#: must survive into the report's component list.
#:
#: Not every placement is an electrical part: logos, fiducials, mounting holes,
#: test points and courtyard-only footprints are placed like components and are
#: legitimately dropped.
#:
#: Measured, not guessed. Replaying the retained corpus evidence against the real
#: board files gives a verified ratio for 61 boards:
#:
#:     lowest 0.667 (lumenpnp, 24 of 36)   2nd 0.787   3rd 0.829
#:     median 0.987                        highest 1.000        below 0.5: none
#:
#: 0.5 therefore sits a quarter below the worst real board in the corpus, so it
#: fires only when extraction lost most of what the file named. There is
#: deliberately no upper bound on this ratio: resistor networks and multi-die
#: packages expand one placement into several modelled devices, so unlike
#: nets-from-flashes there is no defensible ceiling to enforce.
NATIVE_PART_RECOVERY_FLOOR = 0.5

#: Placement records the gate can count without an EDA parser. KiCad 6+ writes
#: `(footprint`, KiCad 5 wrote `(module`, and an Eagle board lists placed parts as
#: `<element>`. Neither token nests nor appears in a library section of these file
#: types, so counting them is exact.
#:
#: Deliberately NOT anchored to the start of a line. KiCad exports a minified
#: single-line `.kicad_pcb` (this repository ships one at
#: frontend/public/samples/boot_gate.kicad_pcb), on which a line-anchored pattern
#: matches nothing and silently reports zero placements.
_KICAD_PLACEMENT_RE = re.compile(rb"\((?:footprint|module)\s")
_EAGLE_PLACEMENT_RE = re.compile(rb"<element\s", re.IGNORECASE)
_PLACEMENT_PATTERNS = {
    "kicad_pcb": _KICAD_PLACEMENT_RE,
    "eagle_brd": _EAGLE_PLACEMENT_RE,
}

#: The reference designator each placement carries, so the gate can check the
#: report's component list by IDENTITY and not only by count. A fabricated entry
#: has to name a part the input file does not contain, which this catches and a
#: count never could.
_KICAD_REFERENCE_RE = re.compile(
    rb'\(property\s+"Reference"\s*"([^"]+)"|\(fp_text\s+reference\s+"?([^"\s\)]+)'
)
_EAGLE_REFERENCE_RE = re.compile(rb'<element[^>]*\sname="([^"]+)"', re.IGNORECASE)
_REFERENCE_PATTERNS = {
    "kicad_pcb": _KICAD_REFERENCE_RE,
    "eagle_brd": _EAGLE_REFERENCE_RE,
}

#: Nets the input file declares for itself. Eagle names them as `<signal>`
#: records. KiCad writes `(net <id> "NAME")` up to version 9 and the bare
#: `(net "NAME")` from version 10, repeating the record on every pad either way.
#:
#: BOTH spellings, and ids in preference to names where ids exist. Matching only
#: the numbered form read zero nets out of `rp2040_minimal_kicad`, a KiCad 10
#: board carrying 562 `(net "…")` records for 52 distinct nets that the report
#: recovers exactly, and then the docs charged that blind spot to the board, which
#: is the failure this whole contract exists to prevent.
#:
#: Preferring ids is not arbitrary. Older writers quote nothing at all: the
#: `mnt_reform` keyboard is `(version 4)` and yields 284 ids against ZERO quoted
#: names, while its report counts 284. `rp2040_minimal_kicad` is the only board in
#: either pool that quotes any. So the id is the net's identity and the quoted
#: name is a fallback for the writers that stopped emitting ids.
_KICAD_NET_ID_RE = re.compile(rb"\(net\s+(\d+)[\s\)]")
_KICAD_NET_NAME_RE = re.compile(rb'\(net\s+"([^"]*)"')
_EAGLE_NET_RE = re.compile(rb'<signal\s+name="([^"]+)"', re.IGNORECASE)
_NET_PATTERNS = {"kicad_pcb": _KICAD_NET_ID_RE, "eagle_brd": _EAGLE_NET_RE}

#: Fraction of the nets the input file declares that must survive into the
#: report. Measured over the real corpus: across all 61 boards whose declared
#: nets are countable, the engine reports EXACTLY the declared count, a ratio of
#: 1.00 with no spread at all. A floor of 0.5 therefore sits a full factor of two
#: below every real board, and still catches a hundred-net layout returning two.
#:
#: The count is bounded from above at the declared total, which stops a report
#: EXCEEDING it, and on Eagle the names are checked against the file's own
#: `<signal>` records, which stops a report inventing them. Neither closes the
#: floor, and it is worth being exact about what is left: a report that copies
#: the file's real net names into an inventory it never reconstructed clears both
#: checks, because every name it lists IS declared. Only a per-net feature count
#: distinguishes a reconstructed partition from a copied list, which is engine
#: follow-up 2. What the checks do buy is that the padding has to come from the
#: input file, so the tool cannot manufacture recovery out of nothing.
NATIVE_NET_RECOVERY_FLOOR = 0.5

#: Extensions that name a copper layer without ambiguity, and the KiCad /
#: Altium layer-name spellings that do the same job inside a generic `.gbr`.
#:
#: Best-effort, and used ONLY to describe the package in retained evidence. The
#: authoritative classifier is the engine's own
#: `crates/hauksbee-extract/src/gerber/layers.rs`, which reads X2 file functions
#: and a `.gbrjob`. A second, weaker copy of it must never decide a grade, which
#: is why the reconstruction floor keys on Gerber layers instead.
_COPPER_SUFFIXES = {
    # Protel / Altium / Eagle CAM conventions for copper, in the several
    # spellings the readers accept. Solder mask (`.gts`/`.gbs`), silkscreen
    # (`.gto`/`.gbo`), paste and outline layers are deliberately absent.
    ".gtl",
    ".gbl",
    ".g1",
    ".g2",
    ".g3",
    ".g4",
    ".g1l",
    ".g2l",
    ".g3l",
    ".g4l",
    ".gp1",
    ".gp2",
    ".gp3",
    ".gp4",
    ".cmp",
    ".sol",
    # `.art` is deliberately absent: Allegro writes mask, silk, paste, assembly
    # and drill films with it too, so treating it as copper would add their
    # apertures to the flash total and raise the floor above what the copper
    # supports. An Allegro copper film is still caught by its X2 attribute or by
    # a `_Cu` / `copper` name.
}
_COPPER_NAME_RE = re.compile(
    r"(?:^|[-_.])(?:f|b|in\d+|top|bottom|l\d+)[-_.]?cu(?:[-_.]|$)|copper", re.IGNORECASE
)
#: Layer roles that are never copper, whatever else their name contains. A film
#: called `soldermask_over_copper` matches the pattern above on the word
#: "copper" alone, and counting its apertures as copper would inflate the flash
#: total and raise the reconstruction floor above what the real copper supports.
#: The engine's classifier excludes these roles for the same reason
#: (crates/hauksbee-extract/src/gerber/layers.rs).
_NOT_COPPER_NAME_RE = re.compile(
    r"mask|paste|silk|legend|overlay|outline|keepout|courtyard|profile|drill"
    r"|assembly|fab|adhesive|glue|stencil|route|mill|dimension",
    re.IGNORECASE,
)
#: An X2 file function that is a recognised non-copper role. The standard's own
#: vocabulary, so a film that declares itself is never "unidentified".
_NON_COPPER_ATTRIBUTE_RE = re.compile(
    rb"%TF\.FileFunction,\s*(?:Soldermask|Solderpaste|Legend|Profile|Paste"
    rb"|Glue|Carbonmask|Goldmask|Heatsinkmask|Peelablemask|Silkscreen|Component"
    # `Pads` is deliberately absent: in X2 a `Pads` film is copper (pad shapes
    # only), so listing it here counted a copper film as definitely-not-copper.
    # A package of one `Copper` film beside one `Pads` film would then read as
    # fully classified at one copper layer and escape the floor entirely, with
    # the `Pads` flashes discarded. Left unlisted it is merely unidentified,
    # which keeps the package on the conservative lower-bound path.
    rb"|Depthroute|Vcut|VCut|Viafill|Other|Drillmap|FabricationDrawing"
    rb"|ArrayDrawing|AssemblyDrawing|Drawing|NonPlated|Plated)",
    re.IGNORECASE,
)
#: The X2 file-function attribute a layer states about itself. This is the
#: standard's own answer to "is this copper", it is what the engine reads, and
#: it beats any filename convention, so it is consulted first.
_COPPER_ATTRIBUTE_RE = re.compile(rb"%TF\.FileFunction,\s*Copper", re.IGNORECASE)
#: Gerber flash command, in both the current (``D03``) and legacy (``D3``)
#: spellings, optionally preceded by coordinates on the same block.
_FLASH_RE = re.compile(rb"D0*3\*")
#: Enough of a Gerber header to tell a layer from an Excellon drill file or a
#: README riding along in the same archive.
_GERBER_MARKERS = (b"%FS", b"%MO", b"D03*")
#: Streamed in chunks so a large panel cannot be pulled into memory whole.
#:
#: Chunks are cut on the LAST `*` in the buffer, never at an arbitrary offset.
#: Every Gerber command ends with `*`, so no command can straddle such a cut, and
#: the leftover tail carried into the next read contains no complete command and
#: cannot be double-counted. Cutting at a fixed offset with an overlap window
#: instead lost any command that happened to straddle the offset: it fell in the
#: gap between the scanned region and the carried tail.
_READ_CHUNK = 1 << 20
#: A buffer this long with no `*` in it is not Gerber, so it is abandoned rather
#: than accumulated without bound.
_MAX_UNTERMINATED = 1 << 22
#: Enough of a layer to carry its X2 attribute block, which sits in the header.
_HEADER_BYTES = 1 << 16
#: The engine's own coverage sentence. The gate derives its own flash count from
#: the bytes and compares the two: a large divergence means one of the two
#: misread the package, which a reader of the evidence should see. It is recorded
#: and disclosed, never graded on, because the gate's count is the authority and
#: the reader's is the thing under test.
_REPORTED_FLASHES_RE = re.compile(r"(\d+)\s+of\s+(\d+)\s+aperture flashes")
_CRITICAL_RE = re.compile(r"^\s*(\d+)\s*/\s*(\d+)\s*$")

#: Input formats that carry a component list. If one of these yields no
#: components at all, extraction collapsed on information the file contained.
PARTFUL_FORMATS = frozenset(
    {
        "kicad_pcb",
        "kicad_sch",
        "eagle_brd",
        "altium_pcbdoc",
        "ipc_2581",
        "odb_archive",
        "hauksbee_board",
        "hauksbee_board_archive",
    }
)

#: Fabrication copper only. No BOM exists in the input, so binding cannot be
#: graded; connectivity reconstruction and DRC are the whole of the value.
PARTLESS_COPPER_FORMATS = frozenset({"gerber_archive", "gerber_bundle"})

#: A netlist, not a layout: nets are the payload, components are optional.
NETLIST_FORMATS = frozenset({"ipc_356"})

DELIVERED = "delivered"
DEGRADED = "degraded"
FAILED = "failed"
REFUSED_HONEST = "refused-honest"


class ValueGrade:
    """One board's value verdict plus every fact the verdict rests on."""

    __slots__ = ("grade", "reasons", "unlocks", "signals")

    def __init__(
        self,
        grade: str,
        reasons: list[str],
        unlocks: list[str],
        signals: dict,
    ) -> None:
        self.grade = grade
        self.reasons = reasons
        self.unlocks = unlocks
        self.signals = signals

    def as_dict(self) -> dict:
        return {
            "grade": self.grade,
            "reasons": self.reasons,
            "unlocks": self.unlocks,
            "signals": self.signals,
        }


def _known_non_copper(name: str, header: bytes) -> bool:
    """Whether a film positively identifies as a role that is never copper.

    Used to tell "this package is single-sided, and here are its mask, silk and
    paste films" from "this package has one film we recognise and three we do
    not". The first is a complete classification; the second is not, and treating
    them alike let a collapsed package escape the floor with no disclosure.
    """

    if _NON_COPPER_ATTRIBUTE_RE.search(header) is not None:
        return True
    return _NOT_COPPER_NAME_RE.search(Path(name).stem) is not None


def _looks_like_copper(name: str, header: bytes) -> bool:
    """Whether a Gerber layer is copper: what it says first, what it is called second."""

    if _COPPER_ATTRIBUTE_RE.search(header) is not None:
        return True
    path = Path(name)
    if _NOT_COPPER_NAME_RE.search(path.stem) is not None:
        return False
    if path.suffix.casefold() in _COPPER_SUFFIXES:
        return True
    return _COPPER_NAME_RE.search(path.stem) is not None


def _count_flashes(handle) -> tuple[int, bool, bytes]:
    """Flashes in one archive member, whether it is a Gerber layer, and its head.

    The head is returned so the caller can read the layer's X2 file-function
    attribute without opening the member a second time.
    """

    flashes = 0
    is_gerber = False
    head = b""
    carry = b""
    while True:
        chunk = handle.read(_READ_CHUNK)
        if not chunk:
            break
        if len(head) < _HEADER_BYTES:
            head += chunk[: _HEADER_BYTES - len(head)]
        buffer = carry + chunk
        if not is_gerber and any(marker in buffer for marker in _GERBER_MARKERS):
            is_gerber = True
        complete, terminator, carry = buffer.rpartition(b"*")
        if terminator:
            flashes += len(_FLASH_RE.findall(complete + terminator))
        else:
            carry = buffer if len(buffer) <= _MAX_UNTERMINATED else b""
    # Whatever follows the final `*` cannot contain a complete command.
    return (flashes, True, head) if is_gerber else (0, False, head)


def gerber_input_facts(staged: Path) -> dict:
    """Count Gerber layers and aperture flashes in a staged Gerber package.

    Derived from the input bytes by the gate, never read off the report, so a
    reader that under- or over-reports its own coverage cannot move the floor
    it is measured against.
    """

    gerber_layers = 0
    copper_layers = 0
    identified_layers = 0
    copper_flashes = 0
    total_flashes = 0
    per_layer_flashes: list[int] = []
    readable = True

    def _members(path: Path):
        """Yield (name, opener) for each film, archive or unpacked directory.

        The staged path is an archive today, because `materialize_candidate` zips
        a bundle's members into one deterministic file before the drop. The
        directory branch is here so that stops being load-bearing: reading a
        directory as zero layers, zero flashes and "floor not applicable" is a
        silent fail-open, and the corpus does hold its one Gerber board
        (`inkplate6_gerber`) as a directory of films on disk. Both shapes give
        the same numbers.
        """

        if path.is_dir():
            for child in sorted(path.rglob("*")):
                if child.is_file() and not child.is_symlink():
                    yield child.name, lambda c=child: c.open("rb")
            return
        with zipfile.ZipFile(path) as archive:
            for info in archive.infolist():
                if info.is_dir():
                    continue
                yield info.filename, lambda i=info: archive.open(i)

    try:
        for name, opener in _members(staged):
            with opener() as handle:
                member_flashes, is_gerber, head = _count_flashes(handle)
            if not is_gerber:
                continue
            gerber_layers += 1
            total_flashes += member_flashes
            per_layer_flashes.append(member_flashes)
            # COPPER only, where copper can be told apart. Solder mask and
            # paste apertures mirror the copper pads, so counting them would
            # roughly triple the flash total and RAISE the net floor derived
            # from it, which would fail a legitimate low-net board. Copper is
            # also exactly what the engine reconstructs connectivity from, so
            # counting the layers it reads is the only comparison that means
            # anything.
            if _looks_like_copper(name, head):
                copper_layers += 1
                identified_layers += 1
                copper_flashes += member_flashes
            elif _known_non_copper(name, head):
                identified_layers += 1
    except (OSError, zipfile.BadZipFile, RuntimeError):
        readable = False

    # "Classified" means the copper is KNOWN, by either of two routes: enough films
    # positively identified themselves as copper to reason about, or every film in
    # the package was accounted for (which includes the honest answer "one copper
    # layer and three that are definitely not").
    #
    # Requiring the whole package to be accounted for was too strict on the very
    # board this contract exists to catch. Altium writes X2 attributes as
    # `G04 #@! TF.…*` comments and emits no `FileFunction` at all, so the ardep
    # mainboard has four clearly-named copper films among eleven, one of which is
    # unidentifiable; demanding all eleven collapsed its copper flash count to the
    # lower bound of 1 and its floor to the clamp minimum of 2. It still failed,
    # but by one net rather than on the evidence its own documentation described.
    #
    # A package where NOTHING identifies as copper still falls to the lower-bound
    # estimate, which is what keeps a deliberately unrecognisable naming scheme
    # from switching the floor off.
    # At least one film must have identified itself AS copper. Accepting "every
    # film is accounted for" on its own was fail-open: a package whose films all
    # match a never-copper name (`l1_route.gbr`, and Allegro writes exactly those)
    # scored `copper_layers == 0` with everything identified, which reported zero
    # copper flashes, skipped both bounds, and disclosed nothing. That is the
    # ardep escape hatch reachable by naming alone.
    classified = copper_layers >= 1 and (
        copper_layers >= GERBER_MIN_LAYERS_FOR_FLOOR
        or identified_layers == gerber_layers
    )
    if classified or gerber_layers < GERBER_MIN_LAYERS_FOR_FLOOR:
        flashes, layers = copper_flashes, copper_layers
    else:
        # Nothing said which films are copper: not the X2 attributes, not the
        # filenames. Switching the floor off here would hand every collapsed
        # package an escape hatch behind an unusual naming scheme, so the floor
        # still applies, on a genuine lower bound: if only the minimum number of
        # films are copper, the copper cannot carry fewer flashes than the same
        # number of SMALLEST films do. An average share would not be a bound at
        # all, because unidentified mask or assembly films can hold more flashes
        # than the copper (two 1000-flash copper films beside eight 10000-flash
        # others average to 16400, eight times the real copper, which would raise
        # the floor and fail a good board).
        flashes = sum(sorted(per_layer_flashes)[:GERBER_MIN_LAYERS_FOR_FLOOR])
        layers = GERBER_MIN_LAYERS_FOR_FLOOR
    return {
        "kind": "gerber",
        "gerber_layers": gerber_layers,
        "copper_layers": copper_layers,
        "identified_layers": identified_layers,
        "copper_classified": classified,
        "aperture_flashes": flashes,
        "total_gerber_flashes": total_flashes,
        "input_readable_by_gate": readable,
    }


def _staged_bytes(staged: Path, raw: bytes | None) -> bytes | None:
    """The staged file's bytes, reusing a caller's read where one was made."""

    if raw is not None:
        return raw
    try:
        return staged.read_bytes()
    except OSError:
        return None


def native_placement_count(
    input_format: str, staged: Path, *, raw: bytes | None = None
) -> int | None:
    """Placements named by the input file itself, or None where uncountable.

    The point of counting them here is that the report's own component total is
    written by the tool under test. A layout naming 373 placements that comes
    back as twelve components is a collapse the report has no way to disclose
    to itself, and no engine-authored number can reveal it.
    """

    pattern = _PLACEMENT_PATTERNS.get(input_format)
    if pattern is None:
        return None
    body = _staged_bytes(staged, raw)
    return None if body is None else len(pattern.findall(body))


def native_reference_designators(
    input_format: str, staged: Path, *, raw: bytes | None = None
) -> tuple[set[str], int] | None:
    """The designators the input names, and how many records carried one.

    Two numbers, because they are not the same: real boards legitimately repeat a
    designator (`crkbd` has two `G***` graphics, `watchy` two `TP` test points,
    `mnt_reform` fifty repeats), so the SET is smaller than the record count. The
    completeness test needs the count; membership checking needs the set.
    """

    pattern = _REFERENCE_PATTERNS.get(input_format)
    if pattern is None:
        return None
    body = _staged_bytes(staged, raw)
    if body is None:
        return None
    found: set[str] = set()
    hits = 0
    for groups in pattern.findall(body):
        candidates = (groups,) if isinstance(groups, bytes) else groups
        for value in candidates:
            if value:
                found.add(_decoded_attribute(value))
                hits += 1
                break
    return found, hits


def _decoded_attribute(value: bytes) -> str:
    """An XML attribute value the way the READER sees it, entities and all.

    The Eagle extractor calls quick-xml's `unescape_value`
    (crates/hauksbee-extract/src/eagle.rs), so a net named `STX-&gt;` in the file
    reaches the report as `STX->`. Comparing the raw bytes against that decoded
    output made the gate call the difference invention: the real
    `solokeys_solo_usb_a` board in the external pool carries `STX-&gt;` and
    `STX-&gt;1`, and an honest report of it FAILED on "the report names net(s) the
    input file does not declare". Escaping is a property of the file format, not
    of the tool, so the gate has to read through it the same way.
    """

    return html.unescape(value.decode("utf-8", "replace"))


def native_declared_net_names(
    input_format: str, staged: Path, *, raw: bytes | None = None
) -> set[str] | None:
    """Declared net names, only for formats where the report's names match them.

    Eagle only, and that is measured rather than assumed: across all 15 Eagle
    boards in the corpus every net the report names is a declared `<signal>`, with
    no extras. KiCad is deliberately excluded, because 38 of its 46 corpus boards
    report names that are not literal quoted strings in the file at all (older
    writers leave them unquoted, and the engine synthesises `Net-(U4-…)` for
    unnamed nets and escapes `~{…}`), so comparing them would manufacture
    failures. The KiCad residual is a named follow-up instead.
    """

    if input_format != "eagle_brd":
        return None
    body = _staged_bytes(staged, raw)
    if body is None:
        return None
    return {_decoded_attribute(m) for m in _EAGLE_NET_RE.findall(body)} or None


def declared_nets_are_exact(input_format: str, staged: Path,
                            *, raw: bytes | None = None) -> bool:
    """Whether the declared-net count is exact rather than a lower bound.

    Net ids are exact: every net has one. Quoted names are NOT, because KiCad
    leaves single-pad nets unnamed, so a name-derived count can only under-state.
    An under-stated denominator must never drive the "more nets than the file
    declares" FAILURE, or an honest report of a board with unnamed nets would be
    failed for having them.
    """

    if input_format != "kicad_pcb":
        return True
    body = _staged_bytes(staged, raw)
    if body is None:
        return True
    return bool({i for i in _KICAD_NET_ID_RE.findall(body) if i != b"0"})


def native_declared_nets(
    input_format: str, staged: Path, *, raw: bytes | None = None
) -> int | None:
    """How many nets the input file declares, or None where uncountable."""

    pattern = _NET_PATTERNS.get(input_format)
    if pattern is None:
        return None
    body = _staged_bytes(staged, raw)
    if body is None:
        return None
    if input_format == "kicad_pcb":
        # Ids first: they are the net's identity, and 0 is the unconnected
        # pseudo-net. Only where the writer emits none (KiCad 10 and later) does
        # the quoted name stand in for it.
        found = {i for i in pattern.findall(body) if i != b"0"}
        if found:
            return len(found)
        return len({n for n in _KICAD_NET_NAME_RE.findall(body) if n})
    return len({name for name in pattern.findall(body) if name})


def input_facts(input_format: str, staged: Path) -> dict:
    """Facts about the input the gate can establish without the tool's help."""

    if input_format in PARTLESS_COPPER_FORMATS:
        return gerber_input_facts(staged)
    # Read once. Three separate read_bytes() calls on a layout the frontend
    # comments describe as reaching 300 MB is three times the I/O for one answer.
    try:
        raw: bytes | None = staged.read_bytes()
    except OSError:
        raw = None
    placements = native_placement_count(input_format, staged, raw=raw)
    designators = native_reference_designators(input_format, staged, raw=raw)
    declared_nets = native_declared_nets(input_format, staged, raw=raw)
    references, reference_hits = designators or (set(), 0)
    return {
        "kind": "native",
        "input_format": input_format,
        "input_placements": placements,
        "input_declared_nets": declared_nets,
        # False where the count came from quoted names, which under-state.
        "declared_nets_exact": declared_nets_are_exact(
            input_format, staged, raw=raw
        ),
        "input_net_names": sorted(
            native_declared_net_names(input_format, staged, raw=raw) or ()
        )
        or None,
        # Trusted only when a designator was recovered for EVERY placement the
        # gate counted, so a partial extraction can never reject a real part.
        # Compared on the record COUNT, not the set size: comparing the set
        # disabled the check on 16 of the 63 graded boards whose placements the
        # gate can count, `watchy` among them, purely because they repeat a
        # designator, and a fabricated component list then graded `delivered`.
        "input_references": (
            sorted(references)
            if designators is not None
            and placements is not None
            and placements > 0
            and reference_hits == placements
            else None
        ),
    }


def expected_min_nets(facts: dict, axes: Iterable[str] = ()) -> int | None:
    """The reconstruction floor for a Gerber package, or None where none applies."""

    if facts.get("kind") != "gerber" or not facts.get("input_readable_by_gate"):
        return None
    # A board the manifest declares as having no microcontroller may legitimately
    # have two nets; see NO_MCU_AXIS.
    if NO_MCU_AXIS in set(axes):
        return None
    classified = facts.get("copper_classified", True)
    # APPLICABILITY is judged on the whole package: "is this a real board at all".
    # Judging it on the lower bound switched the floor off for a package of
    # 1000/1000/10/10-flash unrecognised films, whose two smallest total twenty.
    applicable_flashes = int(
        (facts.get("aperture_flashes") if classified else facts.get("total_gerber_flashes"))
        or 0
    )
    layers = (
        int(facts.get("copper_layers") or 0)
        if classified
        else int(facts.get("gerber_layers") or 0)
    )
    if (
        applicable_flashes < GERBER_MIN_FLASHES_FOR_FLOOR
        or layers < GERBER_MIN_LAYERS_FOR_FLOOR
    ):
        return None
    # The floor's VALUE still comes from the lower bound, so it can only ever
    # under-state what the copper could carry.
    flashes = int(facts.get("aperture_flashes") or 0)
    return min(
        GERBER_MAX_EXPECTED_NETS,
        max(GERBER_MIN_EXPECTED_NETS, flashes // GERBER_FLASHES_PER_NET),
    )


def _critical_bound(report: dict) -> tuple[int, int] | None:
    bind = report.get("bind")
    if not isinstance(bind, dict):
        return None
    match = _CRITICAL_RE.match(str(bind.get("critical_parts_bound") or ""))
    if match is None:
        return None
    return int(match.group(1)), int(match.group(2))


#: How many unlock sentences the summary carries before it says "and N more".
#: A 373-part board can produce ninety near-identical "add a model for Rnnn"
#: lines; the retained report already holds every one of them, and a summary
#: nobody can read is not a summary.
MAX_SUMMARIZED_UNLOCKS = 8


def _entries(value: object) -> list:
    """A tool-written collection, or an empty list when it is not one.

    Every array in a report comes from the tool under test, so `x or []` is not
    enough: `assumptions: 3` is truthy and not iterable, and letting a TypeError
    escape the gate leaves a reserved iteration with no terminal ledger record.
    """

    return value if isinstance(value, list) else []


def _self_consistency_failures(report: dict, components: int, nets: int) -> list[str]:
    """Ways a report contradicts itself, which no amount of honesty excuses.

    A total that outruns the list behind it is the cheapest possible way to
    look like coverage, and it is the one form of inflation the gate can settle
    from the artifact alone. Checked for both inventories, and for the shape of
    the check sections, so an empty placeholder cannot pass as a check that ran.
    """

    failures: list[str] = []
    # Both inventories must be PRESENT. Without them the totals beside them are
    # unverifiable, and an unverifiable total is exactly the number worth
    # inflating: omitting `components` would otherwise let a claimed 50 clear the
    # placement-recovery floor, and omitting `nets` would let a claimed nine
    # clear the Gerber reconstruction floor. All 69 successful reports in the
    # retained evidence carry both, so requiring them costs nothing real.
    for field in ("components", "nets"):
        if not isinstance(report.get(field), list):
            failures.append(
                f"the report published no {field} inventory, so its {field} total "
                "cannot be checked against anything"
            )
    # Distinct identities, not just a length. A list of nine copies of "GND" is
    # nine entries long and one net, and padding a list with repeats is the
    # cheapest way to make a total look backed.
    net_names = report.get("nets")
    if isinstance(net_names, list) and len(set(map(str, net_names))) != len(net_names):
        failures.append(
            f"the report's net inventory repeats itself: {len(net_names)} entries "
            f"naming {len(set(map(str, net_names)))} distinct nets"
        )
    listed_parts = report.get("components")
    if isinstance(listed_parts, list):
        # Only NAMED references have to be unique. A real board legitimately
        # carries several placements with no reference designator at all (a logo,
        # a fiducial, a mounting hole): one olimex_esp32 revision in the retained
        # corpus has two, and demanding uniqueness across those failed it.
        named = [
            str(item.get("reference"))
            for item in listed_parts
            if isinstance(item, dict) and str(item.get("reference") or "").strip()
        ]
        if len(set(named)) != len(named):
            failures.append(
                f"the report's component inventory repeats a reference: "
                f"{len(named)} named entries naming {len(set(named))} distinct parts"
            )
    # One-sided on purpose. `components` carries only the components the reader
    # could place (frontdoor.rs filters on `c.position`), so a positioned list
    # SHORTER than the total is normal and an unpositioned part is not a defect.
    # A list longer than the total is impossible, which is what this catches.
    listed = report.get("components")
    if isinstance(listed, list) and len(listed) > components:
        failures.append(
            f"the report claims {components} components but lists {len(listed)}"
        )
    # `nets` and `num_nets` are written from two different places in the engine
    # (`board.nets` deduplicated, against the binder's net names), and across all
    # 69 successful reports retained in this repository's evidence they agree
    # exactly. Requiring that is what stops a total from being inflated past the
    # inventory behind it, which the connectivity floor would otherwise trust. If
    # a legitimate divergence ever appears, this message names both numbers so
    # the rule can be relaxed against a real case rather than a hypothetical one.
    net_names = report.get("nets")
    if isinstance(net_names, list) and len(net_names) < nets:
        # One-sided: only a total that outruns its inventory can clear the
        # connectivity floor on names the report never produced. A shorter total
        # than the list buys nothing, and failing it would be stricter than the
        # rule's own purpose. All 69 retained reports have them equal.
        failures.append(
            f"the report claims {nets} nets and names {len(net_names)} of them"
        )
    sections = report.get("sections")
    if isinstance(sections, list):
        for index, section in enumerate(sections):
            if not isinstance(section, dict):
                failures.append(f"check section {index + 1} is not a section")
            elif not str(section.get("title") or "").strip():
                failures.append(f"check section {index + 1} has no title")
            elif not str(section.get("verdict") or "").strip():
                failures.append(
                    f"check section {str(section.get('title'))!r} reached no verdict"
                )
    return failures


def _base_designator(reference: str) -> str:
    """A reported reference, minus any suffix the engine added to disambiguate it.

    `@` is not legal in a KiCad or Eagle designator, so everything from the first
    one is the engine's own annotation: it reports a second unset `REF**`
    placeholder as `REF**@conflict-2`, and an Altium hierarchical part as
    `Q9@Top/AUX/M`. Comparing those verbatim against the file's designators
    accused two real corpus boards of inventing a part they had merely renamed.
    """

    return reference.split("@", 1)[0].strip() or reference


def _distinct_parts(listed: list) -> int:
    """Distinct NAMED parts a component list accounts for.

    Named references are counted once each, so repeats cannot pad the number.
    Reference-less entries are counted not at all: they cannot be checked against
    the input by identity, so counting them would let a list padded with anonymous
    entries clear the coverage floor without naming a single real part. Ignoring
    them is safe rather than harsh, because they are vanishingly rare on real
    boards: of the 9371 component entries across this repository's retained
    evidence, exactly two are unnamed, both on one olimex_esp32 revision.

    Counted by BASE designator, because the `@` suffix is the engine's own
    annotation and the numerator here is compared against a count of PLACEMENTS.
    Counting the raw strings made the annotation a padding vector: `R1`, `R1@a`,
    `R1@b` are three distinct strings whose base is in the file, so a report could
    clear the coverage floor on a 373-placement board from twelve real parts and
    187 variants of one designator. It also makes the comparison honest in the
    other direction, since a resistor network or a multi-die package expands one
    placement into several modelled devices under exactly these suffixes. Measured
    over the retained evidence: base counting changes the number on 3 of the 69
    successful reports and moves no board's recovery fraction near the floor (the
    lowest stays `lumenpnp` at 0.667).
    """

    return len(
        {
            _base_designator(str(item.get("reference")).strip())
            for item in listed
            if isinstance(item, dict) and str(item.get("reference") or "").strip()
        }
    )


def _open_parts(report: dict) -> list[str]:
    """Every reference the report flagged as OPEN *for want of a model*.

    This list, not the `critical_parts_bound` count, is what binding is graded
    on. Two reasons. The count's denominator is active ICs only, so an open
    power transistor, whose own consequence line says the nets through it are
    isolated in simulation, would not appear in it at all; and the engine may
    legitimately leave a part out of `open_parts` while still counting it in the
    ratio (an unresolved part off the connected path), which would make a count
    of names an unfair demand. Grading the list keeps both honest.

    `bind.open_parts` carries two buckets, and only the unbound one belongs here.
    See `_resolved_open_parts`.
    """

    bind = report.get("bind")
    if not isinstance(bind, dict):
        return []
    # Reference-less entries are KEPT. They are still open parts, so they still
    # keep the board off `delivered`; they are merely exempt from the per-name
    # unlock requirement below, because an unlock cannot name what the report did
    # not name. Dropping them entirely let an empty reference buy `delivered`.
    return [
        str(part.get("reference") or "").strip()
        for part in _entries(bind.get("open_parts"))
        if isinstance(part, dict) and part.get("bound") is not True
    ]


def _resolved_open_parts(report: dict) -> list[str]:
    """Open parts that HAVE a model: bound, on the live circuit, pins undriven.

    The engine folds two buckets into `bind.open_parts` and tells them apart with
    `bound` (crates/hauksbee-engine/src/frontdoor.rs). `bound: false` is a part
    with no model, which a model upload closes. `bound: true` is
    `resolved_but_open_active`: a resolved active IC carrying a genuine
    open/undriven-pin warning, whose own consequence line says analog, AC and
    thermal results on its nets are not fully trustworthy.

    That distinction has to be honoured, because the engine emits an `open_part`
    assumption only for `BindOutcome::Unresolved`
    (crates/hauksbee-engine/src/evidence.rs), so a bound-but-open part can never
    carry a model-kind unlock naming it. Demanding one failed a truthful report
    for a gap no upload closes, and said "the report named no model upload for
    Q3" about a part that has a model. It is still a real limitation, so it caps
    the board at `degraded` and is disclosed; what unlocks it is a corrected
    input, not an upload.
    """

    bind = report.get("bind")
    if not isinstance(bind, dict):
        return []
    return [
        str(part.get("reference") or "").strip() or "(unnamed)"
        for part in _entries(bind.get("open_parts"))
        if isinstance(part, dict) and part.get("bound") is True
    ]


def _open_active_ics(report: dict) -> list[str]:
    """References the report itself flagged as unbound ACTIVE integrated circuits."""

    bind = report.get("bind")
    if not isinstance(bind, dict):
        return []
    return [
        str(part.get("reference") or "?")
        for part in _entries(bind.get("open_parts"))
        if isinstance(part, dict) and part.get("active_ic") is True
    ]


def _add_caveat(signals: dict, text: str) -> None:
    """Record why the reconstruction floor was weakened, keeping every reason.

    More than one can apply at once, and `setdefault` hid the second: a package
    whose copper could not be classified AND whose flash count the reader
    disputes reported only the dispute.
    """

    existing = signals.get("reconstruction_floor_caveat")
    if not existing:
        signals["reconstruction_floor_caveat"] = text
    elif text not in existing:
        signals["reconstruction_floor_caveat"] = f"{existing}; and {text}"


def _merge_unlocks(primary: list[str], rest: list[str]) -> list[str]:
    """The shortfall's own uploads first, then anything else actionable.

    Order matters for a reader: the sentence that addresses THIS gap leads.
    Uncapped; `_cap_unlocks` truncates once, where the grade is built.
    """

    merged = list(primary)
    for text in rest:
        if text not in merged:
            merged.append(text)
    return merged


def _cap_unlocks(unlocks: list[str]) -> list[str]:
    """Truncate a long unlock list, naming how many were left in the report.

    Applied exactly once, at the grade. Capping in more than one place turned a
    real overflow of 32 into "and 1 more".

    A plain prefix is the wrong truncation: a board with two dozen "add a model
    for Rnnn" sentences and one "supply the manufacturer part number" would show
    eight of the first and lose the second entirely. Measured over the 64 successful
    corpus boards: 49 carry more uploads than the cap, and on 42 of those a plain
    prefix would drop a distinct kind of instruction altogether (comparing the
    shape set of the prefix against the whole list). The list is thinned by SHAPE
    instead, keeping one representative of each distinct kind before deepening any
    of them, so every different thing a reader could do survives the cap.
    """

    if len(unlocks) <= MAX_SUMMARIZED_UNLOCKS:
        return unlocks
    groups: dict[str, list[str]] = {}
    for text in unlocks:
        # The instruction's shape, ignoring the specific reference it names.
        shape = re.sub(r"[A-Za-z]*[\$#]?\d+\w*", "#", text)[:90]
        groups.setdefault(shape, []).append(text)
    kept: list[str] = []
    depth = 0
    while len(kept) < MAX_SUMMARIZED_UNLOCKS:
        added = False
        for members in groups.values():
            if depth < len(members) and len(kept) < MAX_SUMMARIZED_UNLOCKS:
                kept.append(members[depth])
                added = True
        if not added:
            break
        depth += 1
    overflow = len(unlocks) - len(kept)
    if overflow <= 0:
        return kept
    return [
        *kept,
        f"(and {overflow} more, listed in full in this board's retained report)",
    ]


def _unlocks(report: dict, *, kinds: Iterable[str] | None = None) -> list[str]:
    """Distinct 'supply this and you get more' sentences the report offered.

    ``kinds`` restricts the answer to assumptions of the engine's own kinds, so
    a degradation has to be excused by an upload that addresses THAT
    degradation. Without it, "supply the original layout to run DRC" would
    excuse a board with no models bound, which is a different gap and a
    different file.
    """

    wanted = None if kinds is None else set(kinds)
    found: list[str] = []
    for assumption in _entries(report.get("assumptions")):
        if not isinstance(assumption, dict):
            continue
        if wanted is not None and assumption.get("kind") not in wanted:
            continue
        # `replacement` is the engine's instruction field: what to supply. A
        # `coverage` note is descriptive by design and is deliberately NOT
        # accepted here, or "this came from gerber reconstruction" would read as
        # an actionable upload and excuse the degradation it merely narrates.
        replacement = str(assumption.get("replacement") or "").strip()
        if replacement and replacement not in found:
            found.append(replacement)
    return found


def _reported_flashes(report: dict) -> int | None:
    for note in _entries(report.get("notes")):
        if not isinstance(note, dict):
            continue
        match = _REPORTED_FLASHES_RE.search(str(note.get("message") or ""))
        if match is not None:
            return int(match.group(2))
    for assumption in _entries(report.get("assumptions")):
        if not isinstance(assumption, dict):
            continue
        match = _REPORTED_FLASHES_RE.search(str(assumption.get("because") or ""))
        if match is not None:
            return int(match.group(2))
    return None


def _cosim_refusal_reasons(report: dict) -> list[str]:
    """What the report itself said about why the firmware did not co-simulate.

    The field names are the engine's, not invented here: the C5.3 refusal
    contract is `claim` / `missing_prerequisite` / `valid_partial_conclusions` /
    `next_action` (frontend/src/lib/refusal-contract.ts), and a co-sim finding
    carries `what` / `why` / `fix`. Reading for `because` or `reason` would have
    rejected every honest refusal the engine actually emits.
    """

    stated: list[str] = []

    def add(text: object) -> None:
        value = str(text or "").strip()
        if value and value not in stated:
            stated.append(value)

    refusal = report.get("refusal")
    if isinstance(refusal, dict):
        add(refusal.get("missing_prerequisite"))
        add(refusal.get("next_action"))
    cosim = report.get("cosim")
    if isinstance(cosim, dict):
        for finding in _entries(cosim.get("findings")):
            if isinstance(finding, dict):
                add(finding.get("why"))
                add(finding.get("fix"))
    for assumption in _entries(report.get("assumptions")):
        if not isinstance(assumption, dict):
            continue
        haystack = f"{assumption.get('id')} {assumption.get('kind')}".casefold()
        if "cosim" not in haystack and "firmware" not in haystack:
            continue
        add(assumption.get("replacement"))
        add(assumption.get("consequence"))
    for note in _entries(report.get("notes")):
        if not isinstance(note, dict):
            continue
        message = str(note.get("message") or "")
        kind = str(note.get("kind") or "").casefold()
        if "cosim" in kind or "firmware" in kind or "firmware" in message.casefold():
            add(message)
    return stated


def _sim_advanced(result: dict) -> bool:
    before = result.get("sim_time_before_s")
    after = result.get("sim_time_after_s")
    if not isinstance(before, (int, float)) or not isinstance(after, (int, float)):
        return False
    return bool(result.get("live_started")) and after > before


def _grade_firmware(
    result: dict, report: dict, expectation: str, reasons: list[str]
) -> list[str]:
    """Fold the firmware dimension in. Returns extra unlock sentences."""

    firmware = result.get("firmware")
    if not isinstance(firmware, dict) or firmware.get("staged") is not True:
        reasons.append(
            "the manifest declared firmware for this board but the journey staged none"
        )
        return []
    if firmware.get("loaded") is not True:
        reasons.append("the staged firmware never reached the app's firmware slot")
        return []
    cosim = report.get("cosim")
    if expectation == "load-only" and not (
        isinstance(cosim, dict) and cosim.get("ran") is True
    ):
        # The image is expected NOT to co-simulate on this build. What is not
        # acceptable is "it did not, and no reason was given": load-only would
        # then excuse a malformed image, a loader crash and a missing backend
        # equally, which is precisely the escape hatch it must not be. The
        # reason has to come from the report, never from a sentence invented
        # here. A load-only image that DOES co-simulate is a pleasant surprise
        # and falls through to the ordinary co-sim checks below, so the
        # expectation lowers the bar for what must happen and never for the
        # quality of what did.
        if not str(firmware.get("detail") or "").strip():
            reasons.append(
                "firmware declared load-only did not report what the image is"
            )
        stated = _cosim_refusal_reasons(report)
        if not stated:
            reasons.append(
                "firmware declared load-only did not co-simulate and the report "
                "gave no reason, so the expectation excuses an unknown failure"
            )
            return []
        return stated
    if not isinstance(cosim, dict) or cosim.get("ran") is not True:
        reasons.append("firmware was staged but the report co-simulated nothing")
        return []
    seconds = cosim.get("seconds_simulated")
    if not isinstance(seconds, (int, float)) or seconds <= 0:
        reasons.append("firmware co-sim reported zero simulated seconds")
        return []
    if firmware.get("pin_activity_rendered") is False:
        # The co-sim reported driven pins and the page did not show them. A
        # result nobody can see is not a result.
        reasons.append("the co-sim reported pin activity that the page did not render")
    if not bool(firmware.get("pin_activity")):
        # Visible pin activity is the whole point of pairing firmware with a
        # board: it is the only observation that says the two interacted. There
        # is no upload that fixes it either, so it is a failure rather than a
        # degradation with a sentence the gate wrote for itself. Serial output is
        # recorded but is not a substitute.
        reasons.append(
            "the firmware co-simulated but drove no pin"
            + ("" if firmware.get("serial_activity") else " and printed nothing")
        )
        return []
    if not isinstance(cosim.get("analog_valid"), bool):
        # The field is not optional in the engine's co-sim section. Omitting it
        # must not grade better than declaring it false, which is exactly what a
        # rule keying only on the literal `False` would do.
        reasons.append(
            "the firmware co-simulated but stated no analog validity verdict"
        )
        return []
    if cosim.get("analog_valid") is False:
        # Digital toggling with the analog solve invalidated is not a bench
        # answer about the board, only about the firmware's control flow. What
        # would fix it is binding the open parts, which the report has already
        # said per part, so the unlock is ITS sentence and not one invented here.
        model = _unlocks(report, kinds=("open_part", "inferred_pin_role"))
        if not model:
            reasons.append(
                "the co-sim's analog results were invalidated and the report "
                "named nothing that would make them usable"
            )
            return []
        return model
    # A co-sim that drove pins, rendered them and kept its analog solve valid
    # needs nothing uploaded, so there is no unlock to add.
    return []


def grade_board(
    result: dict,
    *,
    input_format: str,
    expects_refusal: bool,
    facts: dict,
    firmware_expect: str | None = None,
    axes: Iterable[str] = (),
) -> ValueGrade:
    """Grade one browser journey row against the value contract."""

    reasons: list[str] = []
    axes = tuple(axes)
    signals: dict = {
        "input_format": input_format,
        "input_facts": facts,
        "firmware_expect": firmware_expect,
        "axes": list(axes),
    }

    if expects_refusal:
        # The refusal contract is validated elsewhere and in both directions.
        # Refusing an input that carries nothing readable is a delivered
        # answer, and grading it on parts or nets would be exactly the
        # incentive this module exists to avoid creating.
        return ValueGrade(REFUSED_HONEST, [], [], signals)

    report = result.get("report")
    if not isinstance(report, dict) or report.get("ok") is not True:
        return ValueGrade(
            FAILED, ["the journey produced no successful report"], [], signals
        )

    components = report.get("num_components")
    nets = report.get("num_nets")
    components = components if isinstance(components, int) else 0
    nets = nets if isinstance(nets, int) else 0
    sections = report.get("sections")
    section_count = len(sections) if isinstance(sections, list) else 0
    critical = _critical_bound(report)
    sim_advanced = _sim_advanced(result)
    # Each shortfall is excused only by an unlock of a kind that addresses it,
    # keyed off the engine's own assumption taxonomy rather than free text.
    model_unlocks = _unlocks(report, kinds=("open_part", "inferred_pin_role"))
    reader_unlocks = _unlocks(report, kinds=("reduced_fidelity",))
    unlocks = _unlocks(report)
    signals.update(
        {
            "num_components": components,
            "num_nets": nets,
            "section_count": section_count,
            "critical_parts_bound": None
            if critical is None
            else f"{critical[0]}/{critical[1]}",
            "critical_bound_fraction": None
            if critical is None or critical[1] == 0
            else round(critical[0] / critical[1], 4),
            "sim_time_before_s": result.get("sim_time_before_s"),
            "sim_time_after_s": result.get("sim_time_after_s"),
            "sim_advanced": sim_advanced,
        }
    )

    if section_count == 0:
        reasons.append("the report ran no checks")
    elif section_count < MIN_CHECK_SECTIONS:
        reasons.append(
            f"the report reached {section_count} check conclusion(s), fewer than "
            f"the {MIN_CHECK_SECTIONS} every real board in the retained evidence "
            f"carries"
        )
    if not sim_advanced:
        reasons.append(
            "the live simulation clock never advanced, so nothing was testable"
        )
    reasons.extend(_self_consistency_failures(report, components, nets))
    if critical is not None and critical[0] > critical[1]:
        reasons.append(
            f"the binding summary is malformed: {critical[0]} of {critical[1]} "
            "critical parts bound"
        )
    open_active_ics = _open_active_ics(report)
    if critical is not None and critical[1] == 0 and open_active_ics:
        # "No critical parts" and "an active IC is left OPEN" cannot both be
        # true: an unbound active IC is the definition of a critical part.
        # Reporting 0/0 there would shrink the denominator to nothing, which is
        # the one thing that could make the binding summary say less than the
        # open-parts list it sits beside.
        reasons.append(
            "the report claims no critical parts while leaving "
            + ", ".join(open_active_ics)
            + " open as active IC(s)"
        )

    # False for every input whose value is connectivity, and raised only where the
    # names were actually compared. Defaulted here rather than inside the two
    # branches that can check it: recording it only where it could be true left the
    # largest unverified dimension invisible on the boards it applied to most, and
    # a format nobody has classified yet would omit itself from the disclosure
    # entirely.
    signals.setdefault("net_identity_verified", False)

    degraded = False
    # Set when the degradation is the gate's own structural statement rather than
    # a shortfall the report is obliged to explain; see GERBER_STRUCTURAL_UNLOCK.
    structural = False

    if input_format in PARTLESS_COPPER_FORMATS:
        floor = expected_min_nets(facts, axes)
        reported = _reported_flashes(report)
        signals["expected_min_nets"] = floor
        # There are no declared net names in a fabrication package to compare
        # against, so the count is all there is and the reader is told so. The
        # native side of this disclosure exists; leaving copper out of it hid the
        # gap on exactly the format the whole contract was written for.
        signals["net_identity_verified"] = False
        signals["reader_reported_flashes"] = reported
        derived = int(facts.get("aperture_flashes") or 0)
        if reported and derived:
            ratio = max(reported, derived) / min(reported, derived)
            signals["flash_count_agreement"] = round(
                min(reported, derived) / max(reported, derived), 3
            )
            if ratio > FLASH_COUNT_DIVERGENCE:
                signals["reconstruction_floor_verified"] = False
                _add_caveat(
                    signals,
                    f"the reader counted {reported} aperture flashes and the gate "
                    f"counted {derived}; the floor used the gate's number",
                )
        if facts.get("copper_classified") is False:
            signals["reconstruction_floor_verified"] = False
            _add_caveat(
                signals,
                "the gate could not classify this package's copper, so the floor "
                "rested on a lower-bound estimate of it"
                if floor is not None
                else "the gate could not classify this package's copper",
            )
        if not facts.get("input_readable_by_gate"):
            # The gate stages the archive itself and read it at discovery, so
            # failing to read it here means the staged bytes are not what was
            # discovered. That is a gate fault, not a tool caveat.
            reasons.append("the gate could not re-read the staged Gerber package")
        else:
            # The CEILING needs only that flashes were counted at all. It is not
            # behind the floor's MCU-axis guard, because "more nets than the
            # copper has features to put them on" is impossible whatever the
            # board is; leaving it there let a `no-mcu` package report fifty
            # thousand nets from six hundred flashes and pass.
            # An UPPER bound on copper flashes, which is a different number from
            # the lower bound the floor uses. Where copper could not be
            # classified, `aperture_flashes` is deliberately the two smallest
            # films; using it here failed a real Allegro-style package that
            # reconstructed thirty nets from 1806 flashes, because the ceiling
            # had been computed from six.
            ceiling_flashes = int(
                (
                    facts.get("aperture_flashes")
                    if facts.get("copper_classified", True)
                    else facts.get("total_gerber_flashes")
                )
                or 0
            )
            # Below the applicability minimum a package's flash count says
            # nothing: copper drawn with D01 or filled with G36/G37 flashes only
            # a handful of vias and fiducials, and a ceiling derived from forty
            # of those would fail a board with forty real nets.
            if ceiling_flashes >= GERBER_MIN_FLASHES_FOR_FLOOR:
                ceiling = ceiling_flashes // MIN_FLASHES_PER_NET
                if nets > ceiling:
                    reasons.append(
                        f"implausible reconstruction: {nets} nets from "
                        f"{ceiling_flashes} aperture flashes, more than the "
                        f"{ceiling} that could each carry "
                        f"{MIN_FLASHES_PER_NET} features"
                    )
            # The FLOOR is arithmetic that means nothing on a package whose
            # copper is drawn (D01) or filled (G36/G37) rather than flashed, and
            # nothing on a board that may legitimately have two nets.
            if floor is not None and nets < floor:
                measured = (
                    f"{facts['copper_layers']} copper layers carrying "
                    f"{facts['aperture_flashes']} aperture flashes"
                    if facts.get("copper_classified", True)
                    else f"{facts['gerber_layers']} unclassified films totalling "
                    f"{facts.get('total_gerber_flashes', 0)} aperture flashes, of "
                    f"which at least {facts['aperture_flashes']} must be copper"
                )
                reasons.append(
                    f"connectivity reconstruction collapsed: {nets} net(s) from "
                    f"{measured}, where the floor for this package is {floor}"
                )
            if floor is None and nets == 0:
                reasons.append("no connectivity was reconstructed from the copper")
            # The floor did not apply, and a reader has to know why. Three
            # answers are worth disclosing: the manifest does not declare a
            # microcontroller, the gate could not pick copper out of the films, or
            # it derived no floor at all for a package that is dense as a whole.
            # A package genuinely too small to reason about is the fourth and is
            # deliberately silent: a hundred D01-drawn traces and six flashed vias
            # is the normal shape of a small board, not a limit worth reporting on
            # every one of them. All three disclosures ride in the run summary
            # next to the degraded list rather than sitting in a per-board row.
            if NO_MCU_AXIS in set(axes):
                signals["reconstruction_floor_verified"] = False
                # The measurement travels with the exemption. `no-mcu` is a
                # manifest CLAIM and the only switch that turns the sole
                # connectivity check on a copper package off, so a run that
                # honours it over a dense package has to print how dense: "no
                # floor applied" reads as routine, "no floor applied to 4 copper
                # films carrying 2575 flashes" does not. `scripts/check-corpus.py`
                # lints the same condition, but only over `corpus.toml`; this
                # covers every pool, because it runs where the grade is decided.
                copper_films = int(facts.get("copper_layers") or 0)
                dense_flashes = int(facts.get("aperture_flashes") or 0)
                measured = (
                    f", over a package this gate measures at {dense_flashes} "
                    f"aperture flashes across {copper_films} copper film(s)"
                    if copper_films >= GERBER_MIN_LAYERS_FOR_FLOOR
                    and dense_flashes >= GERBER_MIN_FLASHES_FOR_FLOOR
                    else ""
                )
                _add_caveat(
                    signals,
                    f"the manifest declares {NO_MCU_AXIS} for this board, so no "
                    f"reconstruction floor applies to it{measured}",
                )
            elif facts.get("copper_classified") is False:
                signals["reconstruction_floor_verified"] = False
                _add_caveat(
                    signals, "the gate could not classify this package's copper"
                )
            if floor is None and NO_MCU_AXIS not in set(axes):
                # Any OTHER reason the floor did not apply to a package that is
                # dense as a whole. One route there is silent and is exactly the
                # ardep shape: a real multilayer package in which one film looks
                # like copper and the rest positively mis-identify as never-copper
                # by name (Allegro writes `l2_route.gbr`), which reads as one
                # classified copper layer, falls below the two-layer guard, and
                # leaves `copper_classified` TRUE so neither branch above fires.
                # Density is judged on the whole package for the same reason
                # applicability is: the question here is "was this a real board
                # that went unchecked", not "how much of it is copper".
                total = int(facts.get("total_gerber_flashes") or 0)
                films = int(facts.get("gerber_layers") or 0)
                # One film is enough to disclose. A bare single-film upload with
                # thousands of flashes gets no floor either, and requiring two
                # films here left that the one silent case among four.
                if films >= 1 and total >= GERBER_MIN_FLASHES_FOR_FLOOR:
                    signals["reconstruction_floor_verified"] = False
                    _add_caveat(
                        signals,
                        f"no reconstruction floor could be derived for a package "
                        f"of {films} films carrying {total} aperture flashes, of "
                        f"which the gate could pick out "
                        f"{int(facts.get('copper_layers') or 0)} copper film(s)",
                    )
        # A fabrication package is at best `degraded`, unconditionally.
        #
        # Clearance DRC and trace-geometry SI need the original layout's rules,
        # which by definition are not in a set of films, and the engine says so
        # itself. So no component count and no net count makes a Gerber-only
        # input bench-grade, which is also what leaves nothing to gain by
        # inflating either of them. Only a reader-side unlock closes this gap;
        # "add a model for U1" would not place a single pad from copper.
        degraded = True
        structural = True
        # The report's own wording where it has any, and the format-level fact
        # otherwise. Either way this degradation never fails for want of an
        # unlock: the gate is the one making the claim.
        unlocks = reader_unlocks or [GERBER_STRUCTURAL_UNLOCK]
    else:
        if input_format in NETLIST_FORMATS and nets < MIN_NETS_FOR_A_LAYOUT:
            reasons.append(
                f"a netlist input yielded {nets} net(s), fewer than the "
                f"{MIN_NETS_FOR_A_LAYOUT} any board has"
            )
        # Partful formats, netlists, and anything the selector does not classify.
        # The input-derived floors apply only where the gate can read the file;
        # the binding rules below apply to every one of them, because they read
        # the report's own list rather than anything about the format.
        if input_format in PARTFUL_FORMATS:
            if components == 0:
                reasons.append(
                    "the input carries a component list and none survived extraction"
                )
            # The commensurability rule below covers a zero-net layout, so this
            # does not restate it.
            placements = facts.get("input_placements")
            signals["input_placements"] = placements
            if not isinstance(placements, int) or placements <= 0:
                # Said plainly in the evidence rather than left as a silence.
                # Either no exact placement token exists for this format, or the
                # token exists and matched nothing; either way the extraction
                # ratio for THIS board was not verified against the input, and a
                # reader of the evidence has to be able to see that. The
                # unlocking change is `num_input_placements` in the web report;
                # see the private release-gate notes.
                signals["placement_recovery_verified"] = False
            else:
                # Count what the report LISTED, never the bare total. Taking
                # `num_components` here would reward inflating it: a
                # 100-placement input could claim 50 components, list 12, and
                # clear the floor. Every placement the gate counted is a
                # positioned part, so on these formats the list is the honest
                # numerator.
                listed_components = report.get("components")
                # DISTINCT references, and no fallback to the claimed total: a
                # report with no inventory has already failed above, and either
                # substitution would be the one that made the total worth
                # inflating. Fifty copies of R1 recover one part, not fifty.
                recovered = (
                    min(components, _distinct_parts(listed_components))
                    if isinstance(listed_components, list)
                    else 0
                )
                recovery = recovered / placements
                signals["recovered_components"] = recovered
                # Identity, not only count. Inflating the total AND fabricating
                # list entries to match would still have to name parts the input
                # file does not contain, which a count can never catch.
                known = facts.get("input_references")
                signals["component_identity_verified"] = isinstance(known, list)
                if isinstance(known, list) and isinstance(listed_components, list):
                    # An unnamed placement cannot be matched by name, so it is
                    # neither confirmed nor invented; only named ones are checked.
                    known_set = set(known)
                    invented = sorted(
                        {
                            str(item.get("reference"))
                            for item in listed_components
                            if isinstance(item, dict)
                            and str(item.get("reference") or "").strip()
                            and _base_designator(str(item.get("reference")))
                            not in known_set
                        }
                    )
                    signals["components_not_in_the_input"] = invented
                    if invented:
                        reasons.append(
                            "the report lists component(s) the input file does not "
                            "contain: " + ", ".join(invented[:8])
                            + (
                                f" and {len(invented) - 8} more"
                                if len(invented) > 8
                                else ""
                            )
                        )
                signals["placement_recovery_verified"] = True
                signals["placement_recovery_fraction"] = round(recovery, 4)
                if recovery < NATIVE_PART_RECOVERY_FLOOR:
                    reasons.append(
                        f"extraction recovered {recovered} of the {placements} "
                        f"placements the input file itself names "
                        f"({recovery:.0%}), below the "
                        f"{NATIVE_PART_RECOVERY_FLOOR:.0%} floor"
                    )
            # A layout has at least a supply and a return. Nothing stronger is
            # inferred from the component count: see MIN_NETS_FOR_A_LAYOUT.
            signals["expected_min_nets"] = MIN_NETS_FOR_A_LAYOUT
            if nets < MIN_NETS_FOR_A_LAYOUT:
                reasons.append(
                    f"a layout came back with {nets} net(s), fewer than the "
                    f"{MIN_NETS_FOR_A_LAYOUT} any board has"
                )
            # And where the file declares its own nets, connectivity is measured
            # against that rather than against a rule of thumb. This is the ardep
            # collapse on a native format: a file declaring a hundred nets whose
            # report returns two.
            net_names = report.get("nets")
            declared = facts.get("input_declared_nets")
            signals["input_declared_nets"] = declared
            signals["net_identity_verified"] = False
            if not isinstance(declared, int) or declared <= 0:
                signals["net_recovery_verified"] = False
            else:
                net_recovery = nets / declared
                # VERIFIED means the denominator is the file's own answer. Where
                # the count could only be read from quoted names it under-states,
                # because KiCad leaves single-pad nets unnamed, so the ratio is
                # measured against a number the gate itself knows is low and the
                # board says so. `rp2040_minimal_kicad` graded `delivered` with
                # `net_recovery_verified: true` on exactly such a denominator.
                signals["net_recovery_verified"] = (
                    facts.get("declared_nets_exact") is not False
                )
                signals["net_recovery_fraction"] = round(net_recovery, 4)
                # Identity, where the format allows it. Padding the inventory up
                # to the declared total is how the floor was cleared without
                # reconstructing anything, and a count can never see it.
                known_nets = facts.get("input_net_names")
                signals["net_identity_verified"] = isinstance(known_nets, list)
                if isinstance(known_nets, list) and isinstance(net_names, list):
                    invented_nets = sorted(
                        {str(n) for n in net_names} - set(known_nets)
                    )
                    signals["nets_not_in_the_input"] = invented_nets
                    if invented_nets:
                        reasons.append(
                            "the report names net(s) the input file does not "
                            "declare: " + ", ".join(invented_nets[:8])
                            + (
                                f" and {len(invented_nets) - 8} more"
                                if len(invented_nets) > 8
                                else ""
                            )
                        )
                floor_nets = math.ceil(declared * NATIVE_NET_RECOVERY_FLOOR)
                # Bounded from ABOVE at the declared count as well. The gate read
                # that number exactly out of the bytes and honest recovery is
                # exactly 1.00 on all 61 measured boards, so a report claiming
                # more nets than the file declares has invented them. This stops
                # exceeding the total; the identity check above is what stops
                # padding up to it, where the format allows one.
                # A name-derived count under-states, so exceeding it is not
                # evidence of invention; those boards are already disclosed as
                # unverified above, and the collapse floor below still applies to
                # them, because recovering half of an under-stated total is a
                # collapse whichever way the denominator errs.
                if nets > declared and facts.get("declared_nets_exact", True):
                    reasons.append(
                        f"the report claims {nets} nets from a file that "
                        f"declares {declared}"
                    )
                if nets < floor_nets:
                    reasons.append(
                        f"connectivity collapsed: {nets} of the {declared} nets the "
                        f"input file itself declares, below the "
                        f"{NATIVE_NET_RECOVERY_FLOOR:.0%} floor of {floor_nets}"
                    )
        elif components == 0 and nets == 0:
            reasons.append("the report recovered neither components nor nets")

        # The binding dimension is the one graded entirely on the tool's own
        # words. What little corroboration exists is the open-parts list: the
        # unlocks must agree with it part by part. A report offering no list at
        # all has nothing to check, whether it says "0/0" or "40/40" - and the
        # second is the stronger claim with the same absence behind it, so keying
        # this on a zero denominator disclosed the weaker lie and hid the bigger.
        #
        # Disclosed, not capped. Capping would put every well-bound board in
        # `degraded` too, because binding is never anchored to the input for any
        # of them, and a grade nothing can reach says nothing. See the third
        # engine follow-up in the private release-gate notes.
        if critical is None or not _open_parts(report):
            signals["binding_verified"] = False
        if critical is None and input_format in PARTFUL_FORMATS:
            # Without a binding summary the reader cannot tell whether the
            # analog, AC and thermal answers on this board mean anything. On an
            # input that carries parts, publishing none is a value failure, not
            # a caveat the report made.
            reasons.append(
                "the report published no model-binding summary for an input "
                "that carries parts"
            )
        else:
            open_parts = _open_parts(report)
            named_open = [reference for reference in open_parts if reference]
            uncovered = [
                reference
                for reference in named_open
                if not any(
                    re.search(rf"(?<![\w$]){re.escape(reference)}(?![\w$])", text)
                    for text in model_unlocks
                )
            ]
            resolved_open = _resolved_open_parts(report)
            signals["resolved_open_parts"] = resolved_open
            if resolved_open:
                # A model is present and the pins are open in the INPUT's wiring.
                # No upload closes it, so it is degradation with an honest
                # instruction rather than a shortfall to punish.
                degraded = True
                unlocks = _merge_unlocks(
                    [
                        "Drive or connect the open pins of "
                        + ", ".join(resolved_open[:8])
                        + (
                            f" and {len(resolved_open) - 8} more"
                            if len(resolved_open) > 8
                            else ""
                        )
                        + " in the source design; the model is already bound, so "
                        "analog, AC and thermal results on their nets are not "
                        "fully trustworthy until the input drives them."
                    ],
                    unlocks,
                )
            signals["open_parts"] = len(open_parts)
            signals["open_parts_unnamed"] = len(open_parts) - len(named_open)
            signals["open_parts_without_an_unlock"] = uncovered
            if open_parts:
                # An open part the report did not name cannot be demanded by
                # name, but something must still be offered that would close it.
                if len(named_open) < len(open_parts) and not model_unlocks:
                    uncovered = uncovered or ["(an unnamed open part)"]
                # Any part left OPEN makes the nets through it untrustworthy, so
                # the board is not bench-grade. It is honest degradation when
                # every one of those parts is individually actionable, and a
                # failure when even one is not: an unlock naming U1 does nothing
                # for the other twelve.
                if uncovered:
                    reasons.append(
                        "the report named no model upload for "
                        + ", ".join(uncovered[:8])
                        + (
                            f" and {len(uncovered) - 8} more"
                            if len(uncovered) > 8
                            else ""
                        )
                    )
                else:
                    degraded = True
                    # The MODEL kind is what excuses this shortfall, but the
                    # summary carries everything actionable the report offered:
                    # narrowing the list here dropped a real `reduced_fidelity`
                    # upload from 40 of the 64 successful corpus boards, while the
                    # docs promise "the uploads that unlock more".
                    unlocks = _merge_unlocks(model_unlocks, unlocks)
            elif critical is not None and critical[1] > critical[0]:
                # The count says parts are unbound while the list names none of
                # them. The engine does this for a part off the connected path,
                # so demanding a name would be unfair; demanding that SOMETHING
                # would bind it is not.
                signals["unbound_criticals_unnamed"] = critical[1] - critical[0]
                if model_unlocks:
                    degraded = True
                    unlocks = _merge_unlocks(model_unlocks, unlocks)
                else:
                    reasons.append(
                        f"{critical[1] - critical[0]} of {critical[1]} critical parts "
                        "are unbound, named nowhere, and nothing was offered to bind them"
                    )

    # Absence counts as unverified, not as "not applicable": a format nobody has
    # classified sets neither signal, and reading that as "no gap" let it reach
    # `delivered` with nothing anchored and nothing disclosed.
    # A refusal never reaches here; it returned above.
    if input_format not in PARTLESS_COPPER_FORMATS:
        signals.setdefault("placement_recovery_verified", False)
        signals.setdefault("net_recovery_verified", False)
        # Identity too: it is only set inside the placement branch, so on a format
        # with no placement count it was neither verified nor disclosed.
        signals.setdefault("component_identity_verified", False)
    if (
        input_format not in PARTLESS_COPPER_FORMATS
        and signals.get("placement_recovery_verified") is False
        and signals.get("net_recovery_verified") is False
    ):
        # Nothing about this board was checkable against its own bytes: no exact
        # placement token, no declared-net record. The run still passes and the
        # board is enumerated, but it must not wear the top grade on the strength
        # of numbers only the tool wrote. Same conservative move the Gerber cap
        # makes, and it needs no engine field.
        degraded = True
        structural = True
        unlocks = [*unlocks, UNANCHORED_INPUT_UNLOCK]

    if firmware_expect is not None:
        extra = _grade_firmware(result, report, firmware_expect, reasons)
        if extra:
            degraded = True
            unlocks = [*unlocks, *(item for item in extra if item not in unlocks)]

    if reasons:
        return ValueGrade(FAILED, reasons, _cap_unlocks(unlocks), signals)
    if degraded:
        # Invariant, not a branch: every path that degrades either names the
        # report's own unlocks or is structural and supplies the gate's. Checked
        # rather than asserted, because `python -O` would drop an assert and a
        # silent shortfall is the one thing this grade must never become.
        if not unlocks and not structural:  # pragma: no cover - unreachable
            raise AssertionError(
                "a degraded grade must carry an unlock; this is a bug in "
                "qc/value_grading.py, not a property of the board"
            )
        return ValueGrade(DEGRADED, [], _cap_unlocks(unlocks), signals)
    return ValueGrade(DELIVERED, [], _cap_unlocks(unlocks), signals)


def summarize(graded: Iterable[tuple[str, ValueGrade]]) -> dict:
    """Roll per-board grades into the summary the evidence document carries.

    Two lists here are not grades but disclosures, and they are at summary level
    for the same reason the degraded list is: a limitation buried in per-board
    signals is a limitation nobody reads. `unverified_extraction` names the
    boards whose extraction coverage the gate could not check against the input,
    `unverified_reconstruction` names Gerber packages whose copper this gate could
    not pick out of their films, and `firmware_expectation_lowered` names every
    board whose manifest asked for less than a co-simulation, so none of the
    three can quietly become the norm.
    """

    summary: dict = {
        "delivered": [],
        "degraded": [],
        "failed": [],
        "refused_honest": [],
        "unverified_binding": [],
        "unverified_extraction": [],
        "unverified_identity": [],
        "unverified_connectivity": [],
        "unverified_net_identity": [],
        "unverified_reconstruction": [],
        "firmware_expectation_lowered": [],
    }
    for name, grade in graded:
        if grade.grade == DELIVERED:
            summary["delivered"].append(name)
        elif grade.grade == REFUSED_HONEST:
            summary["refused_honest"].append(name)
        elif grade.grade == DEGRADED:
            summary["degraded"].append({"board": name, "unlocks": grade.unlocks})
        else:
            summary["failed"].append({"board": name, "reasons": grade.reasons})
        if grade.signals.get("placement_recovery_verified") is False:
            summary["unverified_extraction"].append(
                {"board": name, "input_format": grade.signals.get("input_format")}
            )
        if grade.signals.get("binding_verified") is False:
            summary["unverified_binding"].append(
                {
                    "board": name,
                    "critical_parts_bound": grade.signals.get(
                        "critical_parts_bound"
                    ),
                }
            )
        if grade.signals.get("component_identity_verified") is False:
            summary["unverified_identity"].append(
                {"board": name, "input_format": grade.signals.get("input_format")}
            )
        if grade.signals.get("net_recovery_verified") is False:
            summary["unverified_connectivity"].append(
                {"board": name, "input_format": grade.signals.get("input_format")}
            )
        if grade.signals.get("net_identity_verified") is False:
            summary["unverified_net_identity"].append(
                {"board": name, "input_format": grade.signals.get("input_format")}
            )
        if grade.signals.get("reconstruction_floor_verified") is False:
            summary["unverified_reconstruction"].append(
                {
                    "board": name,
                    "because": grade.signals.get(
                        "reconstruction_floor_caveat", "unstated"
                    ),
                }
            )
        if grade.signals.get("firmware_expect") not in (None, "cosim"):
            summary["firmware_expectation_lowered"].append(
                {"board": name, "expect": grade.signals.get("firmware_expect")}
            )
    return summary
