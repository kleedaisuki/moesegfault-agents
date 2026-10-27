"""Exact production D1 accounting batches keep funding and ledger fences."""
from __future__ import annotations

import pathlib
import re
import sqlite3
import unittest
from typing import Any

ROOT: pathlib.Path = pathlib.Path(__file__).resolve().parents[1]
SOURCE: str = (ROOT / 'crates/agents-worker/src/resources_d1_batch.rs').read_text(encoding='utf-8')
SQL: dict[str, str] = dict(re.findall(r'const (\w+)_SQL: &str = "([^"\n]+)";', SOURCE))


class D1AccountingBatchTests(unittest.TestCase):
    def setUp(self) -> None:
        self.db: sqlite3.Connection = sqlite3.connect(':memory:')
        self.db.row_factory = sqlite3.Row
        for migration in sorted((ROOT / 'migrations').glob('*.sql')):
            self.db.executescript(migration.read_text(encoding='utf-8'))
        for owner in ('alice', 'bob'):
            self.db.execute('INSERT INTO accounts VALUES(?,?,?,?,?)', (owner, 'issuer', owner, 0, 0))
            self.db.execute('INSERT INTO usage_accounts VALUES(?,100000,0,0,0)', (owner,))
        self.db.execute("INSERT INTO agents VALUES('agent','alice','agent','{}',0,0,NULL)")
        self.db.execute("INSERT INTO sessions(id,account_id,agent_id,do_id,status,config_snapshot_json,instructions_hash,created_at,last_input_at) VALUES('session','alice','agent','do','idle','{}','pin',0,0)")
        self.db.commit()

    def tearDown(self) -> None:
        self.db.close()

    def reserve(self, session: str | None = None, budget: int = 1000) -> None:
        self.db.execute("INSERT INTO usage_reservations(id,account_id,session_id,idempotency_key,request_hash,reserved_microusd,state,created_at,updated_at) VALUES('run','alice',?,'key','hash',?,'reserved',0,0)", (session, budget))
        self.db.commit()

    def parameters(self, suffix: str, quantity: int = 0, session: str | None = None, owner: str = 'alice') -> tuple[Any, ...]:
        unit: str = 'rows_read' if suffix == 'read' else 'rows_written'
        key: str = 'query:' + suffix
        return ('run', owner, session, key, unit, 'v1', str(quantity), 'resource:' + key, 'ledger-' + key, str(quantity), '123')

    def declare(self, session: str | None = None, owner: str = 'alice') -> list[sqlite3.Row]:
        rows: list[sqlite3.Row] = []
        with self.db:
            for suffix in ('read', 'write'):
                args: tuple[Any, ...] = self.parameters(suffix, session=session, owner=owner)
                self.db.execute(SQL['DECLARE'], args[:6])
                rows.extend(self.db.execute(SQL['READ'], args[:4]).fetchall())
        return rows

    def record(self, read: int, written: int, session: str | None = None) -> list[sqlite3.Row]:
        rows: list[sqlite3.Row] = []
        with self.db:
            for suffix, quantity in (('read', read), ('write', written)):
                args: tuple[Any, ...] = self.parameters(suffix, quantity, session)
                self.db.execute(SQL['APPEND'], args)
                self.db.execute(SQL['OBSERVE'], args)
                rows.extend(self.db.execute(SQL['VERIFY'], args[:8]).fetchall())
        return rows

    def test_both_bindings_preserve_actual_quantities_and_replay_once(self) -> None:
        for session in (None, 'session'):
            with self.subTest(session=session):
                if session is not None:
                    self.tearDown()
                    self.setUp()
                self.reserve(session)
                self.assertEqual(len(self.declare(session)), 2)
                rows: list[sqlite3.Row] = self.record(11, 3, session)
                self.assertEqual([(r['quantity'], r['status'], r['ledger_quantity'], r['ledger_amount']) for r in rows], [('11', 'observed', '11', '11'), ('3', 'observed', '3', '3')])
                self.record(11, 3, session)
                self.assertEqual(self.db.execute('SELECT COUNT(*) FROM usage_ledger').fetchone()[0], 2)

    def test_missing_funding_and_cross_owner_session_declare_no_observations(self) -> None:
        self.assertEqual(self.declare(), [])
        self.reserve('session')
        self.assertEqual(self.declare(), [])
        self.assertEqual(self.declare('session', 'bob'), [])
        self.assertEqual(self.db.execute('SELECT COUNT(*) FROM resource_observations').fetchone()[0], 0)

    def test_unknown_result_keeps_original_pending_hold(self) -> None:
        self.reserve()
        self.declare()
        with self.assertRaisesRegex(sqlite3.IntegrityError, 'resource_observation_pending'):
            self.db.execute("UPDATE usage_reservations SET state='settled',settled_microusd=0")
        self.assertEqual(self.db.execute('SELECT COUNT(*) FROM usage_ledger').fetchone()[0], 0)

    def test_conflicting_charge_cannot_mark_observed_or_overwrite_ledger(self) -> None:
        self.reserve()
        self.declare()
        self.record(11, 3)
        self.assertEqual([(r['quantity'], r['ledger_quantity']) for r in self.record(99, 3)], [('11', '11'), ('3', '3')])
        self.assertEqual(self.db.execute('SELECT SUM(amount_microusd) FROM usage_ledger').fetchone()[0], 14)

    def test_existing_wrong_ledger_leaves_observation_pending(self) -> None:
        self.reserve()
        self.declare()
        self.db.execute("INSERT INTO usage_ledger VALUES('wrong','run','resource:query:read','d1',12,'rows_read',12,'v1',NULL,0)")
        self.db.commit()
        rows: list[sqlite3.Row] = self.record(11, 3)
        self.assertEqual((rows[0]['status'], rows[0]['quantity'], rows[0]['ledger_quantity']), ('pending', None, '12'))

    def test_budget_failure_rolls_back_both_ledger_and_observed_transitions(self) -> None:
        self.reserve(budget=12)
        self.declare()
        with self.assertRaisesRegex(sqlite3.IntegrityError, 'budget_exceeded'):
            self.record(11, 3)
        self.assertEqual(self.db.execute('SELECT COUNT(*) FROM usage_ledger').fetchone()[0], 0)
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM resource_observations WHERE status='pending'").fetchone()[0], 2)

    def test_zero_usage_has_real_zero_ledger_and_fixed_observation_timestamp(self) -> None:
        self.reserve()
        self.declare()
        self.record(0, 0)
        self.record(0, 0)
        self.assertEqual(self.db.execute('SELECT SUM(amount_microusd) FROM usage_ledger').fetchone()[0], 0)
        self.assertEqual([tuple(r) for r in self.db.execute('SELECT observed_at FROM resource_observations')], [(123,), (123,)])


if __name__ == '__main__':
    unittest.main()
