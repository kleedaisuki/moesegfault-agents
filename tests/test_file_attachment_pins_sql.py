"""Exercise production attachment-retention SQL and both deletion orderings."""

from __future__ import annotations

import json
from pathlib import Path
import re
import sqlite3
import unittest

ROOT = Path(__file__).resolve().parents[1]
PINS = (ROOT / "crates/agents-worker/src/space_pins.rs").read_text(encoding="utf-8")
SPACE = (ROOT / "crates/agents-worker/src/space.rs").read_text(encoding="utf-8")


def query(source: str, prefix: str) -> str:
    matches = [sql for sql in re.findall(r'\.prepare\("([^"\n]+)"\)', source) if sql.startswith(prefix)]
    assert len(matches) == 1, (prefix, matches)
    return matches[0]


ACQUIRE = query(PINS, "INSERT INTO file_attachment_pins")
RELEASE = query(PINS, "DELETE FROM file_attachment_pins")
LIST = query(PINS, "SELECT DISTINCT response_id FROM file_attachment_pins")
CLAIM_DELETE = query(SPACE, "UPDATE space_files SET status='deleting',deletion_reason='user'")
CLAIM_ABORT = query(SPACE, "UPDATE space_files SET status='deleting',deletion_reason='upload_abort'")
PERMIT_DELETE = query(SPACE, "SELECT id FROM space_files WHERE id=? AND account_id=? AND status IN ('deleting','failed')")
FINISH_DELETE = query(SPACE, "UPDATE space_files SET status=CASE deletion_reason")
USER_FAILED_DELETE = query(SPACE, "UPDATE space_files SET deletion_reason='user'")


class FileAttachmentPinSqlTests(unittest.TestCase):
    def setUp(self) -> None:
        self.db: sqlite3.Connection = sqlite3.connect(":memory:")
        for migration in sorted((ROOT / "migrations").glob("*.sql")):
            self.db.executescript(migration.read_text(encoding="utf-8"))
        for owner in ("alice", "bob"):
            self.db.execute("INSERT INTO accounts VALUES(?, 'issuer', ?, 0, 0)", (owner, owner))
            self.db.execute("INSERT INTO agents VALUES(?,?,?,'{}',0,0,NULL)", (owner, owner, owner))
            self.db.execute("INSERT INTO sessions VALUES(?,?,?,?,'idle','{}','pin',0,0,NULL,NULL,NULL)",
                            (owner, owner, owner, "do-" + owner))
            self.db.execute("INSERT INTO spaces VALUES(?,?,?,0,0)", ("space-" + owner, owner, owner))
            self.route("resp-" + owner, owner)
            self.file("file-" + owner, owner)
        self.db.commit()

    def tearDown(self) -> None:
        self.db.close()

    def route(self, response: str, owner: str = "alice") -> None:
        self.db.execute("INSERT INTO response_routes VALUES(?,?,?,?,?,NULL,0)",
                        (response, owner, owner, owner, "request"))

    def file(self, file_id: str, owner: str = "alice", status: str = "processed") -> None:
        self.db.execute("""INSERT INTO space_files
            (id,account_id,space_id,filename,purpose,content_type,object_key,bytes,sha256,
             status,reservation_id,created_at_ms,updated_at_ms,recovery_at_ms,upload_outcome)
            VALUES(?,?,?,'input.bin','user_data','application/octet-stream',?,12,?,?,?,0,0,150000,?)""",
                        (file_id, owner, "space-" + owner, "objects/" + file_id, "ab" * 32,
                         status, "upload-" + file_id, "stored" if status == "processed" else "pending"))

    def reference(self, file_id: str) -> dict[str, str | int]:
        return {"id": file_id, "object_key": "objects/" + file_id, "sha256": "ab" * 32, "bytes": 12}

    def acquire(self, references: list[dict[str, str | int]], owner: str = "alice",
                session: str = "alice", response: str = "resp-alice") -> None:
        self.db.execute(ACQUIRE, (response, owner, session, json.dumps(references), "0"))

    def pins(self) -> list[tuple[str, str]]:
        return self.db.execute("SELECT response_id,file_id FROM file_attachment_pins ORDER BY response_id,file_id").fetchall()

    def test_pin_is_idempotent_and_does_not_advance_user_clock(self) -> None:
        self.acquire([self.reference("file-alice")])
        self.acquire([self.reference("file-alice"), self.reference("file-alice")])
        self.assertEqual(self.pins(), [("resp-alice", "file-alice")])
        self.assertEqual(self.db.execute("SELECT last_input_at FROM sessions WHERE id='alice'").fetchone(), (0,))
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM usage_reservations").fetchone(), (0,))

    def test_cross_owner_session_route_and_stale_metadata_are_rejected(self) -> None:
        for owner, session, response, file_id in (
            ("alice", "alice", "resp-alice", "file-bob"),
            ("alice", "bob", "resp-alice", "file-alice"),
            ("alice", "alice", "resp-bob", "file-alice"),
        ):
            with self.assertRaisesRegex(sqlite3.IntegrityError, "file_not_ready"):
                self.acquire([self.reference(file_id)], owner, session, response)
            self.assertEqual(self.pins(), [])
        stale = self.reference("file-alice")
        for field, changed in (("sha256", "cd" * 32), ("bytes", 13), ("object_key", "objects/other")):
            with self.assertRaisesRegex(sqlite3.IntegrityError, "file_not_ready"):
                self.acquire([{**stale, field: changed}])
            self.assertEqual(self.pins(), [])

    def test_one_invalid_reference_rolls_back_the_whole_statement(self) -> None:
        self.file("file-valid-two")
        for bad in ("file-missing", "file-bob"):
            with self.assertRaisesRegex(sqlite3.IntegrityError, "file_not_ready"):
                self.acquire([self.reference("file-alice"), self.reference("file-valid-two"), self.reference(bad)])
            self.assertEqual(self.pins(), [])

    def test_pin_wins_race_old_and_new_deletion_cannot_destroy_file(self) -> None:
        self.acquire([self.reference("file-alice")])
        self.assertIsNone(self.db.execute(CLAIM_DELETE, ("1", "file-alice", "alice")).fetchone())
        self.assertIsNone(self.db.execute(PERMIT_DELETE, ("file-alice", "alice")).fetchone())
        with self.assertRaisesRegex(sqlite3.IntegrityError, "file_in_use"):
            self.db.execute("UPDATE space_files SET status='deleting' WHERE id='file-alice'")
        with self.assertRaises(sqlite3.IntegrityError):
            self.db.execute("DELETE FROM space_files WHERE id='file-alice'")
        self.assertEqual(self.db.execute("SELECT status FROM space_files WHERE id='file-alice'").fetchone(), ("processed",))

    def test_delete_wins_race_no_later_pin_can_reference_candidate(self) -> None:
        self.assertEqual(self.db.execute(CLAIM_DELETE, ("1", "file-alice", "alice")).fetchone(), ("file-alice",))
        with self.assertRaisesRegex(sqlite3.IntegrityError, "file_not_ready"):
            self.acquire([self.reference("file-alice")])
        self.assertEqual(self.pins(), [])
        self.assertEqual(self.db.execute(PERMIT_DELETE, ("file-alice", "alice")).fetchone(), ("file-alice",))
        with self.assertRaisesRegex(sqlite3.IntegrityError,"invalid_file_lifecycle"):
            self.db.execute("UPDATE space_files SET status='processed' WHERE id='file-alice'")

    def test_release_is_owner_session_scoped_then_delete_can_finish(self) -> None:
        self.acquire([self.reference("file-alice")])
        self.db.execute(RELEASE, ("bob", "bob", "resp-alice"))
        self.db.execute(RELEASE, ("alice", "bob", "resp-alice"))
        self.assertEqual(len(self.pins()), 1)
        self.db.execute(RELEASE, ("alice", "alice", "resp-alice"))
        self.db.execute(RELEASE, ("alice", "alice", "resp-alice"))
        self.assertEqual(self.pins(), [])
        self.assertIsNotNone(self.db.execute(CLAIM_DELETE, ("1", "file-alice", "alice")).fetchone())
        self.db.execute(FINISH_DELETE, ("2", "file-alice", "alice"))
        self.assertEqual(self.db.execute("SELECT status FROM space_files WHERE id='file-alice'").fetchone(), ("deleted",))

    def test_pin_identity_is_immutable_and_never_expires_by_age(self) -> None:
        self.acquire([self.reference("file-alice")])
        for assignment in ("account_id='bob'", "session_id='bob'", "bytes=13", "created_at_ms=999999999"):
            with self.assertRaisesRegex(sqlite3.IntegrityError, "immutable_file_attachment_pin"):
                self.db.execute("UPDATE file_attachment_pins SET " + assignment)
        self.db.execute("UPDATE space_files SET updated_at_ms=999999999 WHERE id='file-alice'")
        self.assertEqual(self.pins(), [("resp-alice", "file-alice")])

    def test_stale_upload_abort_cannot_delete_a_published_or_pinned_file(self) -> None:
        self.acquire([self.reference("file-alice")])
        self.db.execute(CLAIM_ABORT, ("1", "file-alice", "alice"))
        self.assertEqual(self.db.execute("SELECT status,deletion_reason FROM space_files WHERE id='file-alice'").fetchone(), ("processed", "user"))
        self.assertIsNone(self.db.execute(PERMIT_DELETE, ("file-alice", "alice")).fetchone())

    def test_interrupted_abort_preserves_failed_diagnostics_then_user_can_delete(self) -> None:
        self.file("file-interrupted", status="uploading")
        self.db.execute(CLAIM_ABORT, ("1", "file-interrupted", "alice"))
        self.assertEqual(self.db.execute("SELECT status,deletion_reason FROM space_files WHERE id='file-interrupted'").fetchone(), ("deleting", "upload_abort"))
        self.assertIsNotNone(self.db.execute(PERMIT_DELETE, ("file-interrupted", "alice")).fetchone())
        self.db.execute(FINISH_DELETE, ("2", "file-interrupted", "alice"))
        self.assertEqual(self.db.execute("SELECT status FROM space_files WHERE id='file-interrupted'").fetchone(), ("failed",))
        self.db.execute(USER_FAILED_DELETE, ("file-interrupted", "alice"))
        self.db.execute(FINISH_DELETE, ("3", "file-interrupted", "alice"))
        self.assertEqual(self.db.execute("SELECT status FROM space_files WHERE id='file-interrupted'").fetchone(), ("deleted",))

    def test_pinned_response_listing_has_owner_bound_keyset_pages(self) -> None:
        for number in range(140):
            response = f"resp-page-{number:03}"
            self.route(response)
            self.acquire([self.reference("file-alice")], response=response)
        first = self.db.execute(LIST, ("alice", "alice", "", "128")).fetchall()
        second = self.db.execute(LIST, ("alice", "alice", first[-1][0], "128")).fetchall()
        self.assertEqual((len(first), len(second)), (128, 12))
        self.assertEqual(set(first) & set(second), set())
        self.assertEqual(self.db.execute(LIST, ("bob", "bob", "", "128")).fetchall(), [])


if __name__ == "__main__":
    unittest.main()
