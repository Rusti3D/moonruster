# Phase 4 parity component: the MQTT client as components use it (publish
# payload encodings, subscriptions, request / response, errors), against
# the local mosquitto
from __future__ import annotations
import asyncio
import logging

from ..common import RequestType


def log(msg):
    logging.info(f"mr_test_mqtt: {msg}")


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


class MrTestMqtt:
    def __init__(self, config):
        self.server = config.get_server()
        self.received = []
        self.server.register_endpoint("/server/mr_test/mqtt", RequestType.POST, self._run)

    def _on_echo(self, payload):
        self.received.append(payload)

    def _on_request(self, payload):
        mqtt = self.server.lookup_component("mqtt")
        mqtt.publish_topic("mr_test/resp", {"echo": payload.decode()})

    async def _run(self, web_request):
        mqtt = self.server.lookup_component("mqtt")
        out = {}
        out["connected"] = [await mqtt.wait_connection(5.), mqtt.is_connected(), type(mqtt.get_instance_name()).__name__]
        hdl = mqtt.subscribe_topic("mr_test/echo", self._on_echo)
        out["handle"] = [type(hdl).__name__, hdl.topic]
        await asyncio.sleep(0.5)
        for p in ({"a": 1, "b": [1, 2]}, [1, "x"], True, False, 5, 2.5, "text", b"\x01raw", None):
            out[f"publish {p!r}"] = await outcome(mqtt.publish_topic("mr_test/echo", p, qos=1))
        await asyncio.sleep(1.0)
        out["received"] = self.received
        out["bad_qos"] = now(lambda: mqtt.subscribe_topic("mr_test/x", self._on_echo, 3))
        out["wildcard"] = now(lambda: mqtt.subscribe_topic("mr_test/#", self._on_echo))
        mqtt.unsubscribe(hdl)
        req = mqtt.subscribe_topic("mr_test/req", self._on_request)
        await asyncio.sleep(0.5)
        # a callback that publishes runs as the message arrives (not at the
        # loop's next timer)
        got = []
        direct = mqtt.subscribe_topic("mr_test/resp", got.append)
        await asyncio.sleep(0.5)
        mqtt.publish_topic("mr_test/req", "direct")
        await asyncio.sleep(0.3)
        out["direct_response"] = list(got)
        mqtt.unsubscribe(direct)
        out["with_response"] = await outcome(mqtt.publish_topic_with_response("mr_test/req", "mr_test/resp", "ping", timeout=5.))
        out["timeout"] = await outcome(mqtt.publish_topic_with_response("mr_test/void", "mr_test/never", "x", timeout=0.5))
        mqtt.unsubscribe(req)
        for k, v in out.items():
            log(f"{k}: {v!r}")
        return "done"


def load_component(config):
    return MrTestMqtt(config)
