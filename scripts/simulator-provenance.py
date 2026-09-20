#!/usr/bin/env python3
"""Forward to the crate-owned simulator provenance implementation."""

from pathlib import Path

_REAL = Path(__file__).resolve().parents[1] / "crates/hauksbee-engine/assets/scripts/simulator-provenance.py"
__file__ = str(_REAL)
exec(compile(_REAL.read_bytes(), __file__, "exec"))
