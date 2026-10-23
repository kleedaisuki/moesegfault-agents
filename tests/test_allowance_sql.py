"""Real SQLite wallet journeys using the production single-statement grant SQL."""
import pathlib
import re
import sqlite3
import tempfile
import threading
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
GRANT = re.search(r'const GRANT: &str = "(.*?)";', (ROOT / 'crates/agents-worker/src/allowances.rs').read_text(), re.S).group(1)
REPLACE = re.search(r'const REPLACE: &str = "(.*?)";', (ROOT / 'crates/agents-worker/src/allowances.rs').read_text(), re.S).group(1)


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

    def replace(self, paid_start=20, paid_end=120, amount=800, now=20):
        self.db.execute(REPLACE,('a','0',str(paid_start),str(paid_end),'billing:synthetic',str(amount),str(now)))

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

    def test_first_paid_replaces_free_and_keeps_spend_and_holds_inside_cap(self):
        self.grant()
        self.reserve('used',30)
        self.db.execute("INSERT INTO usage_ledger VALUES('event','used','event','model',1,'request',20,'v1',NULL,2)")
        self.db.execute("UPDATE usage_reservations SET state='settled',settled_microusd=20,updated_at=2 WHERE id='used'")
        self.reserve('held',40,3)
        vintage = self.db.execute("SELECT * FROM reservation_vintages WHERE reservation_id='held'").fetchone()
        self.replace()
        self.assertEqual(self.balance(),740)
        self.replace()
        self.assertEqual(self.balance(),740)
        self.assertEqual(self.db.execute("SELECT * FROM reservation_vintages WHERE reservation_id='held'").fetchone(),vintage)
        self.db.execute("UPDATE usage_reservations SET state='released',updated_at=25 WHERE id='held'")
        self.assertEqual(self.balance(),780)
        self.assertEqual(self.db.execute('SELECT COUNT(*) FROM allowance_replacements').fetchone()[0],1)

    def test_replacement_old_hold_settlement_and_expiry_never_inflate(self):
        self.grant()
        self.reserve('held',80)
        self.replace()
        self.db.execute("INSERT INTO usage_ledger VALUES('event','held','event','model',1,'request',30,'v1',NULL,30)")
        self.db.execute("UPDATE usage_reservations SET state='settled',settled_microusd=30,updated_at=30 WHERE id='held'")
        self.assertEqual(self.balance(),770)
        self.reserve('expire',60,40)
        self.grant(start=120,end=200,amount=100,now=120)
        self.db.execute("UPDATE usage_reservations SET state='released',updated_at=130 WHERE id='expire'")
        self.assertEqual(self.balance(),100)

    def test_replacement_upgrade_uses_paid_anchor_not_former_free_anchor(self):
        self.grant()
        self.replace(paid_start=20,paid_end=120,amount=800)
        self.grant(start=0,end=120,source='billing:synthetic',amount=1800,now=70)
        self.assertEqual(self.balance(),1300)  # 800 + (1800-800)*50/100

    def test_replacement_journal_cannot_be_mutated(self):
        self.grant()
        self.replace()
        with self.assertRaisesRegex(sqlite3.IntegrityError,'immutable_allowance_replacement'):
            self.db.execute('DELETE FROM allowance_replacements')

    def test_short_paid_term_does_not_repeat_free_calendar_allocation(self):
        self.grant(start=0,end=1000,amount=1000000,cap=1000000)
        self.replace(paid_start=100,paid_end=900,amount=8000000,now=100)
        self.grant(start=900,end=1000,amount=1000000,now=900,source='free',cap=1000000)
        self.assertEqual(self.balance(),0)
        self.grant(start=1000,end=2000,amount=1000000,now=1000,source='free',cap=2000000)
        self.assertEqual(self.balance(),1000000)

    def test_legacy_lower_current_tier_retires_old_holds_instead_of_handoff(self):
        self.grant(start=0,end=1000,amount=1000000)
        self.grant(start=0,end=1000,amount=180000000,now=733)
        self.reserve('old-hold',9000000)
        self.replace(paid_start=700,paid_end=7000,amount=8000000,now=800)
        self.assertEqual(self.db.execute('SELECT COUNT(*) FROM allowance_replacements').fetchone()[0],0)
        self.grant(start=700,end=7000,source='billing:synthetic',amount=8000000,now=800)
        self.assertEqual(self.balance(),8000000)
        self.db.execute("UPDATE usage_reservations SET state='released',updated_at=900 WHERE id='old-hold'")
        self.assertEqual(self.balance(),8000000)
        self.grant(start=700,end=7000,source='billing:synthetic',amount=180000000,now=3850)
        self.assertEqual(self.balance(),94000000)

    def test_legacy_october_pin_reconciles_once_then_waits_until_paid_renewal(self):
        from datetime import datetime
        def stamp(value): return int(datetime.fromisoformat(value).timestamp())
        first=stamp('2026-10-01T00:00:00+00:00')
        november=stamp('2026-11-01T00:00:00+00:00')
        activated=stamp('2026-10-23T17:33:18+00:00')
        expires=activated+30*86400
        self.grant(start=first,end=november,amount=1000000,now=activated-60)
        self.grant(start=first,end=november,amount=180000000,now=activated)
        self.assertEqual(self.balance(),48744159)
        args=('a',str(first),str(activated),str(expires),'billing:synthetic','180000000',str(activated+60))
        self.db.execute(REPLACE,args)
        self.assertEqual(self.balance(),180000000)
        self.db.execute(REPLACE,args)
        self.grant(start=first,end=expires,source='billing:synthetic',amount=180000000,now=november)
        self.assertEqual(self.balance(),180000000)
        self.assertEqual(self.db.execute('SELECT COUNT(*) FROM allowance_replacements').fetchone()[0],1)
        delta=self.db.execute('SELECT delta_microusd FROM allowance_replacements').fetchone()[0]
        granted=self.db.execute('SELECT SUM(credited_microusd) FROM allowance_grants').fetchone()[0]
        self.assertEqual(delta+granted,180000000)
        self.grant(start=expires,end=stamp('2026-12-01T00:00:00+00:00'),now=expires,amount=1000000)
        self.assertEqual(self.balance(),1000000)

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
