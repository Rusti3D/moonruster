# Phase 4 parity component: endpoints registered by a component, answered
# over HTTP and JSON-RPC (tools/plugin_cmp.py calls them on both servers)
from __future__ import annotations
import logging

from ..common import RequestType


class MrTestEndpoints:
    def __init__(self, config):
        self.server = config.get_server()
        reg = self.server.register_endpoint
        reg("/server/mr_test/info", RequestType.GET, self._info)
        reg("/server/mr_test/args", ["GET", "POST"], self._args)
        reg("/server/mr_test/raw", RequestType.GET, self._raw, wrap_result=False, content_type="text/plain")
        reg("/server/mr_test/notfound", RequestType.GET, self._notfound)
        reg("/server/mr_test/valueerror", RequestType.GET, self._valueerror)
        reg("/server/mr_test/typeerror", RequestType.GET, self._typeerror)
        reg("/server/mr_test/notasync", RequestType.GET, self._notasync)
        reg("/server/mr_test/none", RequestType.POST, self._none)
        reg("/server/mr_test/removable", RequestType.GET, self._info)
        reg("/server/mr_test/app", RequestType.POST, self._app)
        try:
            reg("/server/mr_test/info", RequestType.GET, self._info)
        except self.server.error as e:
            logging.info(f"mr_test_endpoints: duplicate: {e} ({e.status_code})")

    async def _info(self, web_request):
        t = web_request.get_subscribable()
        return {
            "endpoint": web_request.get_endpoint(),
            "request_type": str(web_request.get_request_type()),
            "action": web_request.get_action(),
            "transport": None if t is None else type(t).__qualname__,
            "ip": str(web_request.get_ip_address()),
            "user": None if web_request.get_current_user() is None else web_request.get_current_user().username,
            "args": web_request.get_args(),
        }

    async def _app(self, web_request):
        app = self.server.lookup_component("application")
        app.remove_endpoint("/server/mr_test/removable")
        app.register_static_file_handler("/server/mr_test/static", __file__)
        return {
            "route_prefix": app.route_prefix,
            "parse": app.parse_endpoint("/server/mr_test/x"),
            "https": app.https_enabled(),
            "server": app.get_server() is self.server,
        }

    async def _args(self, web_request):
        return {
            "s": web_request.get_str("s", "dflt"),
            "i": web_request.get_int("i", 0),
            "f": web_request.get_float("f", 0.5),
            "b": web_request.get_boolean("b", False),
            "l": web_request.get_list("l", []),
            "missing": web_request.get("nope", None),
        }

    async def _raw(self, web_request):
        return "plain text body"

    async def _notfound(self, web_request):
        raise self.server.error("mr_test: no such thing", 404)

    async def _valueerror(self, web_request):
        raise ValueError("mr_test: bad value")

    async def _typeerror(self, web_request):
        raise TypeError("mr_test: wrong arguments")

    def _notasync(self, web_request):
        return {"sync": True}

    async def _none(self, web_request):
        return None


def load_component(config):
    return MrTestEndpoints(config)
