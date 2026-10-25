# Phase 4 parity component: Moonraker's SQL API (register_table, the table
# wrapper, cursors, sqlite3 rows / converters / adapters / errors, the
# provider wrapper and transactions)
from __future__ import annotations
import logging

from ..common import RequestType, SqlTableDefinition


def log(msg):
    logging.info(f"mr_test_sql: {msg}")


async def outcome(aw):
    try:
        return ["ok", await aw]
    except Exception as e:
        return ["err", type(e).__name__, str(e), getattr(e, "sqlite_errorname", None)]


def now(fn):
    try:
        return ["ok", fn()]
    except Exception as e:
        return ["err", type(e).__name__, str(e), getattr(e, "sqlite_errorname", None)]


class MrItems(SqlTableDefinition):
    name = "mr_test_items"
    version = 2
    prototype = """
        mr_test_items (
            id INTEGER PRIMARY KEY ASC,
            label TEXT NOT NULL UNIQUE,
            amount REAL,
            data pyjson,
            flag pybool,
            blob BLOB
        )
    """

    def migrate(self, last_version, db_provider):
        conn = db_provider.connection
        log(f"migrate from {last_version}: {type(db_provider).__name__} {conn.in_transaction}")
        with conn:
            conn.execute("INSERT INTO mr_test_items (label, amount) VALUES (?, ?)", ("seed", 0.5))
        db_provider.insert_item("mr_test_sql", "seeded", {"by": "migrate"})
        log(f"migrate ns: {db_provider.get_namespace_keys('mr_test_sql')} "
            f"{db_provider.get_item('mr_test_sql', 'seeded')} "
            f"{list(db_provider.iter_namespace('mr_test_sql'))} {db_provider.get_namespace_length('mr_test_sql')}")


class MrTestSql:
    def __init__(self, config):
        self.server = config.get_server()
        db = self.server.lookup_component("database")
        self.table = db.register_table(MrItems())
        self.dup = now(lambda: db.register_table(MrItems()))
        self.server.register_endpoint("/server/mr_test/sql", RequestType.POST, self._run)

    async def _run(self, web_request):
        db = self.server.lookup_component("database")
        t = self.table
        out = {"dup": self.dup, "wrapper": [type(t).__name__, t.version]}
        async with t as tx:
            cur = await tx.execute(
                "INSERT INTO mr_test_items (label, amount, data, flag, blob) VALUES (?, ?, ?, ?, ?)",
                ("one", 1 / 3, {"a": [1, 2]}, 0, b"\x00\x01"))
            out["insert"] = [type(cur).__name__, cur.rowcount, cur.lastrowid, cur.description]
            cur = await tx.executemany(
                "INSERT INTO mr_test_items (label, amount) VALUES (:label, :amount)",
                [{"label": "two", "amount": 2}, {"label": "three", "amount": 3.5}])
            out["many"] = [cur.rowcount, cur.lastrowid]
        cur = await t.execute("SELECT * FROM mr_test_items ORDER BY id")
        out["desc"] = cur.description
        rows = await cur.fetchall()
        out["rows"] = [[type(r).__name__, r.keys(), tuple(r), r["label"], r[0]] for r in rows]
        cur = await t.execute("SELECT label FROM mr_test_items WHERE amount > ?", (1,))
        out["fetchone"] = [tuple(await cur.fetchone()), [tuple(r) for r in await cur.fetchmany(5)], await cur.fetchone()]
        out["errors"] = [
            await outcome(t.execute("SELEC 1")),
            await outcome(t.execute("SELECT * FROM nope")),
            await outcome(t.execute("INSERT INTO mr_test_items (label) VALUES ('one')")),
            await outcome(t.execute("SELECT ?", (1, 2))),
            await outcome(t.execute("SELECT :x", {"y": 1})),
            await outcome(t.execute("SELECT ?", (object(),))),
            await outcome(t.execute("SELECT 1; SELECT 2")),
            await outcome(t.executemany("SELECT ?", [(1,)])),
        ]
        await t.rollback()
        out["script"] = await outcome(t.executescript(
            "CREATE TABLE IF NOT EXISTS mr_test_tmp (x); INSERT INTO mr_test_tmp VALUES (1); DROP TABLE mr_test_tmp;"))
        out["callback"] = await t.queue_callback(
            lambda conn: [conn.in_transaction, [tuple(r) for r in conn.execute("SELECT id, label FROM mr_test_items ORDER BY id")]])
        # a statement left uncommitted, then a namespace write: Moonraker's
        # provider commits the connection's transaction with its own
        await t.execute("INSERT INTO mr_test_items (label) VALUES ('pending')")
        in_tx = await t.queue_callback(lambda conn: conn.in_transaction)
        await db.insert_item("mr_test_sql", "after", 1)
        after = await t.queue_callback(lambda conn: conn.in_transaction)
        await t.rollback()
        cur = await db.sql_execute("SELECT count(*) AS n FROM mr_test_items WHERE label = 'pending'")
        out["txn"] = [in_tx, after, tuple(await cur.fetchone())]
        cur = await db.sql_execute(
            "SELECT key, value FROM namespace_store WHERE namespace = ? ORDER BY key", ("mr_test_sql",))
        out["record_converter"] = [tuple(r) for r in await cur.fetchall()]
        pw = db.get_provider_wrapper()
        # Moonraker closes the provider's sync connection when its thread starts
        out["provider_wrapper"] = [type(pw).__name__, type(pw.connection).__name__, now(lambda: pw.get_namespace("mr_test_sql"))]
        cmp = await db.compact_database()
        out["compact"] = sorted(cmp.keys())
        await db.sql_commit()
        for k, v in out.items():
            log(f"{k}: {v!r}")
        return "done"


def load_component(config):
    return MrTestSql(config)
