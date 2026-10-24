# Phase 4 parity component: events a component handles and sends, and
# notifications it registers (tools/plugin_cmp.py drives them on both
# servers over HTTP and a websocket)
from __future__ import annotations
import asyncio
import logging

from ..common import BaseRemoteConnection, RequestType


class Marker:
    pass


class MrTestEvents:
    def __init__(self, config):
        self.server = config.get_server()
        self.marker = Marker()
        self.added = None
        reg = self.server.register_event_handler
        reg("mr_test:ev", self._ev_sync)
        reg("mr_test:ev", self._ev_async)
        reg("mr_test:ev", self._ev_raise)
        reg("mr_test:ev", self._ev_async_raise)
        reg("mr_test:obj", self._ev_obj)
        reg("websockets:client_added", self._client_added)
        reg("websockets:client_identified", self._client_identified)
        reg("websockets:client_removed", self._client_removed)
        reg("file_manager:filelist_changed", self._filelist_changed)
        self.server.register_notification("mr_test:notify")
        self.server.register_notification("mr_test:notify2", "mr_test_named")
        self.server.register_endpoint("/server/mr_test/fire", RequestType.POST, self._fire)

    async def _fire(self, web_request):
        fut = self.server.send_event("mr_test:ev", {"a": 1}, [1, "two"])
        logging.info(f"mr_test_events: send_event returned {type(fut).__name__} {isinstance(fut, asyncio.Future)}")
        ret = await fut
        logging.info(f"mr_test_events: send_event done: {ret!r}")
        await self.server.send_event("mr_test:obj", self.marker)
        await self.server.send_event("mr_test:none")
        self.server.send_event("mr_test:notify", {"n": 1}, "x")
        self.server.send_event("mr_test:notify2", [3])
        return "ok"

    def _ev_sync(self, d, l):
        logging.info(f"mr_test_events: sync {d!r} {l!r}")

    async def _ev_async(self, d, l):
        await asyncio.sleep(0)
        logging.info(f"mr_test_events: async {d!r} {l!r}")

    def _ev_raise(self, d, l):
        raise ValueError("mr_test: sync handler failed")

    async def _ev_async_raise(self, d, l):
        raise RuntimeError("mr_test: async handler failed")

    def _ev_obj(self, obj):
        logging.info(f"mr_test_events: obj is marker {obj is self.marker}")

    def _client_added(self, sc):
        self.added = sc
        logging.info(f"mr_test_events: client_added {type(sc).__name__} {isinstance(sc, BaseRemoteConnection)}")

    def _client_identified(self, sc):
        logging.info(f"mr_test_events: client_identified {type(sc).__name__} same {sc is self.added}")

    def _client_removed(self, sc):
        logging.info(f"mr_test_events: client_removed {type(sc).__name__} same {sc is self.added}")

    def _filelist_changed(self, info):
        item = info.get("item", {})
        logging.info(f"mr_test_events: filelist_changed {info.get('action')} {item.get('root')} {item.get('path')}")


def load_component(config):
    return MrTestEvents(config)
