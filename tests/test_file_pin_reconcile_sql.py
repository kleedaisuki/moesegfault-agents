"""Production keyset scheduling never infers attachment liveness from age."""

from __future__ import annotations

import json
from pathlib import Path
import random
import re
import sqlite3
import unittest

ROOT = Path(__file__).resolve().parents[1]
SOURCE = (ROOT / "crates/agents-worker/src/space_pin_reconcile.rs").read_text(encoding="utf-8")
PINS = (ROOT / "crates/agents-worker/src/space_pins.rs").read_text(encoding="utf-8")


def query(source: str, prefix: str) -> str:
    matches = [sql for sql in re.findall(r'\.prepare\("([^"\n]+)"\)', source) if sql.startswith(prefix)]
    if len(matches) != 1:
        raise AssertionError((prefix, matches))
    return matches[0]


SCAN = query(SOURCE, "SELECT DISTINCT account_id,session_id FROM file_attachment_pins")
CURSOR = query(SOURCE, "INSERT INTO resource_system_state")
ACQUIRE = query(PINS, "INSERT INTO file_attachment_pins")
RELEASE = query(PINS, "DELETE FROM file_attachment_pins")


class FilePinReconcileSqlTests(unittest.TestCase):
    def setUp(self) -> None:
        self.db: sqlite3.Connection = sqlite3.connect(":memory:")
        for migration in sorted((ROOT / "migrations").glob("*.sql")):
            self.db.executescript(migration.read_text(encoding="utf-8"))
        for owner in ("alice", "bob"):
            self.db.execute("INSERT INTO accounts VALUES(?, 'issuer', ?, 0, 0)", (owner, owner))
            self.db.execute("INSERT INTO agents VALUES(?,?,?,'{}',0,0,NULL)", (owner, owner, owner))
            self.db.execute("INSERT INTO spaces VALUES(?,?,?,0,0)", ("space-" + owner, owner, owner))
            self.db.execute("""INSERT INTO space_files
                (id,account_id,space_id,filename,purpose,content_type,object_key,bytes,sha256,
                 status,reservation_id,created_at_ms,updated_at_ms,recovery_at_ms,upload_outcome)
                VALUES(?,?,?,'input.bin','user_data','application/octet-stream',?,12,?,
                       'processed',?,0,0,150000,'stored')""",
                            ("file-" + owner, owner, "space-" + owner, "objects/" + owner,
                             "ab" * 32, "upload-" + owner))

    def tearDown(self) -> None:
        self.db.close()

    def pin(self, owner: str, session: str, responses: int = 1) -> None:
        self.db.execute("INSERT INTO sessions VALUES(?,?,?,?,'idle','{}','pin',0,0,NULL,NULL,NULL)",
                        (session, owner, owner, "do-" + session))
        reference = [{"id": "file-" + owner, "object_key": "objects/" + owner,
                      "sha256": "ab" * 32, "bytes": 12}]
        for number in range(responses):
            response = f"resp-{session}-{number}"
            self.db.execute("INSERT INTO response_routes VALUES(?,?,?,?,?,NULL,0)",
                            (response, owner, session, owner, "request"))
            self.db.execute(ACQUIRE, (response, owner, session, json.dumps(reference), "0"))

    def page(self, after: tuple[str, str] = ("", "")) -> list[tuple[str, str]]:
        return self.db.execute(SCAN, (*after, "32")).fetchall()

    def test_many_references_dispatch_one_owner_session(self) -> None:
        self.pin("alice", "session-alice", responses=5)
        self.pin("bob", "session-bob", responses=3)
        self.assertEqual(self.page(), [("alice", "session-alice"), ("bob", "session-bob")])
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM file_attachment_pins").fetchone(), (8,))

    def test_keyset_progress_covers_shuffled_owners_despite_first_failure(self) -> None:
        sessions = [("alice" if index % 2 else "bob", f"session-{index:03}") for index in range(103)]
        random.Random(431).shuffle(sessions)
        for owner, session in sessions:
            self.pin(owner, session)
        failed = min(sessions)
        seen: list[tuple[str, str]] = []
        after = ("", "")
        for _ in range(5):
            page = self.page(after)
            seen.extend(page)
            for owner, session in page:
                if (owner, session) != failed:
                    self.db.execute(RELEASE, (owner, session, f"resp-{session}-0"))
            if len(page) < 32:
                break
            after = page[-1]
        self.assertEqual(seen, sorted(sessions))
        self.assertEqual(self.page(), [failed], "A failed observation must be retried after the pass wraps")

    def test_scheduling_is_read_only_even_for_old_pins(self) -> None:
        self.pin("alice", "session-old")
        for _ in range(3):
            self.assertEqual(self.page(), [("alice", "session-old")])
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM file_attachment_pins").fetchone(), (1,))
        self.assertEqual(self.db.execute("SELECT status FROM space_files WHERE id='file-alice'").fetchone(), ("processed",))
        self.assertEqual(self.db.execute("SELECT last_input_at FROM sessions WHERE id='session-old'").fetchone(), (0,))
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM usage_reservations").fetchone(), (0,))

    def test_cursor_upsert_preserves_opaque_owner_session_position(self) -> None:
        key = "file_attachment_pins_cursor_v1"
        for position in ({"account_id": "alice", "session_id": "s-32"},
                         {"account_id": "", "session_id": ""}):
            self.db.execute(CURSOR, (key, json.dumps(position)))
            row = self.db.execute("SELECT value FROM resource_system_state WHERE key=?", (key,)).fetchone()
            self.assertIsNotNone(row)
            assert row is not None
            self.assertEqual(json.loads(row[0]), position)


if __name__ == "__main__":
    unittest.main()
