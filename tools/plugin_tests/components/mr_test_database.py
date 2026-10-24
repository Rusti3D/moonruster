# Phase 4 parity component: the database as components use it, before the
# server runs (operations run in place) and after (futures)
from __future__ import annotations
import logging

from ..common import RequestType


def log(msg):
    logging.info(f"mr_test_database: {msg}")


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


class MrTestDatabase:
    def __init__(self, config):
        self.server = config.get_server()
        db = self.db = self.server.lookup_component("database")
        self.ns = db.register_local_namespace("mr_test_db")
        fut = self.ns.insert("init_key", {"a": 1})
        log(f"init insert done {fut.done()} {fut.result()!r}")
        log(f"init as_dict {self.ns.as_dict()!r}")
        log(f"init get {db.get_item('mr_test_db', 'init_key.a').result()!r}")
        log(f"init missing {now(lambda: db.get_item('mr_test_db', 'nope'))!r}")
        log(f"init contains {now(lambda: 'init_key' in self.ns)!r}")
        log(f"init wrap unknown {now(lambda: db.wrap_namespace('mr_no_such_ns'))!r}")
        log(f"init register twice {now(lambda: db.register_local_namespace('mr_test_db'))!r}")
        log(f"init encode set {now(lambda: db.insert_item('mr_test_db', 'bad', {1, 2}))!r}")
        log(f"provider alive {db.db_provider.is_alive()} path {type(db.database_path).__name__}")
        self.server.register_endpoint("/server/mr_test/db", RequestType.POST, self._run)

    async def _run(self, web_request):
        db, ns = self.db, self.ns
        out = {}
        fut = ns.insert("k1", {"x": {"y": 1}, "l": [1, 2]})
        out["done_at_once"] = fut.done()
        out["insert"] = await outcome(fut)
        out["nested_insert"] = await outcome(db.insert_item("mr_test_db", "k1.x.z", "zz"))
        out["get"] = await outcome(db.get_item("mr_test_db", "k1"))
        out["get_nested"] = await outcome(db.get_item("mr_test_db", ["k1", "x", "y"]))
        out["get_missing"] = await outcome(db.get_item("mr_test_db", "k1.nope"))
        out["get_default"] = await outcome(db.get_item("mr_test_db", "nope", default=[7]))
        out["update_merge"] = await outcome(db.update_item("mr_test_db", "k1.x", {"w": 2}))
        out["update_replace"] = await outcome(db.update_item("mr_test_db", "k1.l", "now a str"))
        out["update_missing"] = await outcome(db.update_item("mr_test_db", "nope", 1))
        out["update_not_dict"] = await outcome(db.update_item("mr_test_db", "k1.q.r", 1))
        out["after_update"] = await outcome(ns.get("k1"))
        out["insert_scalars"] = [await outcome(ns.insert(k, v)) for k, v in
                                 (("i", 5), ("f", 1.25), ("b", True), ("s", "str"), ("n", None))]
        out["encode_tuple"] = await outcome(ns.insert("t", (1, 2)))
        out["encode_nested_tuple"] = await outcome(ns.insert("t2", {"a": (1, 2)}))
        out["keys"] = await outcome(ns.keys())
        out["values"] = await outcome(ns.values())
        out["items"] = await outcome(ns.items())
        out["length"] = await outcome(ns.length())
        out["contains"] = await outcome(ns.contains("k1"))
        out["as_dict"] = now(lambda: ns.as_dict())
        out["batch_insert"] = await outcome(ns.insert_batch({"b1": 1, "b2": [2], "b3": {"c": 3}}))
        out["batch_get"] = await outcome(ns.get_batch(["b1", "b3", "nope"]))
        out["batch_move"] = await outcome(ns.move_batch(["b1"], ["b1m"]))
        out["batch_delete"] = await outcome(ns.delete_batch(["b1m", "b2"]))
        out["delete_nested"] = await outcome(ns.delete(["k1", "x", "y"]))
        out["delete_missing"] = await outcome(ns.delete("nope"))
        out["pop"] = await outcome(ns.pop("b3"))
        out["pop_default"] = await outcome(ns.pop("nope", "dflt"))
        out["getitem"] = await outcome(ns["s"])
        out["final"] = await outcome(db.get_item("mr_test_db"))
        out["clear"] = await outcome(ns.clear())
        out["after_clear"] = await outcome(ns.length())
        for k, v in out.items():
            log(f"{k}: {v!r}")
        return "done"


def load_component(config):
    return MrTestDatabase(config)
