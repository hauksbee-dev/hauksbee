#!/usr/bin/env python3
"""Generate the hostile-but-realistic edge cases the wild produces.

Every case here corresponds to something a real user actually hands a tool:
a truncated checkout, an un-pulled LFS pointer, a zip of the wrong thing, a
board saved by a future version, a file mangled by a text editor. The point is
that each must ingest or refuse loudly, never crash, hang, or lie.
"""
import gzip
import json
import os
import struct
import zipfile

ROOT = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(ROOT, "ingest-corpus", "edge")
os.makedirs(OUT, exist_ok=True)

MIN_PCB = """(kicad_pcb (version 20221018) (generator pcbnew)
  (general (thickness 1.6))
  (paper "A4")
  (layers (0 "F.Cu" signal) (31 "B.Cu" signal))
  (setup (pad_to_mask_clearance 0))
  (net 0 "")
  (net 1 "GND")
  (net 2 "VCC")
  (footprint "R_0603" (layer "F.Cu") (at 10 10)
    (property "Reference" "R1")
    (property "Value" "10k")
    (pad "1" smd rect (at -0.8 0) (size 0.9 0.95) (layers "F.Cu") (net 1 "GND"))
    (pad "2" smd rect (at 0.8 0) (size 0.9 0.95) (layers "F.Cu") (net 2 "VCC"))
  )
  (segment (start 10 10) (end 20 10) (width 0.25) (layer "F.Cu") (net 1))
)
"""


def w(name, data):
    path = os.path.join(OUT, name)
    mode = "wb" if isinstance(data, bytes) else "w"
    with open(path, mode) as fh:
        fh.write(data)
    return path


cases = {}

# ── truncation and encoding damage ───────────────────────────────────────────
cases["truncated_midtoken.kicad_pcb"] = MIN_PCB[: len(MIN_PCB) // 2]
cases["truncated_at_open_paren.kicad_pcb"] = "(kicad_pcb (version 20221018) (footprint "
cases["unbalanced_extra_close.kicad_pcb"] = MIN_PCB + ")))))\n"
cases["header_only.kicad_pcb"] = "(kicad_pcb (version 20221018) (generator pcbnew))\n"
cases["empty.kicad_pcb"] = ""
cases["whitespace_only.kicad_pcb"] = "   \n\t\n  \n"
cases["bom_prefixed.kicad_pcb"] = "﻿" + MIN_PCB
cases["crlf.kicad_pcb"] = MIN_PCB.replace("\n", "\r\n")
cases["latin1_in_refdes.kicad_pcb"] = (
    MIN_PCB.replace('"R1"', '"R\xe9sistance"').encode("latin-1")
)
cases["nul_bytes_inside.kicad_pcb"] = MIN_PCB[:200].encode() + b"\x00" * 64 + MIN_PCB[200:].encode()
cases["invalid_utf8_middle.kicad_pcb"] = (
    MIN_PCB[:300].encode() + b"\xff\xfe\xfd\x80\x81" + MIN_PCB[300:].encode()
)

# ── un-pulled / wrong file entirely ──────────────────────────────────────────
cases["lfs_pointer.PcbDoc"] = (
    "version https://git-lfs.github.com/spec/v1\n"
    "oid sha256:4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393\n"
    "size 1048576\n"
)
cases["lfs_pointer.kicad_pcb"] = cases["lfs_pointer.PcbDoc"]
cases["git_conflict_markers.kicad_pcb"] = MIN_PCB.replace(
    '(net 1 "GND")',
    '<<<<<<< HEAD\n  (net 1 "GND")\n=======\n  (net 1 "GROUND")\n>>>>>>> feature/rename\n',
    1,
)
cases["html_error_page.kicad_pcb"] = (
    "<!DOCTYPE html>\n<html><head><title>404 Not Found</title></head>\n"
    "<body><h1>Not Found</h1></body></html>\n"
)
cases["readme_instead.kicad_pcb"] = "# My Board\n\nOpen `board.kicad_pcb` in KiCad 8.\n"
cases["json_instead.kicad_pcb"] = json.dumps({"board": "nope", "layers": 4}, indent=2)
cases["random_binary.kicad_pcb"] = bytes(range(256)) * 40
cases["png_renamed.kicad_pcb"] = (
    b"\x89PNG\r\n\x1a\n" + struct.pack(">I", 13) + b"IHDR" + b"\x00" * 13 + b"\x00" * 200
)
cases["ole2_not_altium.PcbDoc"] = b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1" + b"\x00" * 4096
cases["kicad_pro_not_board.kicad_pro"] = json.dumps({"board": {"design_settings": {}}})

# ── future / past format versions ────────────────────────────────────────────
cases["future_version.kicad_pcb"] = MIN_PCB.replace("20221018", "20991231")
cases["version_not_a_number.kicad_pcb"] = MIN_PCB.replace("(version 20221018)", '(version "eight")')
cases["missing_version.kicad_pcb"] = MIN_PCB.replace("(version 20221018) ", "")
cases["kicad4_legacy.kicad_pcb"] = (
    "(kicad_pcb (version 4) (host pcbnew 4.0.7)\n"
    "  (general (links 0) (no_connects 0) (area 0 0 0 0) (thickness 1.6)\n"
    "    (drawings 0) (tracks 0) (zones 0) (modules 1) (nets 2))\n"
    "  (layers (0 F.Cu signal) (31 B.Cu signal))\n"
    "  (net 0 \"\")\n  (net 1 GND)\n"
    "  (module R_0603 (layer F.Cu) (tedit 0) (at 10 10)\n"
    "    (fp_text reference R1 (at 0 0) (layer F.SilkS))\n"
    "    (fp_text value 10k (at 0 1) (layer F.Fab))\n"
    "    (pad 1 smd rect (at -0.8 0) (size 0.9 0.95) (layers F.Cu) (net 1 GND))\n"
    "  )\n)\n"
)

# ── numeric hostility ────────────────────────────────────────────────────────
cases["nan_coords.kicad_pcb"] = MIN_PCB.replace("(at 10 10)", "(at nan nan)")
cases["inf_coords.kicad_pcb"] = MIN_PCB.replace("(at 10 10)", "(at inf -inf)")
cases["exp_overflow_coords.kicad_pcb"] = MIN_PCB.replace("(at 10 10)", "(at 1e400 -1e400)")
cases["huge_coords.kicad_pcb"] = MIN_PCB.replace(
    "(at 10 10)", "(at 179769313486231570000000000 1e300)"
)
cases["negative_size_pad.kicad_pcb"] = MIN_PCB.replace("(size 0.9 0.95)", "(size -0.9 -0.95)")
cases["zero_width_track.kicad_pcb"] = MIN_PCB.replace("(width 0.25)", "(width 0)")
cases["net_id_overflow.kicad_pcb"] = MIN_PCB.replace(
    '(net 1 "GND")', '(net 99999999999999999999 "GND")'
)
cases["negative_net_id.kicad_pcb"] = MIN_PCB.replace('(net 1 "GND")', '(net -5 "GND")')
cases["pad_references_missing_net.kicad_pcb"] = MIN_PCB.replace(
    '(net 2 "VCC"))\n  )', '(net 4242 "GHOST"))\n  )'
)
cases["duplicate_net_ids.kicad_pcb"] = MIN_PCB.replace(
    '(net 2 "VCC")', '(net 1 "VCC")'
)
cases["value_not_a_magnitude.kicad_pcb"] = MIN_PCB.replace(
    '"Value" "10k"', '"Value" "DNP - see BOM tab 3"'
)
cases["value_unicode_ohm.kicad_pcb"] = MIN_PCB.replace('"Value" "10k"', '"Value" "4Ω7"')
cases["value_looks_numeric_but_is_pn.kicad_pcb"] = MIN_PCB.replace(
    '"Value" "10k"', '"Value" "2N3904"'
)

# ── structural depth / size (stack and memory) ───────────────────────────────
cases["deep_nesting_20k.kicad_pcb"] = (
    "(kicad_pcb (version 20221018) " + "(x " * 20000 + ")" * 20000 + ")"
)
cases["deep_nesting_200k.kicad_pcb"] = (
    "(kicad_pcb (version 20221018) " + "(x " * 200000 + ")" * 200000 + ")"
)
cases["very_long_single_line.kicad_pcb"] = (
    "(kicad_pcb (version 20221018) (net 1 \"" + "A" * 4_000_000 + "\"))"
)
_many = ['(kicad_pcb (version 20221018) (net 0 "")']
for i in range(1, 20001):
    _many.append(f'  (net {i} "N{i}")')
_many.append(")")
cases["20k_nets.kicad_pcb"] = "\n".join(_many)

# ── Eagle XML hostility ──────────────────────────────────────────────────────
EAGLE_MIN = """<?xml version="1.0" encoding="utf-8"?>
<!DOCTYPE eagle SYSTEM "eagle.dtd">
<eagle version="9.6.2">
<drawing><board>
 <plain/>
 <libraries><library name="rcl"><packages><package name="R0603">
   <smd name="1" x="-0.8" y="0" dx="0.9" dy="0.95" layer="1"/>
   <smd name="2" x="0.8" y="0" dx="0.9" dy="0.95" layer="1"/>
 </package></packages></library></libraries>
 <elements><element name="R1" library="rcl" package="R0603" value="10k" x="10" y="10"/></elements>
 <signals>
  <signal name="GND"><contactref element="R1" pad="1"/></signal>
  <signal name="VCC"><contactref element="R1" pad="2"/></signal>
 </signals>
</board></drawing></eagle>
"""
cases["eagle_min.brd"] = EAGLE_MIN
cases["eagle_truncated.brd"] = EAGLE_MIN[: len(EAGLE_MIN) // 2]
cases["eagle_billion_laughs.brd"] = (
    '<?xml version="1.0"?>\n<!DOCTYPE eagle [\n'
    '  <!ENTITY a "aaaaaaaaaa">\n'
    '  <!ENTITY b "&a;&a;&a;&a;&a;&a;&a;&a;&a;&a;">\n'
    '  <!ENTITY c "&b;&b;&b;&b;&b;&b;&b;&b;&b;&b;">\n'
    '  <!ENTITY d "&c;&c;&c;&c;&c;&c;&c;&c;&c;&c;">\n'
    '  <!ENTITY e "&d;&d;&d;&d;&d;&d;&d;&d;&d;&d;">\n'
    '  <!ENTITY f "&e;&e;&e;&e;&e;&e;&e;&e;&e;&e;">\n'
    ']>\n<eagle version="9.0"><drawing><board><elements>'
    '<element name="&f;" library="x" package="y" value="z" x="0" y="0"/>'
    "</elements></board></drawing></eagle>\n"
)
cases["eagle_xxe_file_read.brd"] = (
    '<?xml version="1.0"?>\n<!DOCTYPE eagle [\n'
    '  <!ENTITY xxe SYSTEM "file:///etc/passwd">\n]>\n'
    '<eagle version="9.0"><drawing><board><elements>'
    '<element name="&xxe;" library="x" package="y" value="z" x="0" y="0"/>'
    "</elements></board></drawing></eagle>\n"
)
cases["eagle_deep_nesting.brd"] = (
    '<?xml version="1.0"?>\n<eagle version="9.0">'
    + "<g>" * 50000 + "</g>" * 50000 + "</eagle>\n"
)
cases["eagle_nan_coords.brd"] = EAGLE_MIN.replace('x="10" y="10"', 'x="NaN" y="Infinity"')
cases["eagle_missing_element_name.brd"] = EAGLE_MIN.replace('name="R1" ', "")
cases["eagle_v5_binary_marker.brd"] = b"\x10\x80\x00\x00EAGLE" + b"\x00" * 2048

# ── IPC-D-356 hostility ──────────────────────────────────────────────────────
IPC_OK = (
    "C  IPC-D-356A test file\n"
    "P  JOB TESTBOARD\n"
    "P  UNITS CUST 0\n"
    "317GND              R1    -1    D0472PA00X+019000Y+029450X0945Y0945R180S0\n"
    "317VCC              R1    -2    D0472PA00X+021000Y+029450X0945Y0945R180S0\n"
    "999\n"
)
cases["ipc_ok.d356"] = IPC_OK
cases["ipc_short_lines.d356"] = "C hdr\n317GND\n317\n327X\n"
cases["ipc_no_records.d356"] = "C only a comment\nP JOB X\n999\n"
cases["ipc_bad_numbers.d356"] = IPC_OK.replace("X+019000", "X+ABCDEF")
cases["ipc_huge_coords.d356"] = IPC_OK.replace("X+019000", "X+9999999")
cases["ipc_tabs_not_columns.d356"] = "C hdr\n317\tGND\tR1\t-1\tD0472PA00\n"
cases["ipc_crlf.d356"] = IPC_OK.replace("\n", "\r\n")
cases["ipc_latin1.d356"] = IPC_OK.replace("GND", "GN\xd0").encode("latin-1")

# ── gerber / excellon hostility ──────────────────────────────────────────────
GERBER_TOP = (
    "G04 Layer: F.Cu*\n%FSLAX46Y46*%\n%MOMM*%\n%LPD*%\n"
    "%ADD10C,0.500000*%\nD10*\nX10000000Y10000000D03*\nX20000000Y10000000D03*\n"
    "X10000000Y10000000D02*\nX20000000Y10000000D01*\nM02*\n"
)
GERBER_BOT = GERBER_TOP.replace("F.Cu", "B.Cu")
DRILL = "M48\nMETRIC,TZ\nT1C0.300\n%\nG90\nT1\nX10.0Y10.0\nM30\n"


def make_gerber_zip(name, files, nested=None):
    path = os.path.join(OUT, name)
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as z:
        for fn, body in files.items():
            z.writestr(os.path.join(nested, fn) if nested else fn, body)
    return path


make_gerber_zip("gerber_ok.zip", {
    "board-F_Cu.gbr": GERBER_TOP, "board-B_Cu.gbr": GERBER_BOT,
    "board.drl": DRILL, "board-F_Mask.gbr": GERBER_TOP.replace("F.Cu", "F.Mask"),
})
make_gerber_zip("gerber_nested_folder.zip", {
    "board-F_Cu.gbr": GERBER_TOP, "board-B_Cu.gbr": GERBER_BOT, "board.drl": DRILL,
}, nested="fab/gerbers")
make_gerber_zip("gerber_no_drill.zip", {
    "board-F_Cu.gbr": GERBER_TOP, "board-B_Cu.gbr": GERBER_BOT,
})
make_gerber_zip("gerber_no_copper.zip", {
    "board-F_SilkS.gbr": GERBER_TOP.replace("F.Cu", "F.SilkS"),
    "board-Edge_Cuts.gbr": GERBER_TOP.replace("F.Cu", "Edge.Cuts"),
})
make_gerber_zip("gerber_empty_archive.zip", {})
make_gerber_zip("gerber_only_readme.zip", {"README.txt": "gerbers coming soon\n"})
make_gerber_zip("firmware_project.zip", {
    "src/main.c": "int main(void){return 0;}\n",
    "Makefile": "all:\n\tgcc main.c\n",
    "platformio.ini": "[env:uno]\nplatform = atmelavr\n",
})
make_gerber_zip("zip_slip_paths.zip", {
    "../../../../etc/passwd": "root:x:0:0\n",
    "board-F_Cu.gbr": GERBER_TOP,
    "board.drl": DRILL,
})
make_gerber_zip("gerber_absolute_paths.zip", {
    "/tmp/evil-F_Cu.gbr": GERBER_TOP, "/tmp/evil.drl": DRILL,
})
# 20k tiny entries: a directory-walk / allocation stress, not a bomb.
make_gerber_zip("zip_20k_entries.zip", {f"f{i:05d}.gbr": "G04*\nM02*\n" for i in range(20000)})
# A single entry that decompresses to 200 MB (classic zip bomb shape).
_bomb = os.path.join(OUT, "zip_bomb_200mb.zip")
with zipfile.ZipFile(_bomb, "w", zipfile.ZIP_DEFLATED) as z:
    z.writestr("board-F_Cu.gbr", "G04 " + ("0" * (200 * 1024 * 1024)))
# A zip inside a zip.
_inner = os.path.join(OUT, "gerber_ok.zip")
with zipfile.ZipFile(os.path.join(OUT, "zip_inside_zip.zip"), "w") as z:
    z.write(_inner, "gerbers.zip")
# Truncated zip: a half-finished download.
with open(os.path.join(OUT, "gerber_ok.zip"), "rb") as fh:
    _z = fh.read()
w("zip_truncated.zip", _z[: len(_z) // 2])
w("gzip_not_zip.gbr.gz", gzip.compress(GERBER_TOP.encode()))

cases["gerber_lone_layer.gbr"] = GERBER_TOP
cases["gerber_step_repeat_bomb.gbr"] = (
    "%FSLAX46Y46*%\n%MOMM*%\n%SRX20000Y20000I0.01J0.01*%\n"
    "%ADD10C,0.100*%\nD10*\nX0Y0D03*\n%SR*%\nM02*\n"
)
cases["gerber_huge_aperture_macro.gbr"] = (
    "%FSLAX46Y46*%\n%MOMM*%\n%AMBIG*"
    + "".join(f"1,1,{i%9+1},0,0*" for i in range(20000))
    + "%\n%ADD10BIG*%\nD10*\nX0Y0D03*\nM02*\n"
)
cases["gerber_no_format_spec.gbr"] = "G04 no FS*\nD10*\nX10Y10D03*\nM02*\n"
cases["gerber_bad_coord_digits.gbr"] = GERBER_TOP.replace("X10000000", "X!!!!!!!!")
cases["gerber_unterminated_macro.gbr"] = "%FSLAX46Y46*%\n%MOMM*%\n%AMX*1,1,1,0,0"
cases["excellon_no_tools.drl"] = "M48\nMETRIC,TZ\n%\nG90\nX10.0Y10.0\nM30\n"
cases["excellon_huge_repeat.drl"] = "M48\nMETRIC\nT1C0.3\n%\nT1\nR999999999X0.001Y0.001\nM30\n"

# ── Protel / Altium ASCII ────────────────────────────────────────────────────
cases["protel_schematic_not_board.pcbdoc"] = (
    "|RECORD=Sheet|KIND=Protel_Schematic|X=1\n|RECORD=Wire|POINTS=2\n"
)
cases["protel_board_min.pcbdoc"] = (
    "|RECORD=Board|KIND=Protel_Advanced_PCB|VERSION=5.00\n"
    "|RECORD=Net|ID=0|NAME=GND\n"
    "|RECORD=Component|ID=0|LAYER=TOP|X=0mil|Y=0mil|ROTATION=0|PATTERN=R0603|SOURCEDESIGNATOR=R1\n"
    "|RECORD=Pad|COMPONENT=0|NET=0|LAYER=TOP|NAME=1|X=0mil|Y=0mil\n"
)
cases["protel_truncated_record.pcbdoc"] = (
    "|RECORD=Board|KIND=Protel_Advanced_PCB|VERSION=5.00\n|RECORD=Compon"
)
cases["protel_bad_units.pcbdoc"] = cases["protel_board_min.pcbdoc"].replace("0mil", "NaNmil")

# ── KiCad netlist / schematic ────────────────────────────────────────────────
cases["netlist_no_components.net"] = '(export (version "E") (components) (nets))\n'
cases["netlist_truncated.net"] = '(export (version "E") (components (comp (ref "R1")'
cases["sch_no_symbols.kicad_sch"] = "(kicad_sch (version 20230121) (generator eeschema))\n"
cases["sch_self_referencing_sheet.kicad_sch"] = (
    "(kicad_sch (version 20230121) (generator eeschema)\n"
    '  (sheet (at 0 0) (size 10 10)\n'
    '    (property "Sheetname" "self")\n'
    '    (property "Sheetfile" "sch_self_referencing_sheet.kicad_sch")\n'
    "  )\n)\n"
)
cases["sch_missing_subsheet.kicad_sch"] = (
    "(kicad_sch (version 20230121) (generator eeschema)\n"
    '  (sheet (at 0 0) (size 10 10)\n'
    '    (property "Sheetname" "gone")\n'
    '    (property "Sheetfile" "definitely_not_here.kicad_sch")\n'
    "  )\n)\n"
)

for name, body in cases.items():
    w(name, body)

print(f"wrote {len(os.listdir(OUT))} edge cases to {OUT}")
