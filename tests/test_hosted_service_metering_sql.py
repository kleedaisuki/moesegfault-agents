"""Tool fee holds exercise the actual immutable ledger, not another wallet model."""
from __future__ import annotations

import pathlib
import re
import sqlite3
import unittest

ROOT: pathlib.Path = pathlib.Path(__file__).resolve().parents[1]
METER: str = (ROOT / "crates/agents-worker/src/metering.rs").read_text(encoding="utf-8")
SERVICE: str = (ROOT / "crates/agents-worker/src/hosted_service_metering.rs").read_text(encoding="utf-8")


def query(source: str, prefix: str) -> str:
    return next(value for value in re.findall(r'\.prepare\("([^"\n]+)"', source) if value.startswith(prefix))


RESERVE: str = query(METER, "INSERT INTO usage_reservations")
APPEND: str = query(METER, "INSERT INTO usage_ledger")
SETTLE: str = query(METER, "UPDATE usage_reservations SET state='settled'")
RELEASE: str = query(METER, "UPDATE usage_reservations SET state='released'")
VERIFY: str = query(SERVICE, "SELECT request_hash,idempotency_key")


class HostedServiceMeteringSqlTests(unittest.TestCase):
    def setUp(self) -> None:
        self.db: sqlite3.Connection = sqlite3.connect(":memory:")
        for migration in sorted((ROOT / "migrations").glob("*.sql")):
            self.db.executescript(migration.read_text(encoding="utf-8"))
        for owner in ("alice", "bob"):
            self.db.execute("INSERT INTO accounts VALUES(?,?,?,?,?)", (owner, "issuer", owner, 0, 0))
            self.db.execute("INSERT INTO usage_accounts VALUES(?,100000,0,0,0)", (owner,))

    def tearDown(self) -> None:
        self.db.close()

    def reserve(self, hold: str, amount: int, owner: str = "alice") -> None:
        key: str = "tool:seedream5:" + hold
        self.db.execute(RESERVE, (hold, owner, None, key, "frozen:" + hold, str(amount), "1", "1", owner, key, None, owner, None))

    def append(self, hold: str, quantity: int, unit: str, amount: int) -> None:
        self.db.execute(APPEND, ("charge:" + hold, hold, "upstream", "model", str(quantity), unit, str(amount), "reviewed-api-v1", "provider-known", "2", hold, "upstream"))

    def test_paid_image_is_exact_once_and_separate_from_outer_run(self) -> None:
        self.reserve("outer-run", 100)
        self.reserve("image-call", 18000)
        self.reserve("image-call", 18000)
        self.append("image-call", 1, "generated_image", 18000)
        self.db.execute(SETTLE, ("image-call", "3", "image-call"))
        self.append("image-call", 1, "generated_image", 18000)
        self.db.execute(SETTLE, ("image-call", "3", "image-call"))
        self.assertEqual(self.db.execute("SELECT spent_microusd,reserved_microusd FROM usage_accounts WHERE account_id='alice'").fetchone(), (18000, 100))
        self.assertEqual(self.db.execute("SELECT quantity,unit,amount_microusd FROM usage_ledger WHERE reservation_id='image-call'").fetchall(), [(1, "generated_image", 18000)])
        self.assertEqual(self.db.execute("SELECT state FROM usage_reservations WHERE id='outer-run'").fetchone(), ("reserved",))

    def test_unknown_dispatch_stays_funded_and_proved_empty_receipt_releases_surplus(self) -> None:
        self.reserve("unknown-search", 750)
        self.assertEqual(self.db.execute("SELECT state,reserved_microusd FROM usage_reservations WHERE id='unknown-search'").fetchone(), ("reserved", 750))
        self.reserve("known-empty", 750)
        self.append("known-empty", 0, "successful_search_request", 0)
        with self.assertRaises(sqlite3.IntegrityError):
            self.db.execute(RELEASE, ("3", "known-empty"))
        self.db.execute(SETTLE, ("known-empty", "3", "known-empty"))
        self.assertEqual(self.db.execute("SELECT spent_microusd,reserved_microusd FROM usage_accounts WHERE account_id='alice'").fetchone(), (0, 750))
        self.assertEqual(self.db.execute("SELECT quantity,unit,amount_microusd FROM usage_ledger WHERE reservation_id='known-empty'").fetchone(), (0, "successful_search_request", 0))

    def test_readonly_rehydration_is_owner_and_session_scoped(self) -> None:
        self.reserve("image-call", 18000)
        self.assertEqual(self.db.execute(VERIFY, ("image-call", "alice", None)).fetchone(), ("frozen:image-call", "tool:seedream5:image-call", "18000", "reserved"))
        self.assertIsNone(self.db.execute(VERIFY, ("image-call", "bob", None)).fetchone())
        self.assertIsNone(self.db.execute(VERIFY, ("image-call", "alice", "invented-session")).fetchone())
        self.assertIsNone(self.db.execute(VERIFY, ("missing", "alice", None)).fetchone())
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM usage_reservations").fetchone(), (1,))

    def test_byok_zero_upstream_debit_and_no_over_ceiling_charges(self) -> None:
        self.reserve("byok", 0)
        self.append("byok", 1, "provider_request", 0)
        self.db.execute(SETTLE, ("byok", "3", "byok"))
        self.reserve("paid", 18000)
        with self.assertRaises(sqlite3.IntegrityError):
            self.append("paid", 1, "generated_image", 18001)
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM usage_ledger WHERE reservation_id='paid'").fetchone(), (0,))
        self.assertEqual(self.db.execute("SELECT state FROM usage_reservations WHERE id='paid'").fetchone(), ("reserved",))
        self.assertEqual(self.db.execute("SELECT spent_microusd,reserved_microusd FROM usage_accounts WHERE account_id='alice'").fetchone(), (0, 18000))

    def test_expired_window_recovery_reads_then_releases_original_undispatched_hold(self) -> None:
        self.reserve("expired-before-dispatch", 750)
        original: tuple[object, ...] | None = self.db.execute(VERIFY, ("expired-before-dispatch", "alice", None)).fetchone()
        self.assertIsNotNone(original)
        # No new reservation or timestamp is manufactured during cold restore.
        # An admission window is dispatch policy, not financial-row expiration.
        self.assertEqual(self.db.execute("SELECT created_at,updated_at FROM usage_reservations WHERE id='expired-before-dispatch'").fetchone(), (1, 1))
        self.assertEqual(self.db.execute(VERIFY, ("expired-before-dispatch", "alice", None)).fetchone(), original)
        self.db.execute(RELEASE, ("604801", "expired-before-dispatch"))
        self.assertEqual(self.db.execute(VERIFY, ("expired-before-dispatch", "alice", None)).fetchone(), ("frozen:expired-before-dispatch", "tool:seedream5:expired-before-dispatch", "750", "released"))
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM usage_reservations").fetchone(), (1,))
        self.assertEqual(self.db.execute("SELECT spent_microusd,reserved_microusd FROM usage_accounts WHERE account_id='alice'").fetchone(), (0, 0))

    def test_finalized_hold_recovery_replays_known_receipt_without_new_reservation(self) -> None:
        self.reserve("already-paid", 18000)
        self.append("already-paid", 1, "generated_image", 18000)
        self.db.execute(SETTLE, ("already-paid", "3", "already-paid"))
        self.assertEqual(self.db.execute(VERIFY, ("already-paid", "alice", None)).fetchone(), ("frozen:already-paid", "tool:seedream5:already-paid", "18000", "settled"))
        self.append("already-paid", 1, "generated_image", 18000)
        self.db.execute(SETTLE, ("already-paid", "604801", "already-paid"))
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM usage_reservations").fetchone(), (1,))
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM usage_ledger").fetchone(), (1,))
        self.assertEqual(self.db.execute("SELECT spent_microusd,reserved_microusd FROM usage_accounts WHERE account_id='alice'").fetchone(), (18000, 0))


if __name__ == "__main__":
    unittest.main()
