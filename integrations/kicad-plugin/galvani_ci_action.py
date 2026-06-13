"""The pcbnew ActionPlugin: run galvani-ci on the currently open board and show
the pass/fail results in a dialog.

This wrapper is intentionally thin. It finds a spec next to the open board,
shells out to galvani-ci via :mod:`galvani_ci_core`, and renders the parsed
JUnit results in a wx dialog. All simulation lives in the Rust runner.

KiCad 7+ action-plugin conventions: subclass ``pcbnew.ActionPlugin``, implement
``defaults()`` and ``Run()``, and register the instance at import time.
"""

from __future__ import annotations

import os

import pcbnew  # provided by KiCad's Python environment
import wx

from . import galvani_ci_core as core


def _spec_candidates(board_path: str):
    """Specs to offer for a given board file.

    Looks in a sibling ``ci/`` directory and next to the board, so a repo that
    keeps specs in ``ci/`` just works. A spec found here may target the layout
    (``.kicad_pcb``) or, for the same project, the schematic (``.kicad_sch``):
    running from pcbnew is the documented way to drive a schematic-stage check
    too, since eeschema has no plugin API yet (see README).
    """
    board_dir = os.path.dirname(board_path)
    ci_dir = os.path.join(board_dir, "ci")
    return core.find_specs(ci_dir, board_dir)


class GalvaniCiResultDialog(wx.Dialog):
    """A simple results dialog: a coloured headline and a monospace report."""

    def __init__(self, parent, run: core.CiRun):
        title = "galvani-ci: hardware check"
        super().__init__(parent, title=title, size=(640, 460))
        sizer = wx.BoxSizer(wx.VERTICAL)

        headline = wx.StaticText(self, label=run.summary())
        font = headline.GetFont()
        font.SetPointSize(font.GetPointSize() + 3)
        font.SetWeight(wx.FONTWEIGHT_BOLD)
        headline.SetFont(font)
        green = wx.Colour(20, 140, 40)
        red = wx.Colour(190, 30, 30)
        headline.SetForegroundColour(green if run.passed and not run.error else red)
        sizer.Add(headline, 0, wx.ALL, 12)

        text = wx.TextCtrl(
            self,
            value=core.format_report(run),
            style=wx.TE_MULTILINE | wx.TE_READONLY | wx.TE_DONTWRAP,
        )
        text.SetFont(wx.Font(wx.FontInfo(10).Family(wx.FONTFAMILY_TELETYPE)))
        sizer.Add(text, 1, wx.EXPAND | wx.LEFT | wx.RIGHT, 12)

        btns = self.CreateButtonSizer(wx.OK)
        sizer.Add(btns, 0, wx.ALL | wx.ALIGN_RIGHT, 12)
        self.SetSizer(sizer)


class GalvaniCiPlugin(pcbnew.ActionPlugin):
    def defaults(self):
        self.name = "galvani-ci: run hardware check"
        self.category = "Simulation"
        self.description = (
            "Boot the firmware on the emulated PCB and assert rails, UART, and "
            "blink against a checked-in galvani-ci spec."
        )
        self.show_toolbar_button = True
        here = os.path.dirname(__file__)
        icon = os.path.join(here, "icon.png")
        if os.path.isfile(icon):
            self.icon_file_name = icon

    def Run(self):
        board = pcbnew.GetBoard()
        board_path = board.GetFileName() if board else ""
        if not board_path:
            wx.MessageBox(
                "Save the board first so galvani-ci has a file to run on.",
                "galvani-ci",
                wx.OK | wx.ICON_WARNING,
            )
            return

        specs = _spec_candidates(board_path)
        if not specs:
            wx.MessageBox(
                "No galvani-ci spec (*.toml) found next to the board or in a "
                "sibling ci/ directory. Add one, e.g. ci/power-up.toml.",
                "galvani-ci",
                wx.OK | wx.ICON_INFORMATION,
            )
            return

        spec = specs[0]
        if len(specs) > 1:
            choice = wx.SingleChoiceDialog(
                None,
                "Choose a galvani-ci spec to run:",
                "galvani-ci",
                [os.path.relpath(s, os.path.dirname(board_path)) for s in specs],
            )
            if choice.ShowModal() != wx.ID_OK:
                return
            spec = specs[choice.GetSelection()]

        # Prefer a ready-to-run binary (PATH / prebuilt bundle / local build).
        # Only offer to compile if none is found, so users are not forced to
        # build when a prebuilt binary is already available.
        binary = core.find_binary(os.environ.get("GALVANI_CI_BIN"))
        if not binary:
            ans = wx.MessageBox(
                "No galvani-ci binary found (not on PATH, no release bundle, no "
                "local build). Build it now with cargo? This compiles the runner "
                "and may take a few minutes the first time.",
                "galvani-ci",
                wx.YES_NO | wx.ICON_QUESTION,
            )
            if ans != wx.YES:
                return
            binary = core.ensure_binary(build=True)
            if not binary:
                wx.MessageBox(
                    "Could not build galvani-ci (is cargo installed?). Install "
                    "Rust from https://rustup.rs or set GALVANI_CI_BIN to a "
                    "prebuilt binary.",
                    "galvani-ci",
                    wx.OK | wx.ICON_ERROR,
                )
                return

        # Run from the board's directory so relative spec paths resolve.
        run = core.run_ci(spec, binary=binary, cwd=os.path.dirname(board_path))
        GalvaniCiResultDialog(None, run).ShowModal()


# Register on import, per KiCad action-plugin convention.
GalvaniCiPlugin().register()
