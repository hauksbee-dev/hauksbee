#!/usr/bin/env bash
# Decide what the action runs: hauksbee-ci on a list of specs (mode "spec"),
# or `hauksbee run <board> --check --strict` on a board file (mode "check").
#
# Split out of action.yml so the decision can be tested, the same way
# pick-release-tag.sh is. The wrong answer available here is a guess: an
# action that silently runs something the caller did not name makes a green
# run mean nothing, so every ambiguous combination fails loudly and says what
# it found instead.
#
# Inputs come from the environment:
#   IN_MODE   auto | spec | check    (default auto)
#   IN_SPEC   a single spec path
#   IN_SPECS  a newline- or space-separated list of spec paths and/or globs
#   IN_BOARD  a board file path
#
# Results are appended to the file named by GITHUB_OUTPUT (required):
#   mode=spec|check
#   board=<path>                    (empty in spec mode)
#   specs<<HAUKSBEE_SPECS ... HAUKSBEE_SPECS   (expanded paths, one per line)
#
# Contradictory inputs, a glob that matches nothing, and a failed
# auto-detection all exit 1 with the guidance on stderr.
set -euo pipefail

MODE="${IN_MODE:-auto}"
SPEC="${IN_SPEC-}"
SPECS="${IN_SPECS-}"
BOARD="${IN_BOARD-}"
OUT="${GITHUB_OUTPUT:?GITHUB_OUTPUT must name a writable file}"

die() { printf 'hauksbee action: %s\n' "$*" >&2; exit 1; }

case "$MODE" in
  auto|spec|check) ;;
  *) die "mode must be auto, spec, or check (got '$MODE')" ;;
esac

if [ -n "$SPEC" ] && [ -n "$SPECS" ]; then
  die "both 'spec' and 'specs' were given; 'specs' already takes a list, so pick one"
fi

# One spec token per line, however they arrived: a single 'spec', or a
# 'specs' list separated by spaces or newlines.
tokens="$(printf '%s\n%s\n' "$SPEC" "$SPECS" | tr ' \t' '\n' | sed '/^$/d')"

if [ "$MODE" = "auto" ]; then
  if [ -n "$tokens" ] && [ -n "$BOARD" ]; then
    die "both a spec and a board were given; set mode: spec or mode: check to say which one runs"
  elif [ -n "$tokens" ]; then
    MODE=spec
  elif [ -n "$BOARD" ]; then
    MODE=check
  else
    # Nothing named: detect. Exactly one spec in ci/ runs as a spec; failing
    # that, exactly one board file in the repo runs as a check. Anything else
    # is ambiguous, so list what was found and stop.
    ci_specs="$(compgen -G 'ci/*.toml' || true)"
    boards="$(find . \( -name .git -o -name '.hauksbee*' \) -prune \
      -o -name '*.kicad_pcb' -print 2>/dev/null | sed 's|^\./||' | sort)"
    n_specs="$(printf '%s' "$ci_specs" | grep -c . || true)"
    n_boards="$(printf '%s' "$boards" | grep -c . || true)"
    if [ "$n_specs" -eq 1 ]; then
      MODE=spec
      tokens="$ci_specs"
    elif [ "$n_boards" -eq 1 ]; then
      MODE=check
      BOARD="$boards"
    else
      {
        echo "hauksbee action: nothing to run, and auto-detection found no single candidate."
        if [ "$n_specs" -gt 0 ]; then
          echo "  specs in ci/ ($n_specs):"
          printf '%s\n' "$ci_specs" | sed 's/^/    /'
        else
          echo "  no *.toml specs in ci/"
        fi
        if [ "$n_boards" -gt 0 ]; then
          echo "  board files ($n_boards):"
          printf '%s\n' "$boards" | sed 's/^/    /'
        else
          echo "  no *.kicad_pcb board files"
        fi
        echo "Give the action 'spec:' or 'specs:' (run hauksbee-ci specs)," \
             "or 'board:' (run hauksbee run --check --strict on the board)."
      } >&2
      exit 1
    fi
  fi
fi

if [ "$MODE" = "check" ]; then
  [ -n "$BOARD" ] || die "mode: check runs a board, so the 'board' input is required"
  [ -z "$tokens" ] || die "mode: check runs a board, not a spec; drop 'spec'/'specs' or use mode: spec"
else
  [ -n "$tokens" ] || die "mode: spec needs 'spec' or 'specs'"
  [ -z "$BOARD" ] || die "mode: spec runs specs, not a board; drop 'board' or use mode: check"
fi

# Expand any glob tokens. A pattern that matches nothing is an error, not an
# empty run: a spec list that silently shrinks to zero would pass vacuously.
expanded=""
if [ "$MODE" = "spec" ]; then
  while IFS= read -r token; do
    [ -n "$token" ] || continue
    case "$token" in
      *[\*\?\[]*)
        matches="$(compgen -G "$token" || true)"
        [ -n "$matches" ] || die "spec pattern '$token' matched no files"
        expanded="${expanded}${matches}
"
        ;;
      *)
        expanded="${expanded}${token}
"
        ;;
    esac
  done <<<"$tokens"
fi

{
  printf 'mode=%s\n' "$MODE"
  printf 'board=%s\n' "$BOARD"
  printf 'specs<<HAUKSBEE_SPECS\n'
  printf '%s' "$expanded"
  printf 'HAUKSBEE_SPECS\n'
} >>"$OUT"

if [ "$MODE" = "check" ]; then
  echo "mode: check ($BOARD)"
else
  echo "mode: spec"
  printf '%s' "$expanded" | sed 's/^/  /'
fi
