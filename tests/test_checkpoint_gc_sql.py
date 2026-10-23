"""Checkpoint maintenance journeys using the actual production SQL and schema."""

import pathlib
import random
import re
import sqlite3
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
SOURCE = (ROOT / "crates/agents-worker/src/checkpoint_gc.rs").read_text(encoding="utf-8")


def query(prefix):
    return next(sql for sql in re.findall(r'\.prepare\("([^"\n]+)"\)', SOURCE) if sql.startswith(prefix))


INSERT = query("INSERT INTO workspace_checkpoint_intents")
SCAN = query("SELECT object_key,account_id,session_id,manifest_key,created_at_ms,next_check_ms FROM workspace_checkpoint_intents WHERE (")
LOOKUP = query("SELECT object_key,account_id,session_id,manifest_key,created_at_ms,next_check_ms FROM workspace_checkpoint_intents WHERE object_key=?")
DEFER = query("UPDATE workspace_checkpoint_intents")
DELETE = query("DELETE FROM workspace_checkpoint_intents")
REMOVE = next(sql for sql in re.findall(r'\.prepare\("([^"\n]+)"\)', (ROOT / "crates/agents-worker/src/reconcile.rs").read_text()) if sql.startswith("UPDATE resource_storage_objects SET removed_at_ms"))
DAY = 86_400_000


class CheckpointGcTests(unittest.TestCase):
    def setUp(self):
        self.db = sqlite3.connect(":memory:")
        for path in sorted((ROOT / "migrations").glob("*.sql")):
            self.db.executescript(path.read_text(encoding="utf-8"))
        self.db.execute("INSERT INTO accounts VALUES('a','issuer','a',0,0)")
        self.db.execute("INSERT INTO accounts VALUES('b','issuer','b',0,0)")
        self.db.execute("INSERT INTO agents VALUES('agent','a','test','{}',0,0,NULL)")
        self.db.execute("INSERT INTO sessions VALUES('s','a','agent','do','closed','{}','pin',0,0,NULL,NULL,NULL)")

    def tearDown(self):
        self.db.close()

    def intent(self, key, created=0):
        self.db.execute(INSERT, (key, "a", "s", "manifest", str(created), str(created + DAY)))

    def register(self, key):
        self.db.execute("INSERT INTO resource_storage_objects VALUES(?,'a','s','1024',0,0,NULL,'active')", (key,))

    def collect(self, key, current, now, crash=None):
        """Fake R2 fault boundaries, production SQL; user data pointer is external."""
        row = self.db.execute(LOOKUP, (key, "a", "s")).fetchone()
        if not row or row[5] > now or now - row[4] < DAY:
            return
        if current == key:
            self.db.execute(DEFER, (str(now + DAY), key))
            return
        if crash == "delete_failed":
            raise OSError("R2 unavailable")
        self.bucket.discard(key)
        if crash == "after_delete":
            return
        self.db.execute(REMOVE, (str(now), key, str(now), str(now)))
        if crash == "after_clock":
            return
        self.db.execute(DELETE, (key, "a", "s"))

    def test_current_and_conversations_never_age_expire(self):
        self.bucket = {"current", "old", "conversation"}
        self.intent("current")
        self.intent("old")
        self.register("current")
        self.register("old")
        for key in ("current", "old", "conversation"):
            self.collect(key, "current", DAY * 100)
        self.assertEqual(self.bucket, {"current", "conversation"})
        self.assertEqual(self.db.execute("SELECT status FROM resource_storage_objects WHERE object_key='current'").fetchone(), ("active",))
        self.assertEqual(self.db.execute("SELECT next_check_ms FROM workspace_checkpoint_intents").fetchone(), (DAY * 101,))

    def test_never_uploaded_and_each_delete_crash_boundary_retry_safely(self):
        for crash in (None, "delete_failed", "after_delete", "after_clock"):
            with self.subTest(crash=crash):
                key = str(crash)
                self.intent(key)
                self.register(key)
                self.bucket = {key}
                try:
                    self.collect(key, "new-current", DAY, crash)
                except OSError:
                    self.assertIn(key, self.bucket)
                    self.assertEqual(self.db.execute("SELECT status FROM resource_storage_objects WHERE object_key=?", (key,)).fetchone(), ("active",))
                self.collect(key, "new-current", DAY + 1)
                self.collect(key, "new-current", DAY + 2)
                self.assertNotIn(key, self.bucket)
                self.assertIsNone(self.db.execute(LOOKUP, (key, "a", "s")).fetchone())
                self.assertEqual(self.db.execute("SELECT status FROM resource_storage_objects WHERE object_key=?", (key,)).fetchone(), ("removed",))
        self.intent("never-uploaded")
        self.collect("never-uploaded", None, DAY)
        self.assertIsNone(self.db.execute(LOOKUP, ("never-uploaded", "a", "s")).fetchone())

    def test_grace_period_and_tenant_ownership(self):
        self.bucket = {"pending"}
        self.intent("pending")
        self.collect("pending", None, DAY - 1)
        self.assertIn("pending", self.bucket)
        self.assertIsNone(self.db.execute(LOOKUP, ("pending", "b", "s")).fetchone())
        with self.assertRaises(sqlite3.IntegrityError):
            self.db.execute(INSERT, ("foreign", "b", "s", "manifest", "0", str(DAY)))
        with self.assertRaises(sqlite3.IntegrityError):
            self.db.execute("UPDATE workspace_checkpoint_intents SET manifest_key='foreign' WHERE object_key='pending'")

    def test_indexed_bounded_cursor_covers_arbitrary_due_order(self):
        rng = random.Random(701)
        expected = set()
        for index in range(503):
            created = rng.randrange(DAY * 3)
            key = f"key-{index:04}"
            self.intent(key, created)
            if created + DAY <= DAY * 2:
                expected.add(key)
        seen = set()
        cursor = ("0", "")
        while True:
            rows = self.db.execute(SCAN, (*cursor, str(DAY * 2), "64")).fetchall()
            self.assertLessEqual(len(rows), 64)
            self.assertFalse(seen.intersection(row[0] for row in rows))
            seen.update(row[0] for row in rows)
            if len(rows) < 64:
                break
            cursor = (str(rows[-1][5]), rows[-1][0])
        self.assertEqual(seen, expected)
        plan = str(self.db.execute("EXPLAIN QUERY PLAN " + SCAN, ("0", "", str(DAY * 2), "64")).fetchall())
        self.assertIn("checkpoint_gc_due", plan)
        self.assertNotIn("TEMP B-TREE", plan)

    def test_source_orders_write_ahead_and_actual_delete_before_clock(self):
        workspace = (ROOT / "crates/agents-worker/src/workspace.rs").read_text(encoding="utf-8")
        self.assertLess(workspace.index("checkpoint_gc::record_upload_intent"), workspace.index(".put(&key, stream)"))
        collect = SOURCE[SOURCE.index("pub async fn collect("):]
        self.assertLess(collect.index("bucket.delete(key).await?"), collect.index("reconcile::remove_storage_object"))
        self.assertLess(collect.index("reconcile::remove_storage_object"), collect.index("DELETE FROM workspace_checkpoint_intents"))


if __name__ == "__main__":
    unittest.main()
