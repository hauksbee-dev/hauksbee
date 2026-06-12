"""galvani-ci KiCad action plugin.

KiCad imports this package from its plugins directory and expects the action
plugin to register itself on import. The pcbnew/wx wrapper lives in
``galvani_ci_action``; the pcbnew-free core logic lives in ``galvani_ci_core``
so it can be unit-tested with plain python.
"""

from . import galvani_ci_action  # noqa: F401  (registers the plugin on import)
