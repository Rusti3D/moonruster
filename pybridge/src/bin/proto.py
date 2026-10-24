# Checks run by proto.rs on the tokio-driven asyncio loop. Each test is a
# coroutine; it returns a detail string or raises (failure). Rust awaits each
# one with a timeout.
import asyncio
import os
import signal
import socket
import threading
import time

import _mr_native as n

LOOP = None  # set by proto.rs: the driver's loop


async def sleep_accuracy():
    worst = 0.0
    for _ in range(5):
        t = time.monotonic()
        await asyncio.sleep(0.05)
        dt = time.monotonic() - t
        assert 0.05 <= dt < 0.065, f"sleep(0.05) took {dt:.4f}s"
        worst = max(worst, dt)
    return f"worst {worst * 1000:.2f} ms"


async def tokio_not_blocked():
    t0 = n.ticks()
    await asyncio.sleep(0.2)
    d = n.ticks() - t0
    assert d >= 30, f"tokio ticker ran {d} times in 0.2 s (5 ms period)"
    return f"{d} ticks"


async def gather_tasks():
    t = time.monotonic()
    r = await asyncio.gather(*(asyncio.create_task(asyncio.sleep(0.1, i)) for i in range(3)))
    dt = time.monotonic() - t
    assert r == [0, 1, 2] and dt < 0.15, (r, dt)
    return f"{dt * 1000:.1f} ms"


async def wait_for_timeout():
    t = time.monotonic()
    try:
        await asyncio.wait_for(asyncio.sleep(1), 0.05)
    except asyncio.TimeoutError:
        dt = time.monotonic() - t
        assert dt < 0.07, dt
        return f"TimeoutError after {dt * 1000:.1f} ms"
    raise AssertionError("no timeout")


async def cancel_task():
    task = asyncio.create_task(asyncio.sleep(10))
    await asyncio.sleep(0)
    task.cancel()
    try:
        await task
    except asyncio.CancelledError:
        return "CancelledError"
    raise AssertionError("not cancelled")


async def running_loop():
    assert asyncio.get_running_loop() is LOOP
    assert asyncio.get_event_loop() is LOOP
    return type(LOOP).__name__


async def tornado_ioloop():
    from tornado.ioloop import IOLoop
    io = IOLoop.current()
    assert io.asyncio_loop is LOOP, (io.asyncio_loop, LOOP)
    fut = LOOP.create_future()
    io.call_later(0.02, fut.set_result, "fired")
    return await fut


async def tornado_http():
    import tornado.web
    from tornado.httpclient import AsyncHTTPClient
    from tornado.httpserver import HTTPServer

    class H(tornado.web.RequestHandler):
        def get(self):
            self.write("ok")

    srv = HTTPServer(tornado.web.Application([(r"/", H)]))
    sock = socket.socket()
    sock.bind(("127.0.0.1", 0))
    sock.listen(8)
    sock.setblocking(False)
    srv.add_sockets([sock])
    port = sock.getsockname()[1]
    try:
        r = await AsyncHTTPClient().fetch(f"http://127.0.0.1:{port}/")
        assert r.body == b"ok", r.body
        return f"HTTP {r.code} {r.body!r}"
    finally:
        srv.stop()


async def asyncio_sockets():
    async def echo(reader, writer):
        writer.write(await reader.readline())
        await writer.drain()
        writer.close()

    srv = await asyncio.start_server(echo, "127.0.0.1", 0)
    port = srv.sockets[0].getsockname()[1]
    reader, writer = await asyncio.open_connection("127.0.0.1", port)
    writer.write(b"hello\n")
    line = await reader.readline()
    writer.close()
    srv.close()
    await srv.wait_closed()
    assert line == b"hello\n", line
    return "echo ok"


async def rust_future():
    t = time.monotonic()
    v = await n.rust_sleep(0.05, 7)
    dt = time.monotonic() - t
    assert v == 7 and 0.05 <= dt < 0.065, (v, dt)
    return f"{v} after {dt * 1000:.1f} ms"


async def rust_future_error():
    try:
        await n.rust_fail(0.01, "boom")
    except RuntimeError as e:
        assert str(e) == "boom"
        return "RuntimeError('boom')"
    raise AssertionError("no error")


async def executor():
    t = time.monotonic()
    await LOOP.run_in_executor(None, time.sleep, 0.05)
    dt = time.monotonic() - t
    assert dt < 0.065, dt
    return f"{dt * 1000:.1f} ms"


async def threadsafe_wakeup():
    fut = LOOP.create_future()

    def worker():
        time.sleep(0.05)
        LOOP.call_soon_threadsafe(fut.set_result, time.monotonic())

    threading.Thread(target=worker).start()
    sent = await fut
    lat = time.monotonic() - sent
    assert lat < 0.005, f"wakeup latency {lat * 1000:.2f} ms"
    return f"latency {lat * 1000:.3f} ms"


async def thread_gil():
    count = [0]
    stop = threading.Event()

    def spin():
        while not stop.is_set():
            count[0] += 1

    th = threading.Thread(target=spin)
    th.start()
    a = count[0]
    time.sleep(0.1)  # reference: Python itself releases the GIL here
    ref = count[0] - a
    a = count[0]
    await asyncio.sleep(0.1)  # loop thread parked in tokio: GIL released
    idle = count[0] - a
    busy = n.spin(100, count)  # loop thread busy in Rust with the GIL held
    stop.set()
    th.join()
    assert idle >= ref * 0.5, f"thread ran {idle} while the loop was idle, {ref} while Python slept"
    assert busy == 0, f"thread ran while Rust held the GIL ({busy})"
    return f"idle +{idle} (reference +{ref}), busy +{busy}"


async def subprocess_exit():
    p = await asyncio.create_subprocess_exec("sh", "-c", "sleep 0.05; exit 3")
    rc = await p.wait()
    assert rc == 3, rc
    return f"exit {rc}"


async def signal_handler():
    fut = LOOP.create_future()
    LOOP.add_signal_handler(signal.SIGUSR1, fut.set_result, "SIGUSR1")
    os.kill(os.getpid(), signal.SIGUSR1)
    try:
        return await asyncio.wait_for(fut, 1)
    finally:
        LOOP.remove_signal_handler(signal.SIGUSR1)


async def call_cost():
    k = 100000
    t = time.perf_counter()
    for _ in range(k):
        n.noop()
    dt = time.perf_counter() - t

    def f():
        pass
    t = time.perf_counter()
    for _ in range(k):
        f()
    pf = time.perf_counter() - t
    return f"python->rust {dt / k * 1e6:.3f} us/call (python->python {pf / k * 1e6:.3f})"


TESTS = [sleep_accuracy, tokio_not_blocked, gather_tasks, wait_for_timeout, cancel_task,
         running_loop, tornado_ioloop, tornado_http, asyncio_sockets, rust_future,
         rust_future_error, executor, threadsafe_wakeup, thread_gil, subprocess_exit,
         signal_handler, call_cost]
