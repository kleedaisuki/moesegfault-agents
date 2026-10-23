"""API-key-only billing journeys using the actual shared Worker subscription query."""
import json
from pathlib import Path
import re
import sqlite3
import unittest

ROOT = Path(__file__).resolve().parents[1]
STORE = (ROOT / "crates/agents-worker/src/store.rs").read_text(encoding="utf-8")
AUTH = (ROOT / "crates/agents-worker/src/auth.rs").read_text(encoding="utf-8")
ALLOWANCES = (ROOT / "crates/agents-worker/src/allowances.rs").read_text(encoding="utf-8")
SNAPSHOTS = re.search(r'const VERIFIED_SUBSCRIPTIONS_SQL: &str = "([^"]+)";', STORE).group(1)
KEY_LOOKUP = next(sql for sql in re.findall(r'\.prepare\("([^"\n]+)"\)', AUTH)
                  if sql.startswith("SELECT account_id,last_used_at FROM api_keys"))
GRANT = re.search(r'const GRANT: &str = "(.*?)";', ALLOWANCES, re.S).group(1)


class SubscriptionSnapshotTests(unittest.TestCase):
    def setUp(self):
        self.db = sqlite3.connect(":memory:")
        for path in sorted((ROOT / "migrations").glob("*.sql")):
            self.db.executescript(path.read_text(encoding="utf-8"))
        self.start = 1_000
        self.refresh_after = self.start + 300
        self.end = self.start + 30*86_400
        self.grant = {
            "id": "paid-grant", "product_id": "agents", "plan_id": "agents-pro",
            "status": "active", "current_period_start": self.start,
            "current_period_end": self.end,
            "entitlements": ["agents.access", "agents.tier.pro"],
        }
        self.db.execute("INSERT INTO accounts VALUES('a','issuer','subject',0,0)")
        self.db.execute("INSERT INTO usage_accounts VALUES('a',0,0,0,0)")
        self.db.execute("INSERT INTO api_keys(id,account_id,name,key_hash,key_prefix,created_at) VALUES('key','a','SDK','hash','mag_test',0)")
        self.db.execute("INSERT INTO browser_sessions(token_hash,account_id,tokens_encrypted,csrf_hash,expires_at,created_at) VALUES('cookie','a','opaque-ciphertext','csrf',1300,1000)")
        self.db.execute("INSERT INTO subscription_snapshots VALUES('a','agents',?,?,?)",
                        (json.dumps([self.grant]),self.start,self.refresh_after))

    def tearDown(self):
        self.db.close()

    def test_sdk_key_retains_verified_grant_after_refresh_deadline_and_browser_logout(self):
        self.db.execute("DELETE FROM browser_sessions WHERE token_hash='cookie'")
        now = self.refresh_after+1
        account, _ = self.db.execute(KEY_LOOKUP,("hash",)).fetchone()
        rows = self.db.execute(SNAPSHOTS,(account,)).fetchall()
        grants = [grant for row in rows for grant in json.loads(row[0])]
        self.assertEqual(grants,[self.grant])
        self.assertGreater(now,self.refresh_after)
        self.assertLess(now,grants[0]["current_period_end"])
        self.assertIn("agents.tier.pro",grants[0]["entitlements"])
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM browser_sessions").fetchone(),(0,))

    def test_stale_refresh_metadata_does_not_remove_paid_funding_or_duplicate_it(self):
        for now in [self.refresh_after+1,self.start+86_400,self.end-1]:
            grant = json.loads(self.db.execute(SNAPSHOTS,("a",)).fetchone()[0])[0]
            expected = self.db.execute("SELECT COALESCE(MAX(period_start),-1) FROM allowance_windows WHERE account_id='a'").fetchone()[0]
            self.db.execute(GRANT,("a",str(grant["current_period_start"]),str(grant["current_period_end"]),"billing:paid-grant","180000000",str(now),str(expected),None))
        self.assertEqual(self.db.execute("SELECT budget_microusd FROM usage_accounts WHERE account_id='a'").fetchone(),(180_000_000,))
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM allowance_grants").fetchone(),(1,))

    def test_snapshot_reads_are_tenant_and_product_scoped(self):
        self.assertEqual(self.db.execute(SNAPSHOTS,("other-account",)).fetchall(),[])
        self.db.execute("UPDATE subscription_snapshots SET product_id='unrelated'")
        self.assertEqual(self.db.execute(SNAPSHOTS,("a",)).fetchall(),[])

    def test_all_authorization_consumers_share_the_same_grant_reader(self):
        # Freshness may schedule a human-token refresh, never independently expire grants.
        self.assertIn(".subscriptions(owner).await?",STORE)
        self.assertIn(".subscriptions(&context.account_id)",AUTH)
        self.assertIn(".subscriptions(account)",ALLOWANCES)
        self.assertNotIn("valid_until",SNAPSHOTS)


if __name__ == "__main__":
    unittest.main()
