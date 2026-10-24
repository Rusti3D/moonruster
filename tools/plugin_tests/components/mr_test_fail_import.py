# Phase 3 parity component: fails at import (Moonraker logs "Unable to load
# component" with the traceback and lists it in failed_components)
from __future__ import annotations
import mr_no_such_package  # noqa: F401


def load_component(config):
    return object()
