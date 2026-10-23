"""Wallet-drain and allowance-vintage journeys for pre-funded conversation export."""
import pathlib
import re
import sqlite3
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
GRANT = re.search(r'const GRANT: &str = "(.*?)";', (ROOT / 'crates/agents-worker/src/allowances.rs').read_text(), re.S).group(1)
RESERVE = re.search(r'db.prepare\("(INSERT INTO usage_reservations .*?)"\)', (ROOT / 'crates/agents-worker/src/metering.rs').read_text(), re.S).group(1)


class CleanupTests(unittest.TestCase):
    def setUp(self):
        self.db = sqlite3.connect(':memory:')
        for path in sorted((ROOT / 'migrations').glob('*.sql')):
            self.db.executescript(path.read_text(encoding='utf-8'))
        self.db.execute("INSERT INTO accounts VALUES('a','issuer','a',0,0)")
        self.db.execute("INSERT INTO usage_accounts VALUES('a',0,0,0,0)")
        self.db.execute("INSERT INTO agents VALUES('agent','a','test','{}',0,0,NULL)")
        self.db.execute("INSERT INTO sessions VALUES('s','a','agent','do','active','{}','pin',0,0,NULL,NULL,NULL)")
        self.grant(0, 100)

    def tearDown(self):
        self.db.close()

    def grant(self, start, end):
        expected = self.db.execute("SELECT COALESCE(MAX(period_start),-1) FROM allowance_windows WHERE account_id='a'").fetchone()[0]
        self.db.execute(GRANT, ('a', str(start), str(end), 'free', '100', str(start), str(expected), None))

    def reserve(self, key, amount, now=1):
        self.db.execute(RESERVE, (key, 'a', 's', key, '{"pricing":"pinned"}', str(amount), str(now), str(now), 'a', key, None, 'a', None))

    def charge(self, key, event, amount, now):
        self.db.execute("INSERT INTO usage_ledger VALUES(?,?,?,'r2_operation',1,'class_a',?,'pinned',NULL,?)", (key + ':' + event, key, event, amount, now))

    def settle(self, key, now):
        self.db.execute("UPDATE usage_reservations SET state='settled',settled_microusd=(SELECT COALESCE(SUM(amount_microusd),0) FROM usage_ledger WHERE reservation_id=?),updated_at=? WHERE id=? AND state='reserved'", (key, now, key))

    def available(self):
        return self.db.execute("SELECT budget_microusd-spent_microusd-reserved_microusd FROM usage_accounts WHERE account_id='a'").fetchone()[0]

    def drain_after_prefunding(self):
        self.reserve('maintenance:s:0', 67)
        self.reserve('run:1', 33)
        self.charge('run:1', 'model', 33, 2)
        self.settle('run:1', 2)
        self.assertEqual(self.available(), 0)

    def test_archive_is_funded_after_model_drains_available_wallet(self):
        self.drain_after_prefunding()
        self.charge('maintenance:s:0', 'export-and-d1', 53, 3)
        self.settle('maintenance:s:0', 3)
        self.assertEqual(self.available(), 14)

    def test_old_cleanup_settlement_cannot_refund_into_new_month(self):
        self.drain_after_prefunding()
        self.grant(100, 200)
        self.assertEqual(self.available(), 100)
        self.charge('maintenance:s:0', 'export-and-d1', 53, 110)
        self.settle('maintenance:s:0', 110)
        self.assertEqual(self.available(), 100)

    def test_old_unused_cleanup_release_cannot_mint_fresh_credit(self):
        self.drain_after_prefunding()
        self.grant(100, 200)
        self.db.execute("UPDATE usage_reservations SET state='released',updated_at=110 WHERE id='maintenance:s:0' AND state='reserved'")
        self.assertEqual(self.available(), 100)

    def test_recorded_archive_usage_cannot_be_refunded_as_unused(self):
        self.reserve('maintenance:s:0', 67)
        self.charge('maintenance:s:0', 'export', 5, 2)
        with self.assertRaisesRegex(sqlite3.IntegrityError, 'recorded_usage_requires_settlement'):
            self.db.execute("UPDATE usage_reservations SET state='released',updated_at=3 WHERE id='maintenance:s:0'")


if __name__ == '__main__':
    unittest.main()
