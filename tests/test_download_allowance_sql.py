"""Download admission renews eligible funding without changing session state."""

from pathlib import Path
import re
import sqlite3
import unittest

ROOT = Path(__file__).resolve().parents[1]
ALLOWANCES = (ROOT / 'crates/agents-worker/src/allowances.rs').read_text(encoding='utf-8')
METERING = (ROOT / 'crates/agents-worker/src/metering.rs').read_text(encoding='utf-8')
GRANT = re.search(r'const GRANT: &str = "(.*?)";', ALLOWANCES, re.S).group(1)
RESERVE = re.search(r'db.prepare\("(INSERT INTO usage_reservations [^"\n]+)"\)', METERING).group(1)


class DownloadAllowanceSqlTests(unittest.TestCase):
    def test_expired_sdk_wallet_renews_once_before_download_reservation(self) -> None:
        db = sqlite3.connect(':memory:')
        try:
            for migration in sorted((ROOT / 'migrations').glob('*.sql')):
                db.executescript(migration.read_text(encoding='utf-8'))
            db.execute("INSERT INTO accounts VALUES('a','issuer','subject',0,0)")
            db.execute("INSERT INTO usage_accounts VALUES('a',0,0,0,0)")
            db.execute(GRANT, ('a', '0', '100', 'free', '1000000', '0', '-1', '100000000'))
            reservation = ('download:probe', 'a', None, 'download:probe', 'snapshot',
                           '1', '100', '100', 'a', 'download:probe', None, 'a', None)
            with self.assertRaisesRegex(sqlite3.IntegrityError, 'allowance_expired'):
                db.execute(RESERVE, reservation)
            self.assertEqual(db.execute('SELECT COUNT(*) FROM usage_reservations').fetchone()[0], 0)
            for _ in range(2):
                db.execute(GRANT, ('a', '100', '200', 'free', '1000000', '100', '0', '100000000'))
            for _ in range(2):
                db.execute(RESERVE, reservation)
            self.assertEqual(db.execute('SELECT spent_microusd,reserved_microusd FROM usage_accounts').fetchone(), (0, 1))
            self.assertEqual(db.execute('SELECT COUNT(*) FROM allowance_grants').fetchone()[0], 2)
            self.assertEqual(db.execute('SELECT period_start,period_end FROM reservation_vintages').fetchone(), (100, 200))
            self.assertEqual(db.execute("SELECT COUNT(*) FROM usage_reservations WHERE state='reserved' AND execution_finished_at IS NULL AND idempotency_key LIKE 'run:%'").fetchone()[0], 0)
        finally:
            db.close()


if __name__ == '__main__':
    unittest.main()
