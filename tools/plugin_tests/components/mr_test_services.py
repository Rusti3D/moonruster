# Phase 4 parity component: job_state, secrets, job_queue and power as
# components call them
from __future__ import annotations
import asyncio
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
        ann = lc("announcements")
        entry = ann.add_internal_announcement("mr title", "mr desc", "https://example.com", "normal", "mr_feed")
        out["ann_added"] = sorted(entry.keys())
        await asyncio.sleep(0.1)
        listed = await ann.get_announcements()
        out["ann_listed"] = [e["title"] for e in listed if e["feed"] == "mr_feed"]
        out["ann_dismiss"] = await outcome(ann.dismiss_announcement(entry["entry_id"], 3600))
        out["ann_after_dismiss"] = [len([e for e in await ann.get_announcements() if e["feed"] == "mr_feed"]),
                                    len([e for e in await ann.get_announcements(True) if e["feed"] == "mr_feed"])]
        out["ann_remove"] = await outcome(ann.remove_announcement(entry["entry_id"]))
        out["ann_remove_missing"] = await outcome(ann.remove_announcement("mr_feed/none"))
        hist = lc("history")
        out["hist"] = [await outcome(hist.get_job(12345)), await outcome(hist.get_job("00ABCD")),
                       hist.tracking_enabled(False), await outcome(hist.delete_job(12345))]
        ps = lc("proc_stats")
        seqs = []
        ps.register_stat_callback(seqs.append)
        await asyncio.sleep(2.5)
        out["stat_callbacks"] = [len(seqs) >= 1, all(isinstance(x, int) for x in seqs)]
        tmpl = lc("template")
        ui = tmpl.create_ui_template("hello {{ name }}")
        out["ui_template"] = await outcome(ui.render_async({"name": "mr"}))
        m = lc("machine")
        out["machine"] = [await outcome(m.wait_for_init(5.)), m.unit_name, m.linux_user, m.sudo_requested,
                          m.sudo_request_messages, m.is_service_allowed("moonraker"), m.is_service_allowed("nope"),
                          m.validation_enabled(), m.is_inside_container(), m.get_provider_type(),
                          type(m.public_ip).__name__, sorted(m.get_system_info().keys()),
                          sorted(m.get_moonraker_service_info().keys())]
        out["devices"] = [type((await m.detect_serial_devices())).__name__,
                          sorted((await m.detect_video_devices()).keys()),
                          type(await m.detect_usb_devices()).__name__]
        out["power"] = pw is not None
        if pw is not None:
            out["power_bad_state"] = now(lambda: pw.set_device_power("mr_dev", "maybe"))
            out["power_no_device"] = now(lambda: pw.set_device_power("mr_none", True))
        for k, v in out.items():
            log(f"{k}: {v!r}")
        return "done"


def load_component(config):
    return MrTestServices(config)
