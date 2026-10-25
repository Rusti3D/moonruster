# Moonruster component layer: the sqlite3 connection of Moonraker's database
# for Python components (module `_mr_sql`; docs/PLUGINS.md "SQL").
#
# Moonruster has one SQLite connection, in Rust. A second SQLite library in
# the process must not open the same file (POSIX locks are per process:
# sqlite.org/howtocorrupt.html 2.2.1), so this Connection runs every
# statement on Rust's connection, the way Moonraker's sqlite3 connection
# (detect_types=PARSE_DECLTYPES, row_factory=sqlite3.Row, legacy transaction
# control) does:
#  - parameters are adapted and bound by sqlite3 itself: the statement's
#    placeholders as `SELECT ?, :name ...` on an in-memory connection, whose
#    row is the values to bind (the errors and warnings are sqlite3's own)
#  - columns are converted with sqlite3.converters by declared type, rows are
#    real sqlite3.Row objects
#  - DML opens a transaction when none is open; commit / rollback end it
# Moonruster sets the natives: _prepare, _exec, _script, _autocommit,
# _total_changes, _alive
import sqlite3

_mem = sqlite3.connect(":memory:")
_row_cursors = {}

# CPython's _pysqlite_seterror: the exception of an SQLite result code
_ERRORS = {
    "SQLITE_INTERNAL": "InternalError", "SQLITE_NOTFOUND": "InternalError",
    "SQLITE_CORRUPT": "DatabaseError", "SQLITE_TOOBIG": "DataError",
    "SQLITE_CONSTRAINT": "IntegrityError", "SQLITE_MISMATCH": "IntegrityError",
    "SQLITE_MISUSE": "InterfaceError", "SQLITE_RANGE": "InterfaceError",
}
_OPERATIONAL = (
    "SQLITE_ERROR", "SQLITE_PERM", "SQLITE_ABORT", "SQLITE_BUSY", "SQLITE_LOCKED",
    "SQLITE_READONLY", "SQLITE_INTERRUPT", "SQLITE_IOERR", "SQLITE_FULL",
    "SQLITE_CANTOPEN", "SQLITE_PROTOCOL", "SQLITE_EMPTY", "SQLITE_SCHEMA",
)
_PRIMARY = (
    "SQLITE_ERROR", "SQLITE_INTERNAL", "SQLITE_PERM", "SQLITE_ABORT", "SQLITE_BUSY",
    "SQLITE_LOCKED", "SQLITE_NOMEM", "SQLITE_READONLY", "SQLITE_INTERRUPT",
    "SQLITE_IOERR", "SQLITE_CORRUPT", "SQLITE_NOTFOUND", "SQLITE_FULL",
    "SQLITE_CANTOPEN", "SQLITE_PROTOCOL", "SQLITE_EMPTY", "SQLITE_SCHEMA",
    "SQLITE_TOOBIG", "SQLITE_CONSTRAINT", "SQLITE_MISMATCH", "SQLITE_MISUSE",
    "SQLITE_NOLFS", "SQLITE_AUTH", "SQLITE_FORMAT", "SQLITE_RANGE", "SQLITE_NOTADB",
)


def _error(code, message):
    """the sqlite3 exception of an (extended) SQLite result code"""
    primary = next((n for n in _PRIMARY if getattr(sqlite3, n, None) == code & 0xFF), None)
    if primary == "SQLITE_NOMEM":
        return MemoryError()
    if primary in _OPERATIONAL:
        cls = sqlite3.OperationalError
    else:
        cls = getattr(sqlite3, _ERRORS.get(primary, "DatabaseError"))
    exc = cls(message)
    name = primary
    if primary is not None and code != code & 0xFF:
        name = next((n for n in dir(sqlite3) if n.startswith(primary + "_")
                     and getattr(sqlite3, n) == code), "unknown")
    exc.sqlite_errorcode = code
    exc.sqlite_errorname = name or "unknown"
    return exc


def _lstrip_sql(sql):
    """CPython's lstrip_sql: skip blanks and comments (None: nothing left)"""
    i, n = 0, len(sql)
    while i < n:
        c = sql[i]
        if c in " \t\f\n\r":
            i += 1
        elif sql.startswith("--", i):
            j = sql.find("\n", i)
            if j < 0:
                return None
            i = j + 1
        elif sql.startswith("/*", i):
            j = sql.find("*/", i + 2)
            if j < 0:
                return None
            i = j + 2
        else:
            return sql[i:]
    return None


def _is_dml(sql):
    s = _lstrip_sql(sql)
    if s is None:
        return False
    s = s[:7].lower()
    return s.startswith(("insert", "update", "delete")) or s == "replace"


def _bind(names, params):
    """the values sqlite3 binds for params (in placeholder order)"""
    if not names:
        # sqlite3 checks the count even with no placeholders
        if isinstance(params, dict):
            return []
        return list(_mem.execute("SELECT 1", params).fetchone()[1:])
    marks = ", ".join("?" if n is None else n for n in names)
    return list(_mem.execute(f"SELECT {marks}", params).fetchone())


def _row_cursor(names):
    """a cursor whose description is the column names (sqlite3.Row needs one)"""
    cur = _row_cursors.get(names)
    if cur is None:
        cols = ", ".join('NULL AS "%s"' % n.replace('"', '""') for n in names)
        cur = _row_cursors[names] = _mem.execute(f"SELECT {cols}")
    return cur


def _raw_bytes(value):
    """the bytes a converter gets: SQLite's text of the value"""
    if isinstance(value, bytes):
        return value
    if isinstance(value, tuple):
        return value[0]
    if isinstance(value, str):
        return value.encode()
    return _mem.execute("SELECT CAST(? AS TEXT)", (value,)).fetchone()[0].encode()


def _converter(decltype):
    if not decltype:
        return None
    for i, c in enumerate(decltype + " "):
        if c in " (":
            return sqlite3.converters.get(decltype[:i].upper())


class Cursor:
    def __init__(self, connection):
        self.connection = connection
        self.arraysize = 1
        self.row_factory = connection.row_factory
        self.description = None
        self.rowcount = -1
        self.lastrowid = None
        self._rows = []
        self._names = ()
        self._convs = ()
        self._closed = False

    def _check(self):
        if self._closed:
            raise sqlite3.ProgrammingError("Cannot operate on a closed cursor.")
        self.connection._check()

    def _run(self, sql, param_sets, many):
        self._check()
        if not isinstance(sql, str):
            name = "executemany" if many else "execute"
            raise TypeError(f"{name}() argument 1 must be str, not {type(sql).__name__}")
        names, readonly = _prepare(sql)
        if many and readonly:
            raise sqlite3.ProgrammingError("executemany() can only execute DML statements.")
        dml = _is_dml(sql)
        conn = self.connection
        if dml and conn.isolation_level is not None and _autocommit():
            _script("BEGIN " + conn.isolation_level)
        self._rows = []
        self.description = None
        self.rowcount = 0 if dml else -1
        # each parameter set is bound and run in turn (an error leaves the
        # sets before it done)
        cols = rows = None
        for params in param_sets:
            cols, rows, changes, lastrowid = _exec(sql, _bind(names, params))
            if dml:
                self.rowcount += changes
        if not many and cols is not None:
            self.lastrowid = lastrowid
        if cols:
            self.description = tuple((n, None, None, None, None, None, None) for n, _ in cols)
            self._names = tuple(n for n, _ in cols)
            self._convs = tuple(_converter(d) for _, d in cols)
            self._rows = rows
        return self

    def execute(self, sql, parameters=()):
        return self._run(sql, [parameters], False)

    def executemany(self, sql, seq_of_parameters):
        return self._run(sql, list(seq_of_parameters), True)

    def executescript(self, sql_script):
        self._check()
        if not isinstance(sql_script, str):
            raise ValueError("script argument must be unicode.")
        if not _autocommit():
            _script("COMMIT")
        _script(sql_script)
        self._rows = []
        self.description = None
        return self

    def _make(self, raw):
        values = []
        for v, conv, name in zip(raw, self._convs, self._names):
            if v is not None and conv is not None:
                v = conv(_raw_bytes(v))
            elif isinstance(v, tuple):
                text = v[0].decode("utf-8", "replace")
                raise sqlite3.OperationalError(
                    f"Could not decode to UTF-8 column '{name}' with text '{text}'")
            values.append(v)
        row = tuple(values)
        factory = self.row_factory
        if factory is sqlite3.Row:
            return sqlite3.Row(_row_cursor(self._names), row)
        if factory is not None:
            return factory(self, row)
        return row

    def fetchone(self):
        self._check()
        if not self._rows:
            return None
        return self._make(self._rows.pop(0))

    def fetchmany(self, size=None):
        self._check()
        if size is None:
            size = self.arraysize
        out = []
        while self._rows and len(out) < size:
            out.append(self._make(self._rows.pop(0)))
        return out

    def fetchall(self):
        self._check()
        rows, self._rows = self._rows, []
        return [self._make(r) for r in rows]

    def __iter__(self):
        return self

    def __next__(self):
        row = self.fetchone()
        if row is None:
            raise StopIteration
        return row

    def close(self):
        self._closed = True

    def setinputsizes(self, sizes):
        pass

    def setoutputsize(self, size, column=None):
        pass


class Connection:
    """Moonraker's database connection (SqliteProvider's sqlite3 connection)"""

    def __init__(self, sync):
        self.row_factory = sqlite3.Row
        self.text_factory = str
        self._isolation_level = ""
        self._closed = False
        self._sync = sync

    def _check(self):
        # Moonraker closes the provider's sync_conn when its thread starts
        if self._closed or (self._sync and _alive()):
            raise sqlite3.ProgrammingError("Cannot operate on a closed database.")

    @property
    def isolation_level(self):
        return self._isolation_level

    @isolation_level.setter
    def isolation_level(self, value):
        if value is None:
            if not _autocommit():
                _script("COMMIT")
        elif not isinstance(value, str):
            raise TypeError("isolation_level must be str or None")
        elif value.upper() not in ("", "DEFERRED", "IMMEDIATE", "EXCLUSIVE"):
            raise ValueError(
                "isolation_level string must be '', 'DEFERRED', 'IMMEDIATE', or 'EXCLUSIVE'")
        self._isolation_level = value

    @property
    def in_transaction(self):
        self._check()
        return not _autocommit()

    @property
    def total_changes(self):
        self._check()
        return _total_changes()

    def cursor(self, factory=Cursor):
        self._check()
        return factory(self)

    def execute(self, sql, parameters=()):
        return self.cursor().execute(sql, parameters)

    def executemany(self, sql, parameters):
        return self.cursor().executemany(sql, parameters)

    def executescript(self, sql_script):
        return self.cursor().executescript(sql_script)

    def commit(self):
        self._check()
        if not _autocommit():
            _script("COMMIT")

    def rollback(self):
        self._check()
        if not _autocommit():
            _script("ROLLBACK")

    def close(self):
        # Moonruster's connection stays open: only this object closes
        self._closed = True

    def __enter__(self):
        return self

    def __exit__(self, exc_type, exc_value, tb):
        if exc_type is None:
            self.commit()
        else:
            self.rollback()
        return False


# the provider thread's connection and its sync_conn (used before the
# thread starts)
connection = Connection(False)
sync_connection = Connection(True)
