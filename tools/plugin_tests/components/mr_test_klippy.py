# Phase 4 parity component: Klippy through klippy_apis, remote methods, the
# klippy events and status subscriptions (tools/plugin_cmp.py runs a batch
# Klippy behind each server)
from __future__ import annotations
import asyncio
import logging

from ..common import KlippyState, RequestType


def log(msg):
    logging.info(f"mr_test_klippy: {msg}")


async def outcome(aw):
    try:
        return ["ok", await aw]
    except Exception as e:
        return ["err", type(e).__name__, str(e), getattr(e, "status_code", None)]


class MrTestKlippy:
    def __init__(self, config):
        self.server = config.get_server()
        self.kapis = self.server.lookup_component("klippy_apis")
        self.status_seen = []
        self.server.register_remote_method("mr_test_remote", self._remote)
        self.server.register_event_handler("server:klippy_started", self._started)
        self.server.register_event_handler("server:klippy_ready", self._ready)
        self.server.register_endpoint("/server/mr_test/klippy", RequestType.POST, self._run)

    def _remote(self, **kwargs):
        log(f"remote method {sorted(kwargs.items())!r}")

    def _started(self, state):
        log(f"klippy_started {type(state).__name__} {state} {state == KlippyState.READY}")

    async def _ready(self):
        log("klippy_ready")

    def _on_status(self, status, eventtime):
        self.status_seen.append([status, type(eventtime).__name__])

    async def _run(self, web_request):
        k = self.kapis
        out = {}
        info = await outcome(k.get_klippy_info())
        out["info"] = [info[0], sorted(info[1].keys()) if info[0] == "ok" else info, info[1].get("state")]
        out["gcode"] = await outcome(k.run_gcode("M117 mr_test"))
        out["gcode_bad"] = await outcome(k.run_gcode("MR_NO_SUCH_COMMAND"))
        out["gcode_bad_default"] = await outcome(k.run_gcode("MR_NO_SUCH_COMMAND", default="dflt"))
        out["query"] = await outcome(k.query_objects({"toolhead": ["homed_axes", "max_velocity"]}))
        out["query_unknown"] = await outcome(k.query_objects({"mr_no_such_object": None}))
        objs = await outcome(k.get_object_list())
        out["objects"] = [objs[0], "toolhead" in objs[1], "webhooks" in objs[1]]
        out["subscribe"] = await outcome(k.subscribe_objects({"gcode_move": ["homing_origin"]}, self._on_status))
        out["offset"] = await outcome(k.run_gcode("SET_GCODE_OFFSET Z=0.25"))
        out["pause"] = await outcome(k.pause_print())
        out["pause_default"] = await outcome(k.pause_print(default="no print"))
        out["remote"] = await outcome(k.run_gcode("MR_TEST_REMOTE"))
        kc = self.server.lookup_component("klippy_connection")
        out["kc_state"] = [type(kc.state).__name__, str(kc.state), kc.state_message]
        out["kc_flags"] = [kc.is_connected(), kc.is_ready(), kc.is_printing()]
        out["kc_info_keys"] = sorted(kc.klippy_info.keys())
        out["kc_misc"] = [kc.missing_requirements, kc.unit_name, type(kc.path).__name__,
                          type(kc.executable).__name__, sorted(kc.peer_credentials.keys()),
                          kc.klippy_apis is self.kapis]
        out["kc_cache_keys"] = sorted(kc.get_subscription_cache().keys())
        out["kc_wait_connected"] = await outcome(kc.wait_connected())
        try:
            kc.register_remote_method("mr_late", self._remote)
            out["kc_register_late"] = ["ok"]
        except Exception as e:
            out["kc_register_late"] = ["err", type(e).__name__, str(e)]
        await asyncio.sleep(1.0)
        out["status_callbacks"] = self.status_seen
        for key, v in out.items():
            log(f"{key}: {v!r}")
        return "done"


def load_component(config):
    return MrTestKlippy(config)
