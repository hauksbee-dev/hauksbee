#!/usr/bin/env bash
# Tests for resolve-inputs.sh. Run with:
#
#     bash integrations/github-action/test_resolve_inputs.sh
#
# The cases that matter are the ambiguous ones: given inputs that could mean
# two different runs (or none), the resolver must fail loudly rather than
# guess, because a green run of the wrong thing tells the caller nothing.
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
RESOLVE="$HERE/resolve-inputs.sh"
fails=0
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# run_resolve <dir> <mode> <spec> <specs> <board>
# Leaves the exit code in rc and the GITHUB_OUTPUT file path in out.
run_resolve() {
  local dir="$1" mode="$2" spec="$3" specs="$4" board="$5"
  out="$work/out.$$.$RANDOM"
  : >"$out"
  (
    cd "$dir" &&
      IN_MODE="$mode" IN_SPEC="$spec" IN_SPECS="$specs" IN_BOARD="$board" \
        GITHUB_OUTPUT="$out" bash "$RESOLVE" >/dev/null 2>>"$work/stderr.log"
  )
  rc=$?
}

got_mode() { grep '^mode=' "$out" | head -n1 | cut -d= -f2-; }
got_board() { grep '^board=' "$out" | head -n1 | cut -d= -f2-; }
got_specs() {
  local delimiter
  delimiter="$(sed -n 's/^specs<<//p' "$out" | head -n1)"
  awk -v start="specs<<$delimiter" -v stop="$delimiter" \
    '$0 == start { inside=1; next } inside && $0 == stop { exit } inside' "$out"
}
count_output_key() {
  local key="$1"
  awk -v key="$key" '
    inside && $0 == delimiter { inside=0; next }
    inside { next }
    /^.+<<.+$/ { split($0, fields, "<<"); delimiter=fields[2]; inside=1; next }
    index($0, key "=") == 1 { count++ }
    END { print count + 0 }
  ' "$out"
}

pass() { printf 'ok   %s\n' "$1"; }
fail() {
  printf 'FAIL %s\n       want: %s\n       got:  %s\n' "$1" "$2" "$3"
  fails=$((fails + 1))
}

expect() {
  local desc="$1" want="$2" got="$3"
  if [ "$got" = "$want" ]; then pass "$desc"; else fail "$desc" "$want" "$got"; fi
}

expect_rc() {
  local desc="$1" want="$2"
  if [ "$rc" -eq "$want" ]; then pass "$desc"; else fail "$desc" "exit $want" "exit $rc"; fi
}

# Fixture repos.
one_spec="$work/one_spec"
mkdir -p "$one_spec/ci"
printf 'board = "../board.kicad_pcb"\n' >"$one_spec/ci/power-up.toml"

root_spec="$work/root_spec"
mkdir -p "$root_spec"
printf 'board = "board.kicad_pcb"\n' >"$root_spec/power-up.toml"
touch "$root_spec/Cargo.toml" "$root_spec/board.kicad_pcb"

one_board="$work/one_board"
mkdir -p "$one_board/hardware"
touch "$one_board/hardware/board.kicad_pcb"

board_as_code="$work/board_as_code"
mkdir -p "$board_as_code/hardware"
touch "$board_as_code/hardware/blinky.board"

fab_archive="$work/fab_archive"
mkdir -p "$fab_archive/fab"
touch "$fab_archive/fab/release.tar.gz"

ipc2581="$work/ipc2581"
mkdir -p "$ipc2581/manufacturing"
printf '%s\n' '<?xml version="1.0"?><IPC-2581 revision="C"><Ecad/></IPC-2581>' \
  >"$ipc2581/manufacturing/board.XML"

generic_xml="$work/generic_xml"
mkdir -p "$generic_xml"
printf '%s\n' '<project><name>not a board</name></project>' >"$generic_xml/pom.xml"

board_and_generic_xml="$work/board_and_generic_xml"
mkdir -p "$board_and_generic_xml/hardware"
touch "$board_and_generic_xml/hardware/board.kicad_pcb"
printf '%s\n' '<coverage line-rate="1.0"/>' >"$board_and_generic_xml/coverage.xml"

two_specs="$work/two_specs"
mkdir -p "$two_specs/ci"
printf 'board = "../board.kicad_pcb"\n' >"$two_specs/ci/a.toml"
printf 'board = "../board.kicad_pcb"\n' >"$two_specs/ci/b.toml"

specs_and_board="$work/specs_and_board"
mkdir -p "$specs_and_board/ci"
printf 'board = "../board.kicad_pcb"\n' >"$specs_and_board/ci/a.toml"
printf 'board = "../board.kicad_pcb"\n' >"$specs_and_board/ci/b.toml"
touch "$specs_and_board/board.kicad_pcb"

two_boards="$work/two_boards"
mkdir -p "$two_boards"
touch "$two_boards/a.kicad_pcb" "$two_boards/b.kicad_pcb"

empty="$work/empty"
mkdir -p "$empty"

# --- auto-detection ----------------------------------------------------------
run_resolve "$one_spec" auto "" "" ""
expect_rc "one spec in ci/ auto-detects" 0
expect "  ...as mode spec" "spec" "$(got_mode)"
expect "  ...naming that spec" "ci/power-up.toml" "$(got_specs)"

run_resolve "$root_spec" auto "" "" ""
expect_rc "one spec in repo root auto-detects before a board" 0
expect "  ...root spec runs as mode spec" "spec" "$(got_mode)"
expect "  ...naming the root spec" "power-up.toml" "$(got_specs)"

run_resolve "$one_board" auto "" "" ""
expect_rc "one board (no ci/ spec) auto-detects" 0
expect "  ...as mode check" "check" "$(got_mode)"
expect "  ...naming that board" "hardware/board.kicad_pcb" "$(got_board)"

run_resolve "$board_as_code" auto "" "" ""
expect_rc "one Board-as-Code file auto-detects" 0
expect "  ...as mode check" "check" "$(got_mode)"
expect "  ...naming that board" "hardware/blinky.board" "$(got_board)"

run_resolve "$fab_archive" auto "" "" ""
expect_rc "one ODB++ or fab archive auto-detects" 0
expect "  ...archive runs as mode check" "check" "$(got_mode)"
expect "  ...naming the archive" "fab/release.tar.gz" "$(got_board)"

run_resolve "$ipc2581" auto "" "" ""
expect_rc "one IPC-2581 XML file auto-detects case-insensitively" 0
expect "  ...XML runs as mode check" "check" "$(got_mode)"
expect "  ...naming the XML" "manufacturing/board.XML" "$(got_board)"

run_resolve "$generic_xml" auto "" "" ""
expect_rc "an unrelated XML document is not guessed to be a board" 1

run_resolve "$board_and_generic_xml" auto "" "" ""
expect_rc "unrelated XML does not make a real board ambiguous" 0
expect "  ...the actual board is selected" "hardware/board.kicad_pcb" "$(got_board)"

run_resolve "$specs_and_board" auto "" "" ""
expect_rc "several specs never silently fall through to a board" 1

run_resolve "$two_specs" auto "" "" ""
expect_rc "several specs and no board is ambiguous: fail" 1

run_resolve "$two_boards" auto "" "" ""
expect_rc "several boards and no spec is ambiguous: fail" 1

run_resolve "$empty" auto "" "" ""
expect_rc "nothing at all to detect: fail" 1

# --- explicit inputs ---------------------------------------------------------
run_resolve "$empty" auto "x.toml" "" ""
expect_rc "an explicit spec needs no detection" 0
expect "  ...mode spec, passed through" "spec" "$(got_mode)"
expect "  ...spec passed through verbatim" "x.toml" "$(got_specs)"

run_resolve "$empty" spec $'foo\nHAUKSBEE_SPECS\nmode=check' "" ""
expect_rc "a multiline spec input cannot inject a GitHub output record" 0
expect "  ...only the resolver owns the mode output" "1" "$(count_output_key mode)"

run_resolve "$empty" check "" "" $'board.kicad_pcb\nmode=spec'
expect_rc "a multiline board path is refused" 1

run_resolve "$empty" auto "" "" "b.kicad_pcb"
expect_rc "an explicit board needs no detection" 0
expect "  ...mode check" "check" "$(got_mode)"

run_resolve "$two_specs" auto "" "ci/a.toml ci/b.toml" ""
expect_rc "a space-separated specs list is accepted" 0
expect "  ...both specs listed" "ci/a.toml
ci/b.toml" "$(got_specs)"

run_resolve "$two_specs" auto "" "ci/*.toml" ""
expect_rc "a glob in specs expands" 0
expect "  ...to every match" "ci/a.toml
ci/b.toml" "$(got_specs)"

run_resolve "$two_specs" auto "" "ci/missing-*.toml" ""
expect_rc "a glob matching nothing is an error, not an empty run" 1

# --- contradictory inputs ----------------------------------------------------
run_resolve "$empty" auto "a.toml" "b.toml" ""
expect_rc "spec and specs together: fail" 1

run_resolve "$empty" auto "a.toml" "" "b.kicad_pcb"
expect_rc "spec and board under mode auto: fail" 1

run_resolve "$empty" check "" "" ""
expect_rc "mode check without a board: fail" 1

run_resolve "$empty" check "a.toml" "" "b.kicad_pcb"
expect_rc "mode check with a spec: fail" 1

run_resolve "$empty" spec "" "" ""
expect_rc "mode spec without a spec: fail" 1

run_resolve "$empty" spec "a.toml" "" "b.kicad_pcb"
expect_rc "mode spec with a board: fail" 1

run_resolve "$empty" nonsense "" "" ""
expect_rc "an unknown mode: fail" 1

printf '\n'
if [ "$fails" -eq 0 ]; then
echo "all resolve-inputs tests passed"
else
  echo "$fails failed"
  exit 1
fi
