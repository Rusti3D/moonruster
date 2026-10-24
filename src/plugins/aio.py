# CPython's own asyncio event loop, driven by Moonruster's tokio loop
# (docs/PLUGINS.md "asyncio on Moonruster's loop"; proven by
# pybridge/src/aio.py). Module `_mr_aio`.
#
# The loop is a plain SelectorEventLoop (Moonraker's default loop, with
# MOONRAKER_ENABLE_UVLOOP off or uvloop absent). Only its selector differs:
# select() never blocks. Tokio waits instead, on the selector's epoll fd, the
# next timer and wakeups from Rust, then calls step() for one iteration.
import asyncio
import selectors
import sys
import threading
from asyncio import events


class _Selector(selectors.EpollSelector):
    def select(self, timeout=None):
        return super().select(0)


class Driver:
    def __init__(self):
        self.loop = loop = asyncio.SelectorEventLoop(_Selector())
        asyncio.set_event_loop(loop)
        # what run_forever() does before its first iteration
        if hasattr(loop, "_run_forever_setup"):  # 3.13+
            loop._run_forever_setup()
        else:  # 3.10-3.12: run_forever's inline setup, same steps
            loop._check_closed()
            loop._check_running()
            loop._set_coroutine_origin_tracking(loop._debug)
            loop._old_agen_hooks = sys.get_asyncgen_hooks()
            loop._thread_id = threading.get_ident()
            sys.set_asyncgen_hooks(firstiter=loop._asyncgen_firstiter_hook,
                                   finalizer=loop._asyncgen_finalizer_hook)
            events._set_running_loop(loop)

    def fileno(self):
        return self.loop._selector.fileno()

    def step(self):
        """One loop iteration. Returns the seconds until the loop needs the
        next one without new I/O: 0 when callbacks are ready, -1 when only
        I/O or a wakeup can make progress."""
        loop = self.loop
        loop._run_once()
        if loop._ready:
            return 0.0
        if loop._scheduled:
            return max(0.0, loop._scheduled[0]._when - loop.time())
        return -1.0
