"""Exercise the real admission SQL independently of outstanding financial holds."""

import pathlib
import re
import sqlite3
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
SOURCE = (ROOT / "crates/agents-worker/src/metering.rs").read_text(encoding="utf-8")
ADMIT = re.search(r'db\.prepare\("(INSERT INTO usage_reservations[^"\n]+)"\)', SOURCE).group(1)
END = re.search(r'db\.prepare\("(UPDATE usage_reservations SET execution_finished_at[^"\n]+)"\)', SOURCE).group(1)


class ExecutionLifecycleTests(unittest.TestCase):
    def setUp(self):
        self.db = sqlite3.connect(":memory:")
        for migration in sorted((ROOT / "migrations").glob("*.sql")):
            self.db.executescript(migration.read_text(encoding="utf-8"))
        self.db.execute("INSERT INTO accounts VALUES ('a','issuer','sub',0,0)")
        self.db.execute("INSERT INTO usage_accounts VALUES ('a',100,0,0,0)")

    def tearDown(self):
        self.db.close()

    def admit(self, run):
        key = "run:" + run
        self.db.execute(ADMIT, (run, "a", None, key, "hash", "30", "1", "1", "a", key, "1", "a", "1"))
        return self.db.execute("SELECT COUNT(*) FROM usage_reservations WHERE id=?", (run,)).fetchone()[0]

    def test_ended_execution_frees_concurrency_but_keeps_uncertain_money_held(self):
        self.assertEqual(self.admit("first"), 1)
        self.assertEqual(self.admit("second"), 0)
        self.db.execute(END, ("2000", "first", "a"))
        self.db.execute(END, ("3000", "first", "a"))
        self.assertEqual(self.db.execute("SELECT state,execution_finished_at FROM usage_reservations").fetchone(), ("reserved", 2000))
        self.assertEqual(self.db.execute("SELECT reserved_microusd,spent_microusd FROM usage_accounts").fetchone(), (30, 0))
        self.assertEqual(self.admit("second"), 1)
        self.assertEqual(self.db.execute("SELECT reserved_microusd FROM usage_accounts").fetchone(), (60,))
        with self.assertRaises(sqlite3.IntegrityError):
            self.db.execute("UPDATE usage_reservations SET execution_finished_at=NULL WHERE id='first'")

    def test_other_tenant_cannot_finish_execution(self):
        self.admit("first")
        self.db.execute(END, ("2000", "first", "other"))
        self.assertEqual(self.admit("second"), 0)
        self.assertIsNone(self.db.execute("SELECT execution_finished_at FROM usage_reservations").fetchone()[0])


if __name__ == "__main__":
    unittest.main()
