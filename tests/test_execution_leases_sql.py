"""Real SQLite admission and cold-loss execution-lease properties, without provider calls."""
from __future__ import annotations

from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
import re
import sqlite3
import tempfile
import unittest

ROOT: Path = Path(__file__).resolve().parents[1]
SOURCE: str = (ROOT / 'crates/agents-worker/src/execution_leases.rs').read_text(encoding='utf-8')
ADMIT: str = re.search(r'const ADMIT_SQL: &str = "([^"]+)"', SOURCE).group(1)  # type: ignore[union-attr]
UNUSED: str = re.search(r'const UNUSED_SQL: &str = "([^"]+)"', SOURCE).group(1)  # type: ignore[union-attr]
RELEASE: str = re.search(r'const RELEASE_SQL: &str = "([^"]+)"', SOURCE).group(1)  # type: ignore[union-attr]


def initialize(db: sqlite3.Connection) -> None:
    db.execute('PRAGMA foreign_keys=ON')
    for migration in sorted((ROOT / 'migrations').glob('*.sql')):
        db.executescript(migration.read_text(encoding='utf-8'))
    for owner in ('a', 'b'):
        db.execute('INSERT INTO accounts VALUES (?,?,?,0,0)', (owner, 'issuer', owner))
        db.execute('INSERT INTO usage_accounts VALUES (?,1000,0,0,0)', (owner,))
    db.commit()


def admit(db: sqlite3.Connection, run: str, generation: str | None, owner: str = 'a', amount: int = 30) -> int:
    key: str = 'run:' + run
    return db.execute(ADMIT, (run, owner, None, key, 'original-hash', str(amount), '1', '1',
                              generation, owner, key, '1', owner, '1')).rowcount


def release(db: sqlite3.Connection, run: str, generation: str, owner: str = 'a') -> int:
    return db.execute(RELEASE, ('12345', run, owner, generation)).rowcount


class ExecutionLeaseSqlTests(unittest.TestCase):
    db: sqlite3.Connection

    def setUp(self) -> None:
        self.db = sqlite3.connect(':memory:')
        initialize(self.db)

    def tearDown(self) -> None:
        self.db.close()

    def test_cold_fenced_operation_releases_capacity_not_unknown_money(self) -> None:
        self.assertEqual(admit(self.db, 'cold', 'generation1'), 1)
        self.db.execute("INSERT INTO usage_reservations(id,account_id,idempotency_key,request_hash,reserved_microusd,state,created_at,updated_at) VALUES ('model','a','completion:cold:compact','modelhash',20,'reserved',1,1)")
        self.assertEqual(admit(self.db, 'next', 'generation2'), 0)
        # Actor persisted Failed/no-local-I/O evidence before this exact-generation release.
        self.assertEqual(release(self.db, 'cold', 'generation1'), 1)
        self.assertEqual(release(self.db, 'cold', 'generation1'), 0)
        self.assertEqual(self.db.execute("SELECT state,execution_finished_at,settled_microusd FROM usage_reservations WHERE id='cold'").fetchone(), ('reserved', None, None))
        self.assertEqual(self.db.execute("SELECT reserved_microusd,spent_microusd FROM usage_accounts WHERE account_id='a'").fetchone(), (50, 0))
        self.assertEqual(self.db.execute('SELECT COUNT(*) FROM usage_ledger').fetchone(), (0,))
        self.assertEqual(admit(self.db, 'next', 'generation2'), 1)
        self.assertEqual(self.db.execute("SELECT reserved_microusd FROM usage_accounts WHERE account_id='a'").fetchone(), (80,))

    def test_stale_generation_and_wrong_owner_cannot_release_active_execution(self) -> None:
        admit(self.db, 'active', 'current')
        self.assertEqual(release(self.db, 'active', 'old'), 0)
        self.assertEqual(release(self.db, 'active', 'current', 'b'), 0)
        self.assertEqual(admit(self.db, 'blocked', 'next'), 0)
        self.assertEqual(self.db.execute('SELECT state FROM execution_leases').fetchone(), ('active',))

    def test_no_timeout_releases_healthy_work(self) -> None:
        admit(self.db, 'healthy', 'live')
        self.db.execute("UPDATE usage_reservations SET updated_at=999999999 WHERE id='healthy'")
        self.assertEqual(admit(self.db, 'blocked', 'next'), 0)
        self.assertEqual(self.db.execute('SELECT released_at_ms FROM execution_leases').fetchone(), (None,))

    def test_legacy_missing_lease_conservatively_retains_capacity(self) -> None:
        admit(self.db, 'legacy', None)
        self.assertEqual(self.db.execute('SELECT COUNT(*) FROM execution_leases').fetchone(), (0,))
        self.assertEqual(release(self.db, 'legacy', 'invented'), 0)
        self.assertEqual(admit(self.db, 'blocked', 'next'), 0)

    def test_budget_failure_rolls_back_both_funding_and_execution_metadata(self) -> None:
        with self.assertRaisesRegex(sqlite3.IntegrityError, 'budget_exceeded'):
            admit(self.db, 'unfunded', 'generation', amount=1001)
        self.assertEqual(self.db.execute('SELECT COUNT(*) FROM execution_leases').fetchone(), (0,))
        self.assertEqual(self.db.execute('SELECT COUNT(*) FROM usage_reservations').fetchone(), (0,))
        self.assertEqual(self.db.execute("SELECT reserved_microusd FROM usage_accounts WHERE account_id='a'").fetchone(), (0,))

    def test_generation_and_terminal_state_are_immutable(self) -> None:
        admit(self.db, 'cold', 'generation')
        with self.assertRaises(sqlite3.IntegrityError):
            self.db.execute("UPDATE usage_reservations SET execution_generation='other' WHERE id='cold'")
        with self.assertRaises(sqlite3.IntegrityError):
            self.db.execute("UPDATE execution_leases SET generation='other' WHERE reservation_id='cold'")
        release(self.db, 'cold', 'generation')
        with self.assertRaises(sqlite3.IntegrityError):
            self.db.execute("UPDATE execution_leases SET state='active',released_at_ms=NULL WHERE reservation_id='cold'")
        self.assertEqual(admit(self.db, 'cold', 'new-generation'), 0)
        self.assertEqual(self.db.execute('SELECT generation,state FROM execution_leases').fetchone(), ('generation', 'no_local_execution'))

    def test_predispatch_rejection_releases_only_proven_unused_holds(self) -> None:
        admit(self.db, 'rejected', 'generation')
        self.db.execute("INSERT INTO usage_reservations(id,account_id,idempotency_key,request_hash,reserved_microusd,state,created_at,updated_at) VALUES ('model','a','completion:rejected:compact','hash',20,'reserved',1,1)")
        release(self.db, 'rejected', 'generation')
        self.db.execute(UNUSED, ('20','a','rejected','completion:rejected:compact','rejected','a','generation'))
        self.assertEqual(self.db.execute("SELECT reserved_microusd,spent_microusd FROM usage_accounts WHERE account_id='a'").fetchone(), (0,0))
        self.assertEqual(self.db.execute('SELECT COUNT(*) FROM usage_ledger').fetchone(), (0,))
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM usage_reservations WHERE state='released'").fetchone(), (2,))

    def test_pending_resource_observation_retains_money_after_predispatch_rejection(self) -> None:
        admit(self.db, 'rejected', 'generation')
        self.db.execute("INSERT INTO resource_observations(run_id,account_id,session_id,event_key,resource,unit,quantity,price_version,observed_at,status) VALUES ('rejected','a',NULL,'compact.actor','durable_object','wall_ms',NULL,'test',NULL,'pending')")
        release(self.db, 'rejected', 'generation')
        self.db.execute(UNUSED, ('20','a','rejected','completion:rejected:compact','rejected','a','generation'))
        self.assertEqual(self.db.execute("SELECT state FROM usage_reservations WHERE id='rejected'").fetchone(), ('reserved',))
        self.assertEqual(self.db.execute("SELECT reserved_microusd FROM usage_accounts WHERE account_id='a'").fetchone(), (30,))
        self.assertEqual(admit(self.db, 'next', 'next-generation'), 1)

    def test_stale_generation_cannot_refund_unused_holds(self) -> None:
        admit(self.db, 'rejected', 'current')
        self.db.execute(UNUSED, ('20','a','rejected','completion:rejected:compact','rejected','a','old'))
        self.assertEqual(self.db.execute("SELECT reserved_microusd FROM usage_accounts WHERE account_id='a'").fetchone(), (30,))

    def test_concurrent_admission_shares_one_slot_and_one_funded_hold(self) -> None:
        (ROOT / '.temp').mkdir(exist_ok=True)
        with tempfile.TemporaryDirectory(prefix='execution-lease-', dir=ROOT / '.temp') as directory:
            path: Path = Path(directory) / 'leases.sqlite'
            db: sqlite3.Connection = sqlite3.connect(path)
            initialize(db)
            db.close()

            def candidate(index: int) -> int:
                connection: sqlite3.Connection = sqlite3.connect(path, timeout=10)
                try:
                    result: int = admit(connection, f'run{index}', f'gen{index}')
                    connection.commit()
                    return result
                finally:
                    connection.close()

            with ThreadPoolExecutor(max_workers=6) as pool:
                results: list[int] = list(pool.map(candidate, range(6)))
            self.assertEqual(sum(results), 1)
            check: sqlite3.Connection = sqlite3.connect(path)
            try:
                self.assertEqual(check.execute('SELECT COUNT(*) FROM execution_leases').fetchone(), (1,))
                self.assertEqual(check.execute("SELECT reserved_microusd FROM usage_accounts WHERE account_id='a'").fetchone(), (30,))
            finally:
                check.close()


class ExecutionLeaseUpgradeTests(unittest.TestCase):
    def test_deployed_reservations_survive_the_additive_generation_upgrade(self) -> None:
        db: sqlite3.Connection = sqlite3.connect(':memory:')
        try:
            db.execute('PRAGMA foreign_keys=ON')
            for migration in sorted((ROOT / 'migrations').glob('*.sql')):
                number: int = int(migration.name.split('_')[0])
                if number <= 18 or number == 20:
                    db.executescript(migration.read_text(encoding='utf-8'))
            db.execute('INSERT INTO accounts VALUES (?,?,?,0,0)', ('a', 'issuer', 'a'))
            db.execute('INSERT INTO usage_accounts VALUES (?,1000,0,0,0)', ('a',))
            db.execute("INSERT INTO usage_reservations(id,account_id,idempotency_key,request_hash,reserved_microusd,state,created_at,updated_at) VALUES ('legacy','a','run:legacy','original-price-hash',30,'reserved',1,1)")
            before: list[tuple[object, ...]] = db.execute('SELECT id,account_id,request_hash,reserved_microusd,state,execution_finished_at FROM usage_reservations').fetchall()
            wallet: list[tuple[object, ...]] = db.execute('SELECT * FROM usage_accounts').fetchall()
            db.commit()
            db.executescript((ROOT / 'migrations/0019_execution_leases.sql').read_text(encoding='utf-8'))
            self.assertEqual(db.execute('SELECT id,account_id,request_hash,reserved_microusd,state,execution_finished_at FROM usage_reservations').fetchall(), before)
            self.assertEqual(db.execute('SELECT * FROM usage_accounts').fetchall(), wallet)
            self.assertEqual(db.execute('SELECT execution_generation FROM usage_reservations').fetchall(), [(None,)])
            self.assertEqual(db.execute('SELECT COUNT(*) FROM execution_leases').fetchone(), (0,))
            self.assertEqual(admit(db, 'new', 'generation'), 0, 'Unproven old execution stays conservative')
            self.assertEqual(release(db, 'legacy', 'invented'), 0, 'An invented generation cannot release old funds or capacity')
            self.assertEqual(db.execute('PRAGMA foreign_key_check').fetchall(), [])
        finally:
            db.close()
