"""Real SQLite wallet journeys using the production single-statement grant SQL."""
import pathlib
import re
import sqlite3
import tempfile
import threading
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
GRANT = re.search(r'const GRANT: &str = "(.*?)";', (ROOT / 'crates/agents-worker/src/allowances.rs').read_text(), re.S).group(1)


class AllowanceTests(unittest.TestCase):
    def setUp(self):
        self.db = sqlite3.connect(':memory:')
        for path in sorted((ROOT / 'migrations').glob('*.sql')):
            self.db.executescript(path.read_text(encoding='utf-8'))
        self.account('a')

    def tearDown(self):
        self.db.close()

    def account(self, name):
        self.db.execute('INSERT INTO accounts VALUES(?,?,?,?,?)', (name,'issuer',name,0,0))
        self.db.execute('INSERT INTO usage_accounts VALUES(?,0,0,0,0)', (name,))

    def grant(self, start=0, end=100, amount=100, now=0, source='free', account='a', cap=None, expected=None):
        if expected is None:
            expected = self.db.execute('SELECT COALESCE(MAX(period_start),-1) FROM allowance_windows WHERE account_id=?',(account,)).fetchone()[0]
        self.db.execute(GRANT, (account,str(start),str(end),source,str(amount),str(now),str(expected),None if cap is None else str(cap)))

    def reserve(self, key, amount, now=1):
        self.db.execute("INSERT INTO usage_reservations(id,account_id,idempotency_key,request_hash,reserved_microusd,state,created_at,updated_at) VALUES(?,'a',?,'hash',?,'reserved',?,?)", (key,key,amount,now,now))

    def balance(self):
        return self.db.execute('SELECT budget_microusd-spent_microusd-reserved_microusd FROM usage_accounts WHERE account_id="a"').fetchone()[0]

    def test_login_upgrade_downgrade_replay_and_new_period(self):
        self.grant()
        self.grant()
        self.grant(amount=800,now=50)
        self.assertEqual(self.balance(),450)  # 100 + half of (800-100)
        self.grant(amount=100,now=60)
        self.grant(amount=800,now=60)
        self.assertEqual(self.balance(),450)
        self.grant(start=100,end=200,amount=800,now=100)
        self.assertEqual(self.balance(),800)
        self.assertEqual(self.db.execute('SELECT COUNT(*) FROM allowance_grants').fetchone()[0],3)

    def test_old_unknown_outcome_settlement_does_not_refund_new_period(self):
        self.grant()
        self.reserve('old',80)
        self.grant(start=100,end=200,now=100)
        self.db.execute("INSERT INTO usage_ledger VALUES('event','old','event','model',1,'request',30,'v1',NULL,110)")
        self.db.execute("UPDATE usage_reservations SET state='settled',settled_microusd=30,updated_at=110 WHERE id='old'")
        self.assertEqual(self.balance(),100)
        self.assertEqual(self.db.execute('SELECT spent_microusd,reserved_microusd FROM usage_accounts').fetchone(),(30,0))

    def test_old_release_and_legacy_hold_do_not_mint_credit(self):
        self.reserve('legacy',0)
        self.grant()
        self.reserve('old',80)
        self.grant(start=100,end=200,now=100)
        for key in ('old','legacy'):
            self.db.execute("UPDATE usage_reservations SET state='released',updated_at=110 WHERE id=?",(key,))
        self.assertEqual(self.balance(),100)

    def test_expired_wallet_cannot_admit_before_renewal(self):
        self.grant()
        with self.assertRaisesRegex(sqlite3.IntegrityError,'allowance_expired'):
            self.reserve('late',1,100)

    def test_global_free_subsidy_cap_is_atomic_and_replays_remain_readable(self):
        self.grant(cap=100)
        self.grant(cap=100)
        self.account('b')
        with self.assertRaisesRegex(sqlite3.IntegrityError,'trial_capacity_exhausted'):
            self.grant(account='b',cap=100)
        self.assertEqual(self.db.execute('SELECT COUNT(*) FROM allowance_grants').fetchone()[0],1)
        self.grant(account='b',amount=800,source='billing:paid')
        self.assertEqual(self.db.execute('SELECT COUNT(*) FROM allowance_grants').fetchone()[0],2)

    def test_stale_concurrent_transition_cannot_burn_new_credit(self):
        self.grant()
        self.grant(start=100,end=200,now=100,expected=0)
        self.reserve('new',10,101)
        self.grant(start=101,end=200,now=101,expected=0)
        self.assertEqual(self.balance(),90)

    def test_financial_grants_are_immutable(self):
        self.grant()
        with self.assertRaisesRegex(sqlite3.IntegrityError,'immutable_allowance_grant'):
            self.db.execute('DELETE FROM allowance_grants')

    def test_concurrent_free_accounts_cannot_exceed_global_subsidy(self):
        self.account('b')
        self.db.commit()
        temp = ROOT / '.temp'
        temp.mkdir(exist_ok=True)
        with tempfile.TemporaryDirectory(dir=temp) as directory:
            path = str(pathlib.Path(directory) / 'wallet.sqlite')
            disk = sqlite3.connect(path)
            self.db.backup(disk)
            disk.close()
            barrier = threading.Barrier(2)
            results = []
            def fund(account):
                db = sqlite3.connect(path,timeout=10)
                try:
                    barrier.wait()
                    db.execute(GRANT,(account,'0','100','free','100','0','-1','100'))
                    db.commit()
                    results.append('funded')
                except sqlite3.IntegrityError as error:
                    results.append(str(error))
                finally:
                    db.close()
            threads = [threading.Thread(target=fund,args=(account,)) for account in ('a','b')]
            for thread in threads: thread.start()
            for thread in threads: thread.join()
            self.assertCountEqual(results,['funded','trial_capacity_exhausted'])


if __name__ == '__main__':
    unittest.main()
