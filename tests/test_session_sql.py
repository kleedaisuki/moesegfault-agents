"""User session discovery must filter the selected agent before applying its limit."""
import json
from pathlib import Path
import re
import sqlite3
import unittest

ROOT = Path(__file__).resolve().parents[1]
SOURCE = (ROOT / "crates/agents-worker/src/store.rs").read_text(encoding="utf-8")
QUERIES = re.findall(r'"(SELECT config_snapshot_json AS payload FROM sessions [^"\n]+)"', SOURCE)
AGENT_QUERY = next(sql for sql in QUERIES if "AND agent_id=?" in sql)
OWNER_QUERY = next(sql for sql in QUERIES if "WHERE account_id=? ORDER BY" in sql)


class SessionSqlTests(unittest.TestCase):
    def setUp(self):
        self.db = sqlite3.connect(":memory:")
        for migration in sorted((ROOT / "migrations").glob("*.sql")):
            self.db.executescript(migration.read_text(encoding="utf-8"))
        self.db.execute("INSERT INTO accounts VALUES('owner','issuer','subject',0,0)")
        for id in ["selected", "other"]:
            self.db.execute("INSERT INTO agents(id,account_id,name,config_json,created_at,updated_at) VALUES(?,'owner','test','{}',0,0)", (id,))

    def tearDown(self):
        self.db.close()

    def insert(self, id, agent, created):
        self.db.execute("INSERT INTO sessions(id,account_id,agent_id,do_id,status,config_snapshot_json,instructions_hash,created_at,last_input_at) VALUES(?,'owner',?,?,'closed',?,'prefix',?,?)",
                        (id, agent, id, json.dumps({"id": id, "agent_id": agent}), created, created))

    def test_unrelated_history_cannot_hide_selected_agent(self):
        self.insert("old-selected", "selected", 0)
        for i in range(1001):
            self.insert("other-" + str(i), "other", i + 1)
        old_page = [json.loads(row[0]) for row in self.db.execute(OWNER_QUERY, ("owner",))]
        self.assertEqual(len(old_page), 1000)
        self.assertEqual([row for row in old_page if row["agent_id"] == "selected"], [])
        actual = [json.loads(row[0]) for row in self.db.execute(AGENT_QUERY, ("owner", "selected"))]
        self.assertEqual(actual, [{"id": "old-selected", "agent_id": "selected"}])

    def test_owner_scope_and_stable_sort_are_preserved(self):
        for id, created in [("c", 2), ("b", 2), ("a", 1)]:
            self.insert(id, "selected", created)
        self.assertEqual([json.loads(row[0])["id"] for row in self.db.execute(AGENT_QUERY, ("owner", "selected"))], ["b", "c", "a"])
        self.assertEqual(list(self.db.execute(AGENT_QUERY, ("intruder", "selected"))), [])

    def test_selected_agent_query_uses_index_without_temp_sort(self):
        plan = " ".join(row[3] for row in self.db.execute("EXPLAIN QUERY PLAN " + AGENT_QUERY, ("owner", "selected")))
        self.assertIn("sessions_account_agent_order", plan)
        self.assertNotIn("TEMP B-TREE", plan)


if __name__ == "__main__":
    unittest.main()
