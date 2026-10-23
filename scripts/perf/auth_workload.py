"""Compare equivalent auth SQL workloads locally; not a D1 latency benchmark."""
import importlib.util
import json
from pathlib import Path
import sqlite3
import time

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("auth_sql", ROOT / "tests/test_auth_sql.py")
auth = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(auth)


def database():
    db = sqlite3.connect(":memory:")
    for migration in sorted((ROOT / "migrations").glob("*.sql")):
        db.executescript(migration.read_text(encoding="utf-8"))
    db.execute(auth.ACCOUNT_INSERT, ("a", "issuer", "subject", 0))
    db.execute(auth.WALLET_INSERT, ("issuer", "subject", 0))
    db.execute("INSERT INTO api_keys(id,account_id,name,key_hash,key_prefix,created_at) VALUES('k','a','test','hash','prefix',0)")
    return db


def key_workload(optimized):
    with database() as db:
        statements, writes = 0, 0
        start = time.perf_counter()
        for now in range(0, 300, 3):
            row = db.execute(auth.KEY_LOOKUP, ("hash",)).fetchone()
            statements += 1
            if optimized and row[1] is not None and row[1] > now - 300:
                continue
            sql = auth.KEY_TOUCH if optimized else "UPDATE api_keys SET last_used_at=?1 WHERE key_hash=?2"
            args = (now, "hash", now - 300) if optimized else (now, "hash")
            writes += db.execute(sql, args).rowcount
            statements += 1
        return {"requests": 100, "sql_statements": statements, "rows_updated": writes,
                "local_sqlite_ms": round((time.perf_counter() - start) * 1000, 3)}


def native_workload(optimized):
    with database() as db:
        statements = 0
        start = time.perf_counter()
        for i in range(100):
            if optimized:
                assert db.execute(auth.ACCOUNT_LOOKUP, ("issuer", "subject")).fetchone() == ("a",)
                statements += 1
                continue
            db.execute(auth.ACCOUNT_INSERT, ("unused-" + str(i), "issuer", "subject", i))
            assert db.execute("SELECT id FROM accounts WHERE issuer=?1 AND subject=?2", ("issuer", "subject")).fetchone() == ("a",)
            db.execute("INSERT INTO usage_accounts(account_id,updated_at) VALUES(?1,?2) ON CONFLICT(account_id) DO NOTHING", ("a", i))
            statements += 3
        return {"requests": 100, "sql_statements": statements,
                "local_sqlite_ms": round((time.perf_counter() - start) * 1000, 3)}


if __name__ == "__main__":
    report = {
        "environment": {"engine": "in-memory SQLite", "sqlite": sqlite3.sqlite_version, "concurrency": 1},
        "api_key": {"before": key_workload(False), "after": key_workload(True)},
        "native_principal": {"before": native_workload(False), "after": native_workload(True)},
        "note": "Local SQL time excludes network, Worker, D1 replication and JWT verification. Counts, not these timings, predict saved D1 round trips.",
    }
    print(json.dumps(report, indent=2))
