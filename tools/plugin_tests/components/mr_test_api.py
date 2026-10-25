# Phase 4 parity component: the rest of the component API (template
# globals, authorization, http_client extras ...), values and errors
from __future__ import annotations
import base64
import ipaddress
import logging
import pathlib
import shutil

from ..common import RequestType, WebRequest, APITransport
from .power import PowerDevice


def log(msg):
    logging.info(f"mr_test_api: {msg}")


async def outcome(aw):
    try:
        return ["ok", await aw]
    except Exception as e:
        return ["err", type(e).__name__, str(e), getattr(e, "status_code", None)]


def now(fn):
    try:
        return ["ok", fn()]
    except Exception as e:
        return ["err", type(e).__name__, str(e), getattr(e, "status_code", None)]


class MrTransport(APITransport):
    def __init__(self):
        self.statuses = []

    def send_status(self, status, eventtime):
        self.statuses.append(status)


class MrDevice(PowerDevice):
    """a component's power device: Moonraker's PowerDevice base class"""

    def __init__(self, config, fail=False):
        super().__init__(config)
        self.fail = fail

    def init_state(self):
        self.state = "off"

    def initialize(self):
        if self.fail:
            return False
        return super().initialize()

    def refresh_status(self):
        pass

    def set_power(self, state):
        self.state = state


class MrTestApi:
    def __init__(self, config):
        self.server = config.get_server()
        self.config = config
        tf = self.server.lookup_component("template")
        tf.add_environment_global("mr_value", {"a": [1, 2]})
        tf.add_environment_global("mr_double", lambda x, y=1: x * 2 + y)
        self.dup = now(lambda: tf.add_environment_global("mr_value", 1))
        self.reserved = now(lambda: tf.add_environment_global("range", 1))
        self.server.register_endpoint("/server/mr_test/api", RequestType.POST, self._run)

    async def _run(self, web_request):
        lc = self.server.lookup_component
        out = {"dup_global": self.dup, "reserved_global": self.reserved}
        tf = lc("template")
        t = tf.create_template("{mr_value.a[1]} {mr_double(3)} {mr_double(3, y=5)}")
        out["global_sync"] = now(lambda: t.render())
        ta = tf.create_template("{mr_double(4)}", is_async=True)
        out["global_async"] = await outcome(ta.render_async())
        out["ui_has_no_globals"] = await outcome(tf.create_ui_template("{{ mr_value is defined }}").render_async())

        au = lc("authorization")
        key = au.get_api_key()
        out["api_key"] = [type(key).__name__, len(key or ""), au.is_api_key_enabled(), au.cors_enabled()]
        u = au.validate_api_key(key)
        out["validate_api_key"] = [u.username, u.source]
        out["validate_bad_key"] = now(lambda: au.validate_api_key("nope"))
        def b64(t):
            return base64.urlsafe_b64encode(t.encode()).decode().rstrip("=")
        bad = ["a.b.c", "abc", "a.b.c.d", "..", "é.a.b", "ab=c.abcd.abcd", "a=bc.abcd.abcd",
               b64("not json") + ".x.abcd", b64("[1]") + ".x.abcd", b64('"s"') + ".x.abcd",
               b64('{"typ":"JWT"}') + ".x.abcd", b64('{"typ":"JWT","alg":"EdDSA","kid":[1]}') + ".x.abcd",
               b64('{"typ":"JWT","alg":"EdDSA","kid":"nokey"}') + ".x.abcd"]
        out["decode_bad"] = [now(lambda t=t: au.decode_jwt(t)) for t in bad]
        out["validate_jwt_bad"] = now(lambda: au.validate_jwt("x.y.z"))
        tok = au.get_oneshot_token(ipaddress.ip_address("127.0.0.1"), None)
        out["oneshot"] = [type(tok).__name__, len(tok)]
        out["logins_maxed"] = au.check_logins_maxed(ipaddress.ip_address("10.1.2.3"))
        out["cors"] = [await au.check_cors(None), await au.check_cors("http://example.com")]

        hc = lc("http_client")
        out["cached_url_none"] = now(lambda: hc.register_cached_url("http://example.invalid/x"))
        out["cached_url"] = now(lambda: hc.register_cached_url("http://example.invalid/x", etag="abc"))
        out["gh_stats"] = sorted(hc.github_api_stats().keys())
        port = self.server.get_host_info()["port"]
        url = f"http://127.0.0.1:{port}/server/mr_test/static"
        seen = []
        res = await outcome(hc.download_file(url, "text/plain", progress_callback=lambda *a: seen.append(len(a))))
        if res[0] == "ok":
            p = pathlib.Path(res[1])
            out["download"] = [type(res[1]).__name__, p.read_text()[:40], all(n == 3 for n in seen)]
            p.unlink()
        else:
            out["download"] = res
        got = await outcome(hc.get_file(url, "text/plain"))
        out["get_file"] = [got[0], type(got[1]).__name__, got[1][:20] if got[0] == "ok" else got]
        wrapped = hc.wrap_request(url)
        resp = await wrapped.send()
        out["wrap_request"] = [type(wrapped).__name__, resp.status_code, wrapped.last_response() is resp]
        await self._file_manager(out)
        await self._components(out)
        for k, v in out.items():
            log(f"{k}: {v!r}")
        return "done"

    async def _components(self, out):
        lc = self.server.lookup_component
        oc = lc("octoprint_compat")
        out["octo"] = [oc.printer_state(), oc.printer_temps()]
        out["proc_stats_log"] = lc("proc_stats").log_last_stats(2)
        wl = lc("wled")
        out["wled"] = [
            await outcome(wl.wled_on("nostrip", 1)),
            await outcome(wl.set_wled_state("nostrip", state="on")),
            await outcome(wl.set_wled_state("mr_strip")),
            await outcome(wl.set_wled("nostrip", red=1.0)),
        ]
        um = lc("update_manager")
        ups = um.get_updaters()
        # known difference: Moonruster's own "moonraker" updater is a
        # BaseDeploy (it is not a Python source tree)
        out["updaters"] = sorted((n, type(u).__name__ if n != "moonraker" else "-", u.name == n) for n, u in ups.items())
        out["updater_same"] = all(ups[n] is u for n, u in um.get_updaters().items())
        mr = ups.get("klipper")
        if mr is not None:
            out["updater_api"] = [type(mr.needs_refresh()).__name__, type(mr.get_last_refresh_time()).__name__,
                                  sorted(mr.get_persistent_data().keys()), sorted(mr.get_update_status().keys())]
            out["updater_log_exc"] = [type(mr.log_exc("mr test", traceback=False)).__name__, mr.log_info("mr info")]
        out["register_updater_bad"] = now(lambda: um.register_updater("moonraker", {"type": "web"}))
        out["register_updater_type"] = now(lambda: um.register_updater("mr_up", {"type": "nope"}))
        out["refresh_missing"] = await outcome(um.refresh_updater("nope"))
        kc = lc("klippy_connection")
        wr = WebRequest("objects/query", {"objects": {"webhooks": ["state"]}}, RequestType.GET)
        res = await outcome(kc.request(wr))
        out["kc_request"] = [res[0], res[1]["status"] if res[0] == "ok" else res]
        out["kc_request_bad"] = await outcome(kc.request(WebRequest("nope/x", {})))
        out["kc_rollover"] = await outcome(kc.rollover_log())
        tr = MrTransport()
        ka = lc("klippy_apis")
        out["sub_transport"] = await outcome(ka.subscribe_from_transport({"webhooks": ["state"]}, tr))
        out["sub_transport_full"] = sorted((await ka.subscribe_from_transport({"webhooks": None}, tr, full_response=True)).keys())
        out["remove_sub"] = kc.remove_subscription(tr)
        wr = WebRequest("objects/subscribe", {"objects": {"webhooks": ["state"]}}, RequestType.POST, transport=tr)
        res = await outcome(kc.request(wr))
        out["kc_request_sub"] = [res[0], res[1]["status"] if res[0] == "ok" else res]
        out["kc_request_nosub"] = await outcome(kc.request(WebRequest("objects/subscribe", {"objects": {}}, RequestType.POST)))
        kc.remove_subscription(tr)
        for name, part in (("server_config", self._server_config), ("power_analysis", self._power_analysis),
                           ("services", self._services)):
            try:
                await part(out)
            except Exception as e:
                out[f"{name}_error"] = [type(e).__name__, str(e)]

    async def _server_config(self, out):
        srv = self.server
        out["app_arg"] = [type(srv.get_app_arg("data_path")).__name__, now(lambda: srv.get_app_arg("nope")),
                          srv.get_app_arg("nope", 5)]
        out["api_version"] = srv.get_api_version()
        out["debug"] = [srv.is_debug_enabled(), srv.is_verbose_enabled()]
        wid = srv.add_warning("mr_test warning one", "mr_w1")
        anon = srv.add_warning("mr_test warning two", log=False)
        out["warnings"] = [wid, type(anon).__name__, "mr_test warning one" in srv.get_warnings(),
                           "mr_test warning two" in srv.get_warnings()]
        srv.remove_warning("mr_w1")
        srv.remove_warning(anon)
        out["warnings_removed"] = [w for w in srv.get_warnings() if w.startswith("mr_test")]
        out["rollover"] = srv.add_log_rollover_item("mr_test_item", "mr_test rollover line")
        out["klippy_info"] = sorted(srv.get_klippy_info().keys())
        cfg = self.config
        out["cfg_hash"] = cfg.get_hash().hexdigest()
        out["cfg_file"] = [type(cfg.get_file()).__name__, cfg.get_file().name]
        out["cfg_files"] = len(cfg.get_config_files())
        out["cfg_file_sections"] = "mr_test_api" in list(cfg.get_file_sections().values())[0]
        sup = cfg.read_supplemental_dict({"mr_sup one": {"a": "1", "b": "x"}})
        out["supplemental"] = [sup.get_name(), sup.getint("a"), sup.get("b"), sup.get_file(), sup.get_hash().hexdigest()]
        out["supplemental_empty"] = now(lambda: cfg.read_supplemental_dict({}))
        sup_file = pathlib.Path(srv.get_app_arg("data_path")) / "mr_sup.cfg"
        sup_file.write_text("[mr_file sec]\nkey: 7\n")
        fc = cfg.read_supplemental_config(str(sup_file))
        out["supplemental_file"] = [fc.get_name(), fc.getint("key"), fc.get_file().name]
        sup_file.write_text("")
        out["supplemental_file_empty"] = now(lambda: cfg.read_supplemental_config(str(sup_file)))
        sup_file.unlink()
        out["parsed"] = sorted(cfg.get_parsed_config().get("mr_test_api", {}).keys())
        out["orig"] = sorted(cfg.get_orig_config().keys())[:3]

    async def _services(self, out):
        lc = self.server.lookup_component
        srv = self.server
        comps = ["power", "wled", "notifier", "history", "announcements", "machine", "update_manager", "job_queue"]
        out["attr_server"] = [n for n in comps if getattr(lc(n), "server", None) is not srv]
        out["attr_loop"] = [getattr(lc(n), a) is srv.get_event_loop() for n, a in
                            (("announcements", "eventloop"), ("file_manager", "event_loop"), ("klippy_connection", "event_loop"))]
        out["attr_refs"] = [lc("history").file_manager is lc("file_manager"), lc("octoprint_compat").klippy_apis is lc("klippy_apis"),
                            srv.klippy_connection is lc("klippy_connection"), srv.moonraker_app is lc("application"),
                            type(srv.port).__name__, srv.host, srv.debug, srv.server_running]
        fm = lc("file_manager")
        out["attr_data"] = [sorted(fm.file_paths.keys()), sorted(fm.full_access_roots), sorted(lc("job_state").last_print_stats.keys()),
                            type(lc("klippy_connection").subscription_cache).__name__]
        mq = lc("mqtt")
        out["mqtt_transport"] = [str(mq.transport_type), mq.send_status({}, 0.)]
        args = {"mqtt_timestamp": 12.5, "x": 1}
        out["mqtt_screen"] = [now(lambda: mq.screen_rpc_request(None, None, args)), args,
                              now(lambda: mq.screen_rpc_request(None, None, {"mqtt_timestamp": 12.5}))]
        nt = lc("notifier")
        out["notify_missing"] = await outcome(nt.notify_action("nope"))
        out["notify"] = await outcome(nt.notify_action("mr_n", "hello"))
        db = lc("dbus_manager")
        out["dbus"] = db.is_connected()
        intf = await outcome(db.get_interface("org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus"))
        out["dbus_intf"] = [intf[0], type(intf[1]).__name__ if intf[0] == "ok" else intf]
        if intf[0] == "ok":
            out["dbus_call"] = (await intf[1].call_get_name_owner("org.freedesktop.DBus")).startswith(":")
        ifs = await outcome(db.get_interfaces("org.freedesktop.DBus", "/org/freedesktop/DBus",
                                              ["org.freedesktop.DBus", "org.freedesktop.DBus.Introspectable"]))
        out["dbus_intfs"] = [ifs[0], len(ifs[1]) if ifs[0] == "ok" else ifs]

    async def _power_analysis(self, out):
        lc = self.server.lookup_component
        pw = lc("power")
        cfg = self.config.read_supplemental_dict({"power mr_py": {"type": "mr", "on_when_job_queued": "True"}})
        dev = MrDevice(cfg)
        out["power_add"] = await outcome(pw.add_device("mr_py", dev))
        out["power_dup"] = await outcome(pw.add_device("mr_py", dev))
        out["power_on"] = [await dev.process_request("on"), dev.state, dev.get_device_info()]
        out["power_set"] = pw.set_device_power("mr_py", "off")
        bad = MrDevice(self.config.read_supplemental_dict({"power mr_bad": {"type": "mr"}}), fail=True)
        out["power_bad"] = [await outcome(pw.add_device("mr_bad", bad)), [w for w in self.server.get_warnings() if "mr_bad" in w]]
        an = lc("analysis")
        out["est_version"] = an.estimator_version_tuple
        # outside the watched roots: the file watcher's timing is not under test
        tmp = pathlib.Path(self.server.get_app_arg("data_path")) / "mr_est_tmp"
        tmp.mkdir(exist_ok=True)
        gc = tmp / "mr_est.gcode"
        gc.write_text("G1 X1\n; Processed by klipper_estimator v3.7.3, a\n")
        est = gc.with_suffix(".json")
        est.write_text("{}")
        out["est_missing_cfg"] = await outcome(an.estimate_file(gc))
        out["est_missing_gc"] = await outcome(an.estimate_file(gc.with_name("nope.gcode"), est))
        out["est_unavailable"] = await outcome(an.estimate_file(gc, est))
        out["post_processed"] = await outcome(an.post_process_file(gc, est))
        out["post_force"] = await outcome(an.post_process_file(gc, est, force=True))
        gc.unlink()
        est.unlink()

    async def _file_manager(self, out):
        fm = self.server.lookup_component("file_manager")
        gc = pathlib.Path(fm.get_directory("gcodes"))
        sub = gc / "mr_sub"
        sub.mkdir(exist_ok=True)
        (gc / "mr_api_a.gcode").write_text("G1 X1\n")
        (sub / "b.gcode").write_text("G1\n")
        (sub / "c.txt").write_text("x")
        out["fm_fixed"] = sorted(fm.get_fixed_path_args().keys())
        out["fm_queue"] = fm.upload_queue_enabled()
        out["fm_write"] = now(fm.check_write_enabled)
        info = fm.get_path_info(str(gc / "mr_api_a.gcode"), "gcodes")
        out["fm_info"] = [info["size"], info["permissions"], type(info["modified"]).__name__]
        out["fm_info_missing"] = now(lambda: fm.get_path_info(gc / "nope.gcode", "gcodes"))
        out["fm_info_noraise"] = fm.get_path_info(gc / "nope.gcode", "gcodes", raise_error=False)
        out["fm_list"] = sorted(fm.get_file_list("gcodes").keys())
        out["fm_list_fmt"] = [d["path"] for d in fm.get_file_list("gcodes", list_format=True)]
        out["fm_list_bad"] = now(lambda: fm.get_file_list("nope"))
        out["fm_meta"] = fm.get_file_metadata("/gcodes/nonexist.gcode")
        out["fm_list_dir"] = now(lambda: fm.list_dir("gcodes/mr_sub", simple_format=True))
        ld = fm.list_dir("/gcodes/mr_sub")
        out["fm_list_dir_full"] = [sorted(f["filename"] for f in ld["files"]), sorted(ld.keys())]
        out["fm_list_dir_bad"] = now(lambda: fm.list_dir("nope/x"))
        out["fm_list_dir_missing"] = now(lambda: fm.list_dir("gcodes/nodir"))
        out["fm_add_res"] = [fm.add_reserved_path("mr_res", sub, read_access=False), fm.add_reserved_path("mr_res", sub)]
        out["fm_check_res"] = [
            now(lambda: fm.check_reserved_path(str(sub / "b.gcode"), False)),
            fm.check_reserved_path(sub / "b.gcode", False, raise_error=False),
            fm.check_reserved_path(gc / "mr_api_a.gcode", True),
            now(lambda: fm.check_reserved_path(str(gc / ".git/x"), False)),
        ]
        out["fm_info_res"] = fm.get_path_info(sub / "b.gcode", "gcodes")["permissions"]
        out["fm_delete"] = await outcome(fm.delete_file("gcodes/mr_api_a.gcode"))
        out["fm_delete_missing"] = await outcome(fm.delete_file("gcodes/mr_api_a.gcode"))
        out["fm_delete_reserved"] = await outcome(fm.delete_file("gcodes/mr_sub/b.gcode"))
        out["fm_cancel"] = fm.cancel_notification("nokey")
        dp = fm.register_data_folder("mr_data", full_access=True)
        out["fm_data"] = [type(dp).__name__, dp.name, "mr_data" in fm.get_registered_dirs()]
        t = pathlib.Path(fm.gen_temp_upload_path())
        out["fm_tmp"] = [t.name.startswith("moonraker.upload-"), t.suffix]
        t.write_text("[mr]\n")
        up = await outcome(fm.finalize_upload({"tmp_file_path": str(t), "filename": "mr_up.cfg", "root": "config"}))
        if up[0] == "ok":
            up[1]["item"]["modified"] = type(up[1]["item"]["modified"]).__name__
        out["fm_upload"] = up
        out["fm_upload_bad"] = await outcome(fm.finalize_upload({"tmp_file_path": str(t), "root": "config"}))
        out["fm_upload_del"] = await outcome(fm.delete_file("config/mr_up.cfg"))
        out["fm_validate"] = [fm.validate_gcode_path("/nonexistent/gcodes"), fm.validate_gcode_path(str(gc))]
        shutil.rmtree(sub)
        fm.disable_write_access()
        out["fm_write_disabled"] = now(fm.check_write_enabled)


def load_component(config):
    return MrTestApi(config)
