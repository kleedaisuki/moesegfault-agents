"""Compatibility of the shared batch path with actual legacy partial receipts."""
from __future__ import annotations

import unittest
from typing import Any

import test_d1_accounting_batch_sql as contract


class LegacyD1ReceiptTests(unittest.TestCase):
    def test_account_and_session_mixed_legacy_receipts_keep_original_charges(self) -> None:
        for session in (None, 'session'):
            for read_observed in (False, True):
                with self.subTest(session=session, read_observed=read_observed):
                    fixture: contract.D1AccountingBatchTests = contract.D1AccountingBatchTests()
                    fixture.setUp()
                    try:
                        fixture.reserve(session)
                        fixture.declare(session)
                        legacy: list[tuple[str, int]] = [('read', 11)]
                        if read_observed:
                            legacy.append(('write', 3))
                        for suffix, quantity in legacy:
                            args: tuple[Any, ...] = fixture.parameters(suffix, quantity, session)
                            fixture.db.execute(contract.SQL['APPEND'], args)
                        # Old record_observation wrote ledger first, then marked
                        # one side observed. A crash can preserve either boundary.
                        if read_observed:
                            fixture.db.execute("UPDATE resource_observations SET quantity='11',status='observed',observed_at=17 WHERE event_key='query:read'")
                        fixture.db.commit()
                        old_ledger: list[tuple[Any, ...]] = [tuple(row) for row in fixture.db.execute('SELECT * FROM usage_ledger ORDER BY event_key')]
                        fixture.declare(session)
                        rows: list[Any] = fixture.record(11, 3, session)
                        self.assertEqual([(row['status'], row['quantity']) for row in rows], [('observed', '11'), ('observed', '3')])
                        fixture.record(11, 3, session)
                        final_ledger: list[tuple[Any, ...]] = [tuple(row) for row in fixture.db.execute('SELECT * FROM usage_ledger ORDER BY event_key')]
                        self.assertEqual(len(final_ledger), 2)
                        for prior in old_ledger:
                            self.assertIn(prior, final_ledger)
                        if read_observed:
                            self.assertEqual(fixture.db.execute("SELECT observed_at FROM resource_observations WHERE event_key='query:read'").fetchone()[0], 17)
                        fixture.db.execute("UPDATE usage_reservations SET state='settled',settled_microusd=14 WHERE id='run'")
                        self.assertEqual(fixture.db.execute('SELECT reserved_microusd,spent_microusd FROM usage_accounts WHERE account_id=\'alice\'').fetchone()[0:2], (0, 14))
                    finally:
                        fixture.tearDown()


if __name__ == '__main__':
    unittest.main()
