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
got_specs() { sed -n '/^specs<<HAUKSBEE_SPECS$/,/^HAUKSBEE_SPECS$/p' "$out" | sed '1d;$d'; }

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
touch "$one_spec/ci/power-up.toml"

one_board="$work/one_board"
mkdir -p "$one_board/hardware"
touch "$one_board/hardware/board.kicad_pcb"

two_specs="$work/two_specs"
mkdir -p "$two_specs/ci"
touch "$two_specs/ci/a.toml" "$two_specs/ci/b.toml"

specs_and_board="$work/specs_and_board"
mkdir -p "$specs_and_board/ci"
touch "$specs_and_board/ci/a.toml" "$specs_and_board/ci/b.toml"
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

run_resolve "$one_board" auto "" "" ""
expect_rc "one board (no ci/ spec) auto-detects" 0
expect "  ...as mode check" "check" "$(got_mode)"
expect "  ...naming that board" "hardware/board.kicad_pcb" "$(got_board)"

run_resolve "$specs_and_board" auto "" "" ""
expect_rc "several ci/ specs fall through to the single board" 0
expect "  ...as mode check" "check" "$(got_mode)"

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
