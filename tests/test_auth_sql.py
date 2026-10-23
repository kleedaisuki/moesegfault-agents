"""Exercise the actual auth SQL against the migrated schema, including crash repair."""
from pathlib import Path
import re
import sqlite3
import unittest

ROOT = Path(__file__).resolve().parents[1]
SOURCE = (ROOT / "crates/agents-worker/src/auth.rs").read_text(encoding="utf-8")
SQL = re.findall(r'\.prepare\("([^"\n]+)"\)', SOURCE)


def query(prefix):
    matches = [sql for sql in SQL if sql.startswith(prefix)]
    assert len(matches) == 1, (prefix, len(matches))
    return matches[0]


KEY_LOOKUP = query("SELECT account_id,last_used_at FROM api_keys")
KEY_TOUCH = query("UPDATE api_keys SET last_used_at=")
ACCOUNT_LOOKUP = query("SELECT a.id FROM accounts a JOIN usage_accounts")
ACCOUNT_INSERT = query("INSERT INTO accounts(id,issuer,subject,")
WALLET_INSERT = query("INSERT INTO usage_accounts(account_id,updated_at) SELECT")


class AuthSqlTests(unittest.TestCase):
    def setUp(self):
        self.db = sqlite3.connect(":memory:")
        for migration in sorted((ROOT / "migrations").glob("*.sql")):
            self.db.executescript(migration.read_text(encoding="utf-8"))

    def tearDown(self):
        self.db.close()

    def bootstrap(self, id="a", issuer="issuer", subject="subject"):
        with self.db:
            self.db.execute(ACCOUNT_INSERT, (id, issuer, subject, 0))
            self.db.execute(WALLET_INSERT, (issuer, subject, 0))
        return self.db.execute(ACCOUNT_LOOKUP, (issuer, subject)).fetchone()

    def key(self):
        self.bootstrap()
        self.db.execute("INSERT INTO api_keys(id,account_id,name,key_hash,key_prefix,created_at) VALUES('k','a','test','hash','prefix',0)")
        self.db.commit()

    def test_warm_lookup_does_not_modify_wallet(self):
        self.assertEqual(self.bootstrap(), ("a",))
        self.db.execute("UPDATE usage_accounts SET budget_microusd=100")
        before = self.db.total_changes
        for _ in range(100):
            self.assertEqual(self.db.execute(ACCOUNT_LOOKUP, ("issuer", "subject")).fetchone(), ("a",))
        self.assertEqual(self.db.total_changes, before)
        self.assertEqual(self.db.execute("SELECT budget_microusd FROM usage_accounts").fetchone(), (100,))

    def test_repeated_bootstrap_resolves_existing_subject_and_separates_issuers(self):
        self.assertEqual(self.bootstrap(), ("a",))
        self.assertEqual(self.bootstrap("losing-race-id"), ("a",))
        self.assertEqual(self.bootstrap("b", "other-issuer"), ("b",))
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM usage_accounts").fetchone(), (2,))

    def test_partial_legacy_account_is_repaired(self):
        self.db.execute(ACCOUNT_INSERT, ("original", "issuer", "subject", 0))
        self.assertIsNone(self.db.execute(ACCOUNT_LOOKUP, ("issuer", "subject")).fetchone())
        self.assertEqual(self.bootstrap("retry"), ("original",))

    def test_wallet_failure_rolls_back_principal_creation(self):
        self.db.execute("CREATE TRIGGER fail_wallet BEFORE INSERT ON usage_accounts BEGIN SELECT RAISE(ABORT,'injected'); END")
        with self.assertRaises(sqlite3.IntegrityError):
            self.bootstrap()
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM accounts").fetchone(), (0,))

    def test_polling_coarsens_only_last_used_and_revocation_is_immediate(self):
        self.key()
        writes = 0
        # A dashboard/API polling workload: one authoritative key read every 3 seconds.
        for now in range(0, 300, 3):
            account, last = self.db.execute(KEY_LOOKUP, ("hash",)).fetchone()
            self.assertEqual(account, "a")
            if last is None or last <= now - 300:
                writes += self.db.execute(KEY_TOUCH, (now, "hash", now - 300)).rowcount
        self.assertEqual(writes, 1)
        self.assertEqual(self.db.execute(KEY_TOUCH, (299, "hash", -1)).rowcount, 0)
        self.assertEqual(self.db.execute(KEY_TOUCH, (300, "hash", 0)).rowcount, 1)
        self.db.execute("UPDATE api_keys SET revoked_at=301 WHERE id='k'")
        self.assertIsNone(self.db.execute(KEY_LOOKUP, ("hash",)).fetchone())
        self.assertEqual(self.db.execute(KEY_TOUCH, (600, "hash", 300)).rowcount, 0)


if __name__ == "__main__":
    unittest.main()
