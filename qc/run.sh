#!/usr/bin/env bash
# Run the scenario QC suite against a build of the binaries.
#
#   qc/run.sh                             # target/release
#   qc/run.sh --scenario 04               # one scenario
#   qc/run.sh --bin-dir /path/to/bins     # somewhere else
#
# Nothing here is built for you: the suite asserts on the behaviour of a real
# build, and silently rebuilding would hide which build was measured.
set -euo pipefail
exec python3 "$(dirname "$0")/runner.py" "$@"
