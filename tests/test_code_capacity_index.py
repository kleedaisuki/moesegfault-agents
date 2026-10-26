"""Code quota admission is indexed by retained capacity, not historical receipts."""
from __future__ import annotations

import sqlite3
import unittest

import test_code_context_sql as fixtures


class CodeCapacityIndexTests(unittest.TestCase):
    def test_both_capacity_queries_use_the_live_index_with_large_history(self) -> None:
        fixture: fixtures.CodeContextTests = fixtures.CodeContextTests(methodName="runTest")
        fixture.setUp()
        try:
            fixture.db.executemany(
                "INSERT INTO code_contexts(allocation_id,account_id,source_session_id,source_profile,funding_run_id,state,created_at_ms,last_used_at_ms,expires_at_ms,ended_at_ms,allocation_released_at_ms) VALUES(?,'a','a-0','basic1g','run-a-0','expired',0,0,1,1,1)",
                [(f"history-{ordinal}",) for ordinal in range(5000)],
            )
            parameters: tuple[str, ...] = ("new", "a", "a-0", "basic1g", "run-a-0", "2", "1200002", "2")
            plan: list[sqlite3.Row] = fixture.db.execute("EXPLAIN QUERY PLAN " + fixtures.CLAIM, parameters).fetchall()
            capacity: list[str] = [str(row["detail"]) for row in plan if "code_contexts_live_allocations" in row["detail"]]
            self.assertEqual(len(capacity), 2, [str(row["detail"]) for row in plan])
            self.assertFalse(any("TEMP B-TREE" in row["detail"] for row in plan))
            steps: int = 0

            def advance() -> int:
                nonlocal steps
                steps += 1
                return 0

            fixture.db.set_progress_handler(advance, 1)
            try:
                fixture.db.execute(fixtures.CLAIM, parameters)
            finally:
                fixture.db.set_progress_handler(None, 0)
            self.assertLess(steps, 2000, "Quota admission must not walk 5000 released historical allocations")
        finally:
            fixture.tearDown()


if __name__ == "__main__":
    unittest.main()
