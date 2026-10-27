"""Runtime application identity is immutable and owner-scoped in shipped SQL."""

import json
from pathlib import Path
import re
import sqlite3
import unittest


ROOT: Path = Path(__file__).resolve().parents[1]


class RuntimeProfileTests(unittest.TestCase):
    db: sqlite3.Connection

    def setUp(self) -> None:
        self.db = sqlite3.connect(":memory:")
        for migration in sorted((ROOT / "migrations").glob("*.sql")):
            if migration.name < "0018":
                self.db.executescript(migration.read_text(encoding="utf-8"))
        self.db.execute("INSERT INTO accounts VALUES('owner','issuer','owner',0,0)")
        self.db.execute("INSERT INTO agents VALUES('agent','owner','Agent','{}',0,0,NULL)")
        self.db.execute("INSERT INTO sessions(id,account_id,agent_id,do_id,status,config_snapshot_json,instructions_hash,created_at,last_input_at) VALUES('old','owner','agent','old-do','active','{}','hash',0,0)")
        self.db.executescript((ROOT / "migrations/0018_runtime_profile.sql").read_text(encoding="utf-8"))

    def tearDown(self) -> None:
        self.db.close()

    def test_old_session_defaults_to_exact_legacy_application(self) -> None:
        self.assertEqual(self.db.execute("SELECT runtime_profile FROM sessions WHERE id='old'").fetchone(), ("basic1g",))
        self.db.execute("UPDATE sessions SET config_snapshot_json='{}' WHERE id='old'")
        with self.assertRaises(sqlite3.IntegrityError):
            self.db.execute("UPDATE sessions SET runtime_profile='standard4g' WHERE id='old'")

    def test_production_admission_pins_metadata_and_payload_together(self) -> None:
        source: str = (ROOT / "crates/agents-worker/src/store.rs").read_text(encoding="utf-8")
        statements: list[str] = re.findall(r'"(INSERT INTO sessions[^"\n]+)"', source)
        self.assertEqual(len(statements), 1)
        payload: str = json.dumps({"runtime_profile": "standard4g"})
        row: tuple[str] | None = self.db.execute(statements[0], ("new", "owner", "agent", "new-do", payload, "hash", 1, 1, "standard4g", "agent", "owner", "owner", 4)).fetchone()
        self.assertEqual(row, ("new",))
        self.assertEqual(self.db.execute("SELECT runtime_profile FROM sessions WHERE id='new'").fetchone(), ("standard4g",))
        with self.assertRaises(sqlite3.IntegrityError):
            self.db.execute("UPDATE sessions SET config_snapshot_json='{}' WHERE id='new'")

    def test_invalid_or_mislabeled_physical_profile_is_rejected(self) -> None:
        statement: str = "INSERT INTO sessions(id,account_id,agent_id,do_id,status,config_snapshot_json,instructions_hash,created_at,last_input_at,runtime_profile) VALUES(?,?,?,?,'active',?,'hash',0,0,?)"
        for profile, payload in [("16g", '{"runtime_profile":"16g"}'), ("standard4g", "{}"), ("basic1g", '{"runtime_profile":"standard4g"}')]:
            with self.subTest(profile=profile, payload=payload):
                with self.assertRaises(sqlite3.IntegrityError):
                    self.db.execute(statement, (profile, "owner", "agent", profile + "-do", payload, profile))

    def test_deployed_profiles_share_image_but_not_sku_or_namespace(self) -> None:
        config: dict[str, object] = json.loads((ROOT / "wrangler.jsonc").read_text(encoding="utf-8"))
        self.assertEqual(config["migrations"][-1], {"tag": "v3-runtime-profiles", "new_sqlite_classes": ["Session4G"]})
        for name in ("staging", "production"):
            env: dict[str, object] = config["env"][name]
            apps: dict[str, dict[str, object]] = {app["class_name"]: app for app in env["containers"]}
            self.assertEqual(apps["Session"]["instance_type"], "basic")
            self.assertEqual(apps["Session4G"]["instance_type"], "standard-1")
            self.assertEqual(apps["Session4G"]["image"], apps["Session"]["image"])
            bindings: dict[str, str] = {item["name"]: item["class_name"] for item in env["durable_objects"]["bindings"]}
            self.assertEqual(bindings["SESSIONS"], "Session")
            self.assertEqual(bindings["SESSION_4G"], "Session4G")


if __name__ == "__main__":
    unittest.main()
