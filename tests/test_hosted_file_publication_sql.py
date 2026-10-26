"""Hosted File publication retries respect accepted deletion, not failed uploads."""
from __future__ import annotations

import hashlib
import pathlib
import re
import sqlite3
import unittest

ROOT: pathlib.Path = pathlib.Path(__file__).resolve().parents[1]
SOURCE: str = (ROOT / "crates/agents-worker/src/space.rs").read_text(encoding="utf-8")


def query(prefix: str) -> str:
    return next(sql for sql in re.findall(r'\.prepare\("([^"\n]+)"\)', SOURCE) if sql.startswith(prefix))


ADMIT: str = query("INSERT INTO space_files(")
DELETED: str = query("SELECT id FROM space_files WHERE account_id=? AND inline_key=? AND upload_outcome='stored'")
columns_match: re.Match[str] | None = re.search(r'const FILE_COLUMNS: &str = "([^"]+)";', SOURCE)
assert columns_match is not None
COLUMNS: str = columns_match.group(1)
ACTIVE: str = next(sql for sql in re.findall(r'\.prepare\(format!\("([^"\n]+)"\)\)', SOURCE)
                   if sql.startswith("SELECT {FILE_COLUMNS} FROM space_files WHERE account_id=? AND inline_key=? AND status IN"))
ACTIVE = ACTIVE.replace("{FILE_COLUMNS}", COLUMNS)
GUARD: str = "(CAST(?15 AS INTEGER)=0 OR NOT EXISTS(SELECT 1 FROM space_files WHERE account_id=?2 AND inline_key=?14 AND upload_outcome='stored' AND status IN ('deleting','deleted')))"


class HostedFilePublicationTests(unittest.TestCase):
    def setUp(self) -> None:
        self.db: sqlite3.Connection = sqlite3.connect(":memory:")
        self.db.row_factory = sqlite3.Row
        self.db.execute("PRAGMA foreign_keys=ON")
        for migration in sorted((ROOT / "migrations").glob("*.sql")):
            self.db.executescript(migration.read_text(encoding="utf-8"))
        for owner in ("a", "b"):
            self.db.execute("INSERT INTO accounts VALUES(?,?,?,0,0)", (owner, "issuer", owner))
            self.db.execute("INSERT INTO spaces VALUES(?,?,'Space',0,0)", (f"space-{owner}", owner))

    def tearDown(self) -> None:
        self.db.close()

    @staticmethod
    def publication(key: str) -> str:
        return hashlib.sha256(key.encode()).hexdigest()

    def seed(self, identity: str, status: str, outcome: str, *, key: str = "result", owner: str = "a") -> None:
        operations: int | None = None if outcome == "pending" else int(outcome == "stored")
        self.db.execute("INSERT INTO space_files(id,account_id,space_id,filename,purpose,content_type,object_key,bytes,sha256,status,reservation_id,created_at_ms,updated_at_ms,recovery_at_ms,inline_key,upload_outcome,upload_operations) VALUES(?,?,?,'result.csv','user_data','text/csv',?,7,?,?,?,0,0,0,?,?,?)",
                        (identity, owner, f"space-{owner}", f"spaces/{owner}/{identity}", "ab" * 32, status, f"upload:{identity}", self.publication(key), outcome, operations))

    def admit(self, identity: str, *, strict: bool = True, key: str = "result", owner: str = "a", sql: str = ADMIT) -> sqlite3.Row | None:
        return self.db.execute(sql, (
            identity, owner, f"space-{owner}", "result.csv", "user_data", "text/csv", f"spaces/{owner}/{identity}", "7", "ab" * 32,
            f"upload:{identity}", "100", "10000", "100", self.publication(key), "1" if strict else "0",
        )).fetchone()

    def test_accepted_legacy_delete_and_deleting_are_terminal_for_artifact_retry(self) -> None:
        for status in ("deleted", "deleting"):
            with self.subTest(status=status):
                self.seed(status, status, "stored", key=status)
                self.assertIsNotNone(self.db.execute(DELETED, ("a", self.publication(status))).fetchone())
                self.assertIsNone(self.admit(f"retry-{status}", key=status))
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM space_files").fetchone()[0], 2)

    def test_never_published_known_failures_remain_retryable(self) -> None:
        for status in ("failed", "deleted"):
            with self.subTest(status=status):
                self.seed(f"failed-{status}", status, "failed", key=status)
                self.assertIsNone(self.db.execute(DELETED, ("a", self.publication(status))).fetchone())
                self.assertIsNotNone(self.admit(f"retry-{status}", key=status))
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM space_files WHERE status='uploading'").fetchone()[0], 2)

    def test_pending_and_processed_results_return_the_existing_publication(self) -> None:
        for status, outcome in (("uploading", "pending"), ("processed", "stored")):
            with self.subTest(status=status):
                self.seed(status, status, outcome, key=status)
                self.assertIsNone(self.admit(f"retry-{status}", key=status))
                existing: sqlite3.Row = self.db.execute(ACTIVE, ("a", self.publication(status))).fetchone()
                self.assertEqual(existing["id"], status)
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM space_files").fetchone()[0], 2)

    def test_regular_inline_input_keeps_its_existing_republication_behavior(self) -> None:
        self.seed("deleted", "deleted", "stored")
        self.assertIsNotNone(self.admit("input-retry", strict=False))
        normal: str = SOURCE[SOURCE.index("pub async fn save_bytes_idempotent("):SOURCE.index("pub(crate) async fn save_artifact_bytes_idempotent(")]
        artifact: str = SOURCE[SOURCE.index("pub(crate) async fn save_artifact_bytes_idempotent("):SOURCE.index("async fn save_bytes_with_policy(")]
        self.assertIn("Publication::Input", normal)
        self.assertIn("Publication::Artifact", artifact)

    def test_owner_and_publication_keys_scope_the_tombstone(self) -> None:
        self.seed("deleted", "deleted", "stored")
        self.assertIsNotNone(self.admit("other-owner", owner="b"))
        self.assertIsNotNone(self.admit("other-key", key="different-result"))

    def test_atomic_guard_closes_lookup_delete_insert_race_and_ablation_resurrects(self) -> None:
        self.seed("accepted", "processed", "stored")
        self.assertIsNone(self.db.execute(DELETED, ("a", self.publication("result"))).fetchone())
        # User deletion commits between the helper's initial check and admission.
        self.db.execute("UPDATE space_files SET status='deleted' WHERE id='accepted'")
        self.assertIsNone(self.db.execute(ACTIVE, ("a", self.publication("result"))).fetchone())
        self.assertIsNone(self.admit("strict-retry"))
        self.assertIn(GUARD, ADMIT)
        ablated: str = ADMIT.replace(GUARD, "(CAST(?15 AS INTEGER)=0 OR 1)")
        self.assertIsNotNone(self.admit("unsafe-retry", sql=ablated))

    def test_failed_lineage_does_not_mask_a_later_accepted_deletion(self) -> None:
        self.seed("failed-attempt", "failed", "failed")
        self.seed("accepted-attempt", "deleted", "stored")
        self.assertIsNone(self.admit("retry"))
        self.assertEqual(self.db.execute(DELETED, ("a", self.publication("result"))).fetchone()["id"], "accepted-attempt")

    def test_historical_tombstone_lookup_is_owner_indexed_and_maps_to_terminal_code(self) -> None:
        plan: str = str([tuple(row) for row in self.db.execute("EXPLAIN QUERY PLAN " + DELETED, ("a", self.publication("result")))])
        self.assertIn("space_files_publication", plan)
        self.assertNotIn("SCAN space_files", plan)
        self.assertIn('(409, Some("file_publication_deleted")) => "file_publication_deleted"', SOURCE)

    def test_additive_index_migration_upgrades_the_already_applied_blob_schema(self) -> None:
        self.db.execute("DROP INDEX space_files_publication")
        migration: str = (ROOT / "migrations/0020_code_artifact_publication.sql").read_text(encoding="utf-8")
        self.db.executescript(migration)
        self.db.executescript(migration)
        columns: list[str] = [row[2] for row in self.db.execute("PRAGMA index_info(space_files_publication)")]
        self.assertEqual(columns, ["account_id", "inline_key", "upload_outcome", "status"])
        plan: str = str([tuple(row) for row in self.db.execute("EXPLAIN QUERY PLAN " + DELETED, ("a", self.publication("result")))])
        self.assertIn("space_files_publication", plan)


if __name__ == "__main__":
    unittest.main()
