"""Resume through production SQL with D1's JavaScript-number REAL bindings."""
import json
import pathlib
import re
import sqlite3
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
SOURCE = (ROOT / "crates/agents-worker/src/store.rs").read_text(encoding="utf-8")
RESUME = re.search(r'self.changed\("(UPDATE sessions SET status=\'active\'.*?)"', SOURCE).group(1)


class SessionResumeSqlTests(unittest.TestCase):
    def setUp(self):
        self.db = sqlite3.connect(":memory:")
        for migration in sorted((ROOT / "migrations").glob("*.sql")):
            self.db.executescript(migration.read_text(encoding="utf-8"))
        self.db.execute("INSERT INTO accounts VALUES('owner','issuer','subject',0,0)")
        self.db.execute("INSERT INTO agents VALUES('a','owner','agent','{}',0,0,NULL)")
        self.snapshot = {"id": "s", "status": "closed", "input_revision": 1, "last_input_at_ms": 10,
                         "pinned": {"instructions": "fixed", "agent_revision": 1}}
        self.db.execute("INSERT INTO sessions VALUES('s','owner','a','do','closed',?,'pin',0,10,20,NULL,NULL)",
                        (json.dumps(self.snapshot),))

    def tearDown(self):
        self.db.close()

    def resume(self, owner="owner", limit=2):
        # worker::JsValue::from_f64 reaches SQLite as REAL, even for integral timestamps.
        now = float(1792776157970)
        return self.db.execute(RESUME, (now, now, owner, 's', owner, float(limit))).fetchone()

    def test_resume_preserves_json_integer_types_and_pinned_context(self):
        self.assertEqual(self.resume(), ('s',))
        text, timestamp, timestamp_type = self.db.execute(
            "SELECT config_snapshot_json,last_input_at,typeof(last_input_at) FROM sessions WHERE id='s'").fetchone()
        snapshot = json.loads(text)
        self.assertIs(type(snapshot['last_input_at_ms']), int)
        self.assertIs(type(snapshot['input_revision']), int)
        self.assertEqual(snapshot['last_input_at_ms'], 1792776157970)
        self.assertEqual(snapshot['last_input_at_ms'], timestamp)
        self.assertEqual(timestamp_type, 'integer')
        self.assertEqual(snapshot['input_revision'], 2)
        self.assertEqual(snapshot['status'], 'idle')
        self.assertEqual(snapshot['pinned'], self.snapshot['pinned'])
        types = self.db.execute("SELECT json_type(config_snapshot_json,'$.last_input_at_ms'),json_type(config_snapshot_json,'$.input_revision') FROM sessions WHERE id='s'").fetchone()
        self.assertEqual(types, ('integer', 'integer'))

    def test_resume_keeps_owner_and_atomic_quota_guards(self):
        self.assertIsNone(self.resume(owner='other'))
        self.assertIsNone(self.resume(limit=0))
        self.assertEqual(self.resume(), ('s',))
        self.assertIsNone(self.resume())


if __name__ == '__main__':
    unittest.main()
