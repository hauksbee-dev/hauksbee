#!/usr/bin/env python3
"""Positive and hostile resolution checks for the history-campaign-core pack.

The boards are the exact pinned campaign source files, not synthetic boards.
This test checks identity resolution only; it does not turn an invalid campaign
run into a green result.
"""

from __future__ import annotations

import json
import os
import subprocess
from pathlib import Path


ROOT = Path(__file__).resolve().parents[3]
CAMPAIGN = ROOT.parent / "external-bug-hunts" / "hauksbee-blinded-history-campaign"
BIN = Path(os.environ.get("HAUKSBEE_BIN", str(ROOT / "target" / "debug" / "hauksbee")))


def resolve(board: Path) -> dict:
    result = subprocess.run(
        [str(BIN), "models", "resolve", str(board), "--json"],
        check=True,
        capture_output=True,
        text=True,
    )
    return json.loads(result.stdout)


def rows(doc: dict) -> dict[tuple[str, str], dict]:
    return {(row["ref"], row["value"]): row for row in doc["components"]}


def test_positive_exact_matches() -> None:
    # `input-fix.kicad_pcb` is the byte-pinned after artifact recorded by the
    # frozen campaign (the source checkout is retained separately for context).
    pedal = resolve(
        CAMPAIGN / "cases/pedalboard_usb_footprints/input-fix.kicad_pcb"
    )
    pedal_rows = rows(pedal)
    expected = {
        ("U1", "74AHCT1G32SE-7"): "74ahct1g32_history",
        ("U2", "TLP2761"): "tlp2761_history_identity",
        ("U5", "NCP1117-3.3_SOT223"): "ncp1117_3v3_history",
        ("U6", "AP64501SP-13"): "ap64501_history_identity",
        ("U7", "74AHCT1G32SE-7"): "74ahct1g32_history",
    }
    for key, model in expected.items():
        assert pedal_rows[key]["model"] == model, (key, pedal_rows[key])
        assert pedal_rows[key]["layer"] == "pack(10)", (key, pedal_rows[key])

    fpv_rows = rows(
        resolve(CAMPAIGN / "cases/fpv_drone_controller/input-fix.kicad_pcb")
    )
    assert fpv_rows[("U3", "BMP280")]["model"] == "bmp280_history_identity"
    assert fpv_rows[("U3", "BMP280")]["layer"] == "pack(10)"

    bms_rows = rows(
        resolve(CAMPAIGN / "cases/libresolar_bms_c1/input-fix.kicad_pcb")
    )
    assert bms_rows[("U1", "SN65HVD230")]["model"] == "sn65hvd230_history_identity"
    assert bms_rows[("U1", "SN65HVD230")]["layer"] == "pack(10)"


def test_hostile_near_match_stays_unresolved() -> None:
    """SN65HVD75 must not inherit the exact SN65HVD230 pin map."""

    bms_rows = rows(
        resolve(CAMPAIGN / "cases/libresolar_bms_c1/input-fix.kicad_pcb")
    )
    hostile = bms_rows[("U2", "SN65HVD75")]
    assert hostile["resolved"] is False, hostile
    assert hostile["model"] == "UNRESOLVED", hostile


if __name__ == "__main__":
    test_positive_exact_matches()
    test_hostile_near_match_stays_unresolved()
    print("history-campaign-core resolution tests: 2 passed")
