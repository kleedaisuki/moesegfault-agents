"""Original delivery consumes the existing escrow, with atomically bounded reads."""
from __future__ import annotations

import re
import sqlite3
import unittest

from test_hosted_service_metering_sql import RESERVE, ROOT

SOURCE: str = (ROOT / "crates/agents-worker/src/response_image_projection.rs").read_text(encoding="utf-8")
CLAIM: str = next(sql for sql in re.findall(r'\.prepare\(\s*"([^"\n]+)"', SOURCE)
                  if sql.startswith("INSERT INTO resource_observations"))


class OriginalImageReadSqlTests(unittest.TestCase):
    def setUp(self) -> None:
        self.db: sqlite3.Connection = sqlite3.connect(":memory:")
        for migration in sorted((ROOT / "migrations").glob("*.sql")):
            self.db.executescript(migration.read_text(encoding="utf-8"))
        self.db.execute("INSERT INTO accounts VALUES('owner','issuer','subject',0,0)")
        self.db.execute("INSERT INTO usage_accounts VALUES('owner',100,0,0,0)")
        self.db.execute(RESERVE, ("delivery", "owner", None, "image:delivery", "snapshot", "100", "1", "1",
                                 "owner", "image:delivery", None, "owner", None))

    def tearDown(self) -> None:
        self.db.close()

    def claim(self, event: str, owner: str = "owner", maximum: int = 8) -> tuple[str] | None:
        return self.db.execute(CLAIM, ("delivery", owner, event, "pinned-v1", "delivery", owner,
                                      "delivery", owner, str(maximum))).fetchone()

    def test_original_reads_work_with_no_unreserved_balance_and_never_create_new_holds(self) -> None:
        self.assertEqual(self.db.execute("SELECT budget_microusd-spent_microusd-reserved_microusd FROM usage_accounts").fetchone(), (0,))
        for ordinal in range(8):
            self.assertEqual(self.claim(f"original:{ordinal}"), (f"original:{ordinal}",))
        self.assertIsNone(self.claim("original:denied"))
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM usage_reservations").fetchone(), (1,))
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM resource_observations WHERE status='pending' AND quantity IS NULL").fetchone(), (8,))
        with self.assertRaises(sqlite3.IntegrityError):
            self.db.execute(RESERVE, ("later-get", "owner", None, "image:later-get", "new", "1", "2", "2",
                                     "owner", "image:later-get", None, "owner", None))

    def test_claim_is_exact_owner_and_reserved_account_scope(self) -> None:
        self.assertIsNone(self.claim("foreign", "different"))
        self.db.execute("UPDATE usage_reservations SET state='released' WHERE id='delivery'")
        self.assertIsNone(self.claim("finalized"))
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM resource_observations").fetchone(), (0,))

    def test_cap_ablation_demonstrates_pending_attempts_are_not_free_retries(self) -> None:
        for ordinal in range(8):
            self.claim(f"first:{ordinal}")
        self.assertIsNone(self.claim("retry:unknown"))
        bounded: str = "AND (SELECT COUNT(*) FROM resource_observations WHERE run_id=? AND account_id=? AND session_id IS NULL AND resource='r2_operation' AND unit='class_b')<CAST(? AS INTEGER)"
        self.assertIn(bounded, CLAIM)
        unbounded: str = CLAIM.replace(bounded, "AND (? IS NOT NULL OR ? IS NOT NULL) AND CAST(? AS INTEGER)>=0")
        self.assertEqual(self.db.execute(unbounded, ("delivery", "owner", "ablation", "pinned-v1", "delivery", "owner", "delivery", "owner", "8")).fetchone(), ("ablation",))


if __name__ == "__main__":
    unittest.main()
