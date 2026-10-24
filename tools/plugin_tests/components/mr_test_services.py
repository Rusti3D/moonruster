# Phase 4 parity component: job_state, secrets, job_queue and power as
# components call them
from __future__ import annotations
import logging

from ..common import RequestType


def log(msg):
    logging.info(f"mr_test_services: {msg}")


async def outcome(aw):
    try:
        return ["ok", await aw]
    except Exception as e:
        return ["err", type(e).__name__, str(e), getattr(e, "status_code", None)]


def now(fn):
    try:
        return ["ok", fn()]
    except Exception as e:
        return ["err", type(e).__name__, str(e)]


class MrTestServices:
    def __init__(self, config):
        self.server = config.get_server()
        self.server.register_endpoint("/server/mr_test/services", RequestType.POST, self._run)

    async def _run(self, web_request):
        lc = self.server.lookup_component
        js, sec, jq, pw = lc("job_state"), lc("secrets"), lc("job_queue"), lc("power", None)
        out = {}
        out["last_stats_keys"] = sorted(js.get_last_stats().keys())
        ev = js.get_last_job_event()
        out["last_event"] = [type(ev).__name__, str(ev)]
        out["secrets"] = [sec.get_type(), type(sec.get_secrets_file()).__name__, sec.get("nope", "dflt"),
                          now(lambda: sec["nope"])]
        out["queue_missing_file"] = await outcome(jq.queue_job("mr_no_such_file.gcode"))
        out["queue_no_check"] = await outcome(jq.queue_job(["a.gcode", "b.gcode"], check_exists=False))
        out["queue_pause"] = await outcome(jq.pause_queue())
        out["queue_delete_all"] = await outcome(jq.delete_job([], all=True))
        out["queue_start"] = await outcome(jq.start_queue())
        out["power"] = pw is not None
        if pw is not None:
            out["power_bad_state"] = now(lambda: pw.set_device_power("mr_dev", "maybe"))
            out["power_no_device"] = now(lambda: pw.set_device_power("mr_none", True))
        for k, v in out.items():
            log(f"{k}: {v!r}")
        return "done"


def load_component(config):
    return MrTestServices(config)
