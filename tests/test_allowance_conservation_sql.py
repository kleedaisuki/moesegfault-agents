"""Independent bounded financial state sweep; production SQL, in-memory SQLite only."""

import itertools
import pathlib
import re
import sqlite3
import tempfile
import threading
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
SOURCE = (ROOT / "crates/agents-worker/src/allowances.rs").read_text()
GRANT = re.search(r'const GRANT: &str = "(.*?)";', SOURCE, re.S).group(1)
REPLACE = re.search(r'const REPLACE: &str = "(.*?)";', SOURCE, re.S).group(1)


def create_wallet(legacy):
    db = sqlite3.connect(":memory:")
    for migration in sorted((ROOT / "migrations").glob("*.sql")):
        db.executescript(migration.read_text())
    db.execute("INSERT INTO accounts VALUES('a','i','s',0,0)")
    db.execute("INSERT INTO usage_accounts VALUES('a',0,0,0,0)")
    db.execute(
        GRANT,
        ("a", "0", "1000", "free", "1000000", "0", "-1", "100000000"),
    )
    if legacy:
        db.execute(
            GRANT,
            ("a", "0", "1000", "free", "180000000", "733", "0", None),
        )
    return db


def available_balance(db):
    return db.execute(
        "SELECT budget_microusd-spent_microusd-reserved_microusd "
        "FROM usage_accounts"
    ).fetchone()[0]


def reserve(db, reservation_id, amount):
    db.execute(
        "INSERT INTO usage_reservations"
        "(id,account_id,idempotency_key,request_hash,reserved_microusd,state,"
        "created_at,updated_at) VALUES(?,'a',?,'hash',?,'reserved',740,740)",
        (reservation_id, reservation_id, amount),
    )


def settle(db, reservation_id, cost, now=850):
    if cost:
        db.execute(
            "INSERT INTO usage_ledger "
            "VALUES(?,?,?,'model',1,'request',?,'v1',NULL,?)",
            (reservation_id, reservation_id, reservation_id, cost, now),
        )
    db.execute(
        "UPDATE usage_reservations SET state='settled',settled_microusd=?,"
        "updated_at=? WHERE id=?",
        (cost, now, reservation_id),
    )


def apply_paid_transition(db, mode, target, spent, held):
    if mode == "legacy_lower":
        db.execute(
            REPLACE,
            ("a", "0", "700", "7000", "billing:go", str(target), "800"),
        )
        assert db.execute(
            "SELECT COUNT(*) FROM allowance_replacements"
        ).fetchone()[0] == 0
        db.execute(
            GRANT,
            ("a", "700", "7000", "billing:go", str(target), "800", "0", None),
        )
        return target

    for now in ("800", "801"):
        db.execute(
            REPLACE,
            ("a", "0", "700", "7000", "billing:paid", str(target), now),
        )
    assert db.execute(
        "SELECT COUNT(*) FROM allowance_replacements"
    ).fetchone()[0] == 1
    return target - spent - held


def check_state_sequence(mode, spent, held, partial_settlement, rollover):
    db = create_wallet(legacy=mode != "first")
    try:
        reserve(db, "spent", spent)
        settle(db, "spent", spent, 750)
        reserve(db, "held", held)
        target = 180000000 if mode == "legacy_same" else 8000000
        expected = apply_paid_transition(db, mode, target, spent, held)
        assert available_balance(db) == expected, (
            mode, "replacement", spent, held, available_balance(db), expected
        )

        actual = held // 2 if partial_settlement else 0
        if rollover:
            start = "700" if mode == "legacy_lower" else "0"
            db.execute(
                GRANT,
                ("a", "7000", "8000", "free", "1000000", "7000", start, "100000000"),
            )
            expected = 1000000
            settle(db, "held", actual, 7100)
        else:
            settle(db, "held", actual)
            expected = target if mode == "legacy_lower" else target - spent - actual
        assert available_balance(db) == expected, (
            mode, "settlement", spent, held, actual, rollover,
            available_balance(db), expected,
        )
    finally:
        db.close()


def check_over_cap_legacy_hold():
    # Explicit over-cap old hold remains old-source money on lower-current-tier reset.
    db = create_wallet(legacy=True)
    try:
        reserve(db, "held", 9000000)
        db.execute(
            GRANT,
            ("a", "700", "7000", "billing:go", "8000000", "800", "0", None),
        )
        settle(db, "held", 0)
        assert available_balance(db) == 8000000
    finally:
        db.close()


def check_concurrent_handoff():
    # Concurrent credentials hand off the same Free identity only once.
    source = create_wallet(legacy=False)
    source.commit()
    try:
        with tempfile.TemporaryDirectory(dir=ROOT / ".temp") as directory:
            path = str(pathlib.Path(directory) / "wallet.sqlite")
            disk = sqlite3.connect(path)
            source.backup(disk)
            disk.close()
            barrier = threading.Barrier(2)
            errors = []

            def handoff():
                db = sqlite3.connect(path, timeout=10)
                try:
                    barrier.wait()
                    db.execute(
                        REPLACE,
                        ("a", "0", "700", "7000", "billing:paid", "180000000", "800"),
                    )
                    db.commit()
                except Exception as error:
                    errors.append(str(error))
                finally:
                    db.close()

            threads = [threading.Thread(target=handoff) for _ in range(2)]
            for thread in threads:
                thread.start()
            for thread in threads:
                thread.join()
            assert not errors, errors

            db = sqlite3.connect(path)
            try:
                assert available_balance(db) == 180000000
                assert db.execute(
                    "SELECT COUNT(*) FROM allowance_replacements"
                ).fetchone()[0] == 1
            finally:
                db.close()
    finally:
        source.close()


class AllowanceConservationReviewTests(unittest.TestCase):
    def test_handoff_retirement_refunds_and_concurrent_credentials(self):
        cases = itertools.product(
            ("first", "legacy_same", "legacy_lower"),
            (0, 200000, 500000),
            (0, 100000, 400000),
            (False, True),
            (False, True),
        )
        count = 0
        for case in cases:
            check_state_sequence(*case)
            count += 1
        self.assertEqual(count, 108)
        check_over_cap_legacy_hold()
        check_concurrent_handoff()


if __name__ == "__main__":
    unittest.main()
