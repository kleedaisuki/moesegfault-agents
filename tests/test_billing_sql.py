"""Database-level user journeys: metering, retry, concurrency, and immutable audit."""

import pathlib
from contextlib import closing
import sqlite3
import tempfile
import threading
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
SCHEMA = ROOT / "migrations/0001_initial.sql"
TEMP = ROOT / ".temp"
TEMP.mkdir(exist_ok=True)


def setup(db):
    db.executescript(SCHEMA.read_text(encoding="utf-8"))
    for migration in sorted(SCHEMA.parent.glob("*.sql")):
        if migration != SCHEMA:
            db.executescript(migration.read_text(encoding="utf-8"))
    db.execute("INSERT INTO accounts VALUES ('a','issuer','subject',0,0)")
    db.execute("INSERT INTO usage_accounts VALUES ('a',100,0,0,0)")
    db.commit()


def reserve(db, key, amount):
    db.execute("""INSERT INTO usage_reservations
      (id,account_id,idempotency_key,request_hash,reserved_microusd,state,created_at,updated_at)
      SELECT ?,'a',?,'hash',?,'reserved',0,0 WHERE NOT EXISTS
      (SELECT 1 FROM usage_reservations WHERE account_id='a' AND idempotency_key=?)""",
      (key, key, amount, key))


def charge(db, event, amount):
    db.execute("""INSERT INTO usage_ledger
      (id,reservation_id,event_key,resource,quantity,unit,amount_microusd,price_version,created_at)
      SELECT ?,'r',?,'model',1,'request',?,'v1',0 WHERE NOT EXISTS
      (SELECT 1 FROM usage_ledger WHERE reservation_id='r' AND event_key=?)""",
      (event, event, amount, event))


def settle(db):
    db.execute("""UPDATE usage_reservations SET state='settled',settled_microusd=
      (SELECT COALESCE(SUM(amount_microusd),0) FROM usage_ledger WHERE reservation_id='r')
      WHERE id='r' AND state='reserved'""")


class BillingSqlTests(unittest.TestCase):
    def setUp(self):
        self.db = sqlite3.connect(":memory:")
        setup(self.db)

    def tearDown(self):
        self.db.close()

    def test_real_usage_retry_settlement_journey(self):
        reserve(self.db, "r", 80)
        reserve(self.db, "r", 80)
        charge(self.db, "model-1", 30)
        charge(self.db, "model-1", 30)
        settle(self.db)
        settle(self.db)
        charge(self.db, "model-1", 30)
        self.assertEqual(self.db.execute("SELECT spent_microusd,reserved_microusd FROM usage_accounts").fetchone(), (30, 0))
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM usage_ledger").fetchone(), (1,))

    def test_over_budget_rolls_back_entire_insert(self):
        reserve(self.db, "r", 80)
        with self.assertRaises(sqlite3.IntegrityError):
            reserve(self.db, "s", 21)
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM usage_reservations").fetchone(), (1,))
        self.assertEqual(self.db.execute("SELECT reserved_microusd FROM usage_accounts").fetchone(), (80,))

    def test_cannot_release_recorded_usage_or_mutate_audit(self):
        reserve(self.db, "r", 80)
        charge(self.db, "e", 30)
        for sql in ["UPDATE usage_reservations SET state='released' WHERE id='r'",
                    "DELETE FROM usage_ledger", "UPDATE usage_ledger SET amount_microusd=0",
                    "UPDATE usage_reservations SET reserved_microusd=100",
                    "UPDATE usage_reservations SET state='settled',settled_microusd=0"]:
            with self.subTest(sql=sql), self.assertRaises(sqlite3.IntegrityError):
                self.db.execute(sql)
        settle(self.db)
        with self.assertRaises(sqlite3.IntegrityError):
            self.db.execute("UPDATE usage_reservations SET settled_microusd=0")

    def test_integer_money_stays_exact_above_javascript_safe_integer(self):
        amount = 9007199254740993
        self.db.execute("UPDATE usage_accounts SET budget_microusd=?", (amount,))
        reserve(self.db, "r", amount)
        charge(self.db, "e", amount)
        settle(self.db)
        self.assertEqual(self.db.execute("SELECT CAST(spent_microusd AS TEXT) FROM usage_accounts").fetchone(), (str(amount),))
        with self.assertRaises(sqlite3.IntegrityError):
            self.db.execute("UPDATE usage_accounts SET budget_microusd=1e20")

    def test_two_concurrent_admissions_cannot_overspend(self):
        with tempfile.TemporaryDirectory(dir=TEMP) as directory:
            path = pathlib.Path(directory) / "billing.sqlite"
            with closing(sqlite3.connect(path)) as db:
                setup(db)
            barrier = threading.Barrier(2)
            outcomes = []

            def attempt(key):
                with closing(sqlite3.connect(path, timeout=5)) as db:
                    barrier.wait()
                    try:
                        reserve(db, key, 80)
                        db.commit()
                        outcomes.append("accepted")
                    except sqlite3.IntegrityError:
                        outcomes.append("rejected")

            threads = [threading.Thread(target=attempt, args=(str(i),)) for i in range(2)]
            for thread in threads:
                thread.start()
            for thread in threads:
                thread.join()
            self.assertCountEqual(outcomes, ["accepted", "rejected"])
            with closing(sqlite3.connect(path)) as db:
                self.assertEqual(db.execute("SELECT reserved_microusd FROM usage_accounts").fetchone(), (80,))

    def test_unknown_resource_usage_holds_budget_until_observed(self):
        self.db.execute("INSERT INTO agents(id,account_id,name,config_json,created_at,updated_at) VALUES('agent','a','test','{}',0,0)")
        self.db.execute("INSERT INTO sessions(id,account_id,agent_id,do_id,status,config_snapshot_json,instructions_hash,created_at,last_input_at) VALUES('session','a','agent','do','active','{}','hash',0,0)")
        self.db.execute("INSERT INTO usage_reservations(id,account_id,idempotency_key,request_hash,session_id,reserved_microusd,state,created_at,updated_at) VALUES('r','a','run:r','hash','session',80,'reserved',0,0)")
        self.db.execute("INSERT INTO resource_observations VALUES('r','a','session','r2:write','r2_operation','class_a',NULL,'v1',NULL,'pending')")
        with self.assertRaisesRegex(sqlite3.IntegrityError, "resource_observation_pending"):
            settle(self.db)
        self.assertEqual(self.db.execute("SELECT reserved_microusd FROM usage_accounts").fetchone(), (80,))
        charge(self.db, "r2:write", 2)
        self.db.execute("UPDATE resource_observations SET status='observed',quantity='1',observed_at=1 WHERE run_id='r'")
        settle(self.db)
        self.assertEqual(self.db.execute("SELECT spent_microusd,reserved_microusd FROM usage_accounts").fetchone(), (2,0))
        with self.assertRaisesRegex(sqlite3.IntegrityError, "immutable_resource_observation"):
            self.db.execute("UPDATE resource_observations SET quantity='2' WHERE run_id='r'")

    def test_run_cap_is_atomic_with_reservation_and_retries(self):
        statement = """INSERT INTO usage_reservations
          (id,account_id,idempotency_key,request_hash,reserved_microusd,state,created_at,updated_at)
          SELECT ?,'a',?,'hash',10,'reserved',0,0 WHERE NOT EXISTS
          (SELECT 1 FROM usage_reservations WHERE account_id='a' AND idempotency_key=?)
          AND (SELECT COUNT(*) FROM usage_reservations WHERE account_id='a'
               AND state='reserved' AND idempotency_key LIKE 'run:%') < 1"""
        for key in ["run:a", "run:a", "run:b"]:
            self.db.execute(statement, (key, key, key))
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM usage_reservations").fetchone(), (1,))
        self.db.execute("UPDATE usage_reservations SET state='released' WHERE id='run:a'")
        self.db.execute(statement, ("run:b", "run:b", "run:b"))
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM usage_reservations WHERE state='reserved'").fetchone(), (1,))
        self.assertEqual(self.db.execute("SELECT reserved_microusd FROM usage_accounts").fetchone(), (10,))

    def test_retained_storage_has_one_recoverable_pinned_interval(self):
        self.db.execute("INSERT INTO agents(id,account_id,name,config_json,created_at,updated_at) VALUES('agent','a','test','{}',0,0)")
        self.db.execute("INSERT INTO sessions(id,account_id,agent_id,do_id,status,config_snapshot_json,instructions_hash,created_at,last_input_at) VALUES('session','a','agent','do','active','{}','hash',0,0)")
        self.db.execute("INSERT INTO resource_storage_objects(object_key,account_id,session_id,size_bytes,retained_from_ms,billed_through_ms) VALUES('object','a','session','1024',0,0)")
        statement = """INSERT INTO resource_storage_deficits(object_key,account_id,session_id,interval_start_ms,interval_end_ms,quantity,amount_microusd,price_version,tariff_json,reservation_id) VALUES('object','a','session',?,?,'1024000','2','v1','{}',?)"""
        self.db.execute(statement,(0,1000,'r1'))
        with self.assertRaises(sqlite3.IntegrityError):
            self.db.execute(statement,(1000,2000,'r2'))
        self.db.execute("UPDATE resource_storage_deficits SET status='settled' WHERE reservation_id='r1'")
        self.db.execute("UPDATE resource_storage_objects SET billed_through_ms=1000 WHERE object_key='object'")
        self.db.execute(statement,(1000,2000,'r2'))
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM resource_storage_deficits WHERE status='pending'").fetchone(),(1,))
        with self.assertRaisesRegex(sqlite3.IntegrityError,'immutable_storage_charge'):
            self.db.execute("UPDATE resource_storage_deficits SET amount_microusd='0' WHERE reservation_id='r1'")


if __name__ == "__main__":
    unittest.main()
