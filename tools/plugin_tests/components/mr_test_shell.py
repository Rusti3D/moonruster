# Phase 4 parity component: shell_command as components use it (exec_cmd,
# build_shell_command + run / run_with_response, failures, stderr logging)
from __future__ import annotations
import logging
import sys

from ..common import RequestType


def log(msg):
    logging.info(f"mr_test_shell: {msg}")


async def outcome(aw):
    try:
        return ["ok", await aw]
    except Exception as e:
        return ["err", type(e).__name__, str(e)]


class MrTestShell:
    def __init__(self, config):
        self.server = config.get_server()
        self.server.register_endpoint("/server/mr_test/shell", RequestType.POST, self._run)

    async def _run(self, web_request):
        sc = self.server.lookup_component("shell_command")
        out = {}
        out["exec_ok"] = await outcome(sc.exec_cmd("echo hello", 10.))
        out["exec_python"] = await outcome(sc.exec_cmd(f"{sys.executable} -c 'print(6*7)'", 10.))
        out["exec_fail"] = await outcome(sc.exec_cmd("sh -c 'echo oops >&2; exit 3'", 10., log_stderr=True))
        lines = []
        scmd = sc.build_shell_command("printf 'a\\nb\\n'", callback=lines.append)
        out["run"] = await outcome(scmd.run(timeout=10.))
        out["run_lines"] = [bytes(x).decode() if not isinstance(x, str) else x for x in lines]
        out["return_code"] = scmd.get_return_code()
        # tornado's AsyncHTTPClient as Moonraker configured it (http_client's
        # import-time configure)
        from tornado.httpclient import AsyncHTTPClient
        client = AsyncHTTPClient()
        out["tornado_client"] = [type(client).__name__, client.defaults.get("user_agent"),
                                 getattr(client, "max_body_size", None)]
        scmd2 = sc.build_shell_command("echo resp")
        out["run_with_response"] = await outcome(scmd2.run_with_response(timeout=10.))
        for k, v in out.items():
            log(f"{k}: {v!r}")
        return "done"


def load_component(config):
    return MrTestShell(config)
