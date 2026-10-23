"""Usage dashboard contract against the actual migrations and production SQL."""

import json
import pathlib
import re
import sqlite3
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
SOURCE = ROOT / "crates/agents-worker/src/store.rs"
QUERY = re.search(
    r'const USAGE_SUMMARY_SQL: &str = r#"(.*?)"#;',
    SOURCE.read_text(encoding="utf-8"),
    re.DOTALL,
).group(1)


class UsageSqlTests(unittest.TestCase):
    def setUp(self):
        self.db = sqlite3.connect(":memory:")
        for migration in sorted((ROOT / "migrations").glob("*.sql")):
            self.db.executescript(migration.read_text(encoding="utf-8"))
        for owner in ["alice", "bob"]:
            self.db.execute("INSERT INTO accounts VALUES (?,?,?,?,?)", (owner, "issuer", owner, 1, 1))
            self.db.execute("INSERT INTO usage_accounts VALUES (?,100,0,0,1)", (owner,))

    def tearDown(self):
        self.db.close()

    def summary(self, owner):
        return json.loads(self.db.execute(QUERY, (owner,)).fetchone()[0])

    def charge(self, key, owner, reservation, amount, resource, settle=False):
        self.db.execute("""INSERT INTO usage_reservations
            (id,account_id,idempotency_key,request_hash,reserved_microusd,state,created_at,updated_at)
            VALUES (?,?,?,?,?,'reserved',1,1)""", (key, owner, key, key, reservation))
        self.db.execute("""INSERT INTO usage_ledger
            (id,reservation_id,event_key,resource,quantity,unit,amount_microusd,price_version,created_at)
            VALUES (?,?,?,?,1,'request',?,'test',1)""", (key, key, key, resource, amount))
        if settle:
            self.db.execute("UPDATE usage_reservations SET state='settled',settled_microusd=? WHERE id=?", (amount, key))

    def test_wallet_and_resource_cost_remain_exact_above_javascript_safe_integer(self):
        amount = 9007199254740993
        self.db.execute("UPDATE usage_accounts SET budget_microusd=? WHERE account_id='alice'", (amount + 6,))
        self.charge("model", "alice", amount, amount, "model", settle=True)
        self.charge("workspace", "alice", 5, 2, "container")
        self.assertEqual(self.summary("alice"), {
            "wallet": {"budget_microusd": str(amount + 6), "spent_microusd": str(amount),
                       "reserved_microusd": "5", "available_microusd": "1"},
            "by_resource": [{"resource": "container", "cost_microusd": "2"},
                            {"resource": "model", "cost_microusd": str(amount)}],
        })

    def test_resource_summary_never_crosses_tenant_boundary(self):
        self.charge("alice", "alice", 60, 40, "model", settle=True)
        self.charge("bob", "bob", 20, 20, "model", settle=True)
        self.assertEqual(self.summary("alice")["by_resource"], [{"resource": "model", "cost_microusd": "40"}])
        self.assertEqual(self.summary("bob")["by_resource"], [{"resource": "model", "cost_microusd": "20"}])

    def test_unprovisioned_account_has_explicit_empty_wallet(self):
        summary = self.summary("unprovisioned")
        self.assertEqual(summary["by_resource"], [])
        self.assertEqual(set(summary["wallet"].values()), {"0"})


if __name__ == "__main__":
    unittest.main()
