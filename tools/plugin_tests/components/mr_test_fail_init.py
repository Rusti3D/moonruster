# Phase 3 parity component: loads, then fails in component_init (Moonraker
# adds a warning "Component ... failed to load with error" and lists it in
# failed_components)
from __future__ import annotations


class MrTestFailInit:
    def __init__(self, config):
        self.server = config.get_server()

    async def component_init(self) -> None:
        raise ValueError("mr_test_fail_init: deliberate failure")


def load_component(config):
    return MrTestFailInit(config)
