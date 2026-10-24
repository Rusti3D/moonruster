# Phase 3 parity component (tools/plugin_cmp.sh): loads on Python Moonraker
# and on Moonruster from the same file; everything it observes goes to the
# log, and the logs are compared.
from __future__ import annotations
import asyncio
import logging

from ..utils import ServerError
from ..common import RequestType


class MrTestBasic:
    def __init__(self, config):
        self.server = config.get_server()
        self.name = config.get_name()
        log = logging.info
        log(f"mr_test_basic: name={self.name!r}")
        log(f"str={config.get('str_opt')!r} default={config.get('missing', 'dflt')!r} "
            f"none={config.get('missing2', None)!r}")
        log(f"int={config.getint('int_opt')!r} float={config.getfloat('float_opt')!r} "
            f"bool={config.getboolean('bool_opt')!r}")
        log(f"int_default={config.getint('missing3', 7)!r} float_default={config.getfloat('missing4', 2.5)!r} "
            f"bool_default={config.getboolean('missing5', False)!r}")
        try:
            config.getint("int_opt", above=100)
        except config.error as e:
            log(f"bounds error: {e}")
        try:
            config.get("required_missing")
        except config.error as e:
            log(f"required error: {e}")
        log(f"has_option={config.has_option('str_opt')} {config.has_option('nope')}")
        log(f"options={sorted(config.get_options().items())}")
        db = self.server.lookup_component("database")
        log(f"database class={type(db).__module__}.{type(db).__qualname__}")
        log(f"lookup default={self.server.lookup_component('no_such_component', 'fallback')!r}")
        try:
            self.server.lookup_component("no_such_component")
        except ServerError as e:
            log(f"lookup error: {e} ({e.status_code})")
        args = self.server.get_app_args()
        log(f"software_version={args['software_version']}")
        log(f"RequestType.GET={RequestType.GET!r}")
        el = self.server.get_event_loop()
        log(f"eventloop running={el.is_running()} same={el.asyncio_loop is asyncio.get_event_loop()}")

    async def component_init(self) -> None:
        await asyncio.sleep(0.01)
        logging.info("mr_test_basic: component_init done")

    def on_exit(self) -> None:
        logging.info("mr_test_basic: on_exit")

    async def close(self) -> None:
        logging.info("mr_test_basic: close")


def load_component(config):
    return MrTestBasic(config)
