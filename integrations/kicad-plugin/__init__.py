"""hauksbee-ci KiCad action plugin.

KiCad imports this package from its plugins directory and expects the action
plugin to register itself on import. The pcbnew/wx wrapper lives in
``hauksbee_ci_action``; the pcbnew-free core logic lives in ``hauksbee_ci_core``
so it can be unit-tested with plain python.

The registration import is gated on pcbnew being importable and on this file
being loaded as a real package, so tooling outside KiCad (pytest collecting
this directory, linters) can import it as a no-op. Inside KiCad both hold and
the plugin registers exactly as before.
"""

import importlib.util

if __package__ and importlib.util.find_spec("pcbnew"):
    from . import hauksbee_ci_action  # noqa: F401  (registers the plugin on import)
