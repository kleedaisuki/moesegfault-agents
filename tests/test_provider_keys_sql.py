"""Exercise the shipped credential migration and production SQL on real SQLite."""

from pathlib import Path
import re
import sqlite3
import unittest


ROOT = Path(__file__).resolve().parents[1]
SOURCE = (ROOT / "crates/agents-worker/src/provider_keys.rs").read_text(encoding="utf-8")
SQL = re.findall(r'"((?:SELECT|INSERT|UPDATE|DELETE) [^"\n]+)"', SOURCE)


def query(prefix: str, contains: str | None = None) -> str:
    matches = [statement for statement in SQL if statement.startswith(prefix)
               and (contains is None or contains in statement)]
    assert len(matches) == 1, (prefix, contains, matches)
    return matches[0]


CREATE = query("INSERT INTO provider_keys", "SELECT ?1,?2,?1,?3,")
LEGACY = query("INSERT INTO provider_keys", "SELECT ?1,?2,?3,'openai',")
UPDATE = query("UPDATE provider_keys SET")
LIST = query("SELECT id,name,CASE WHEN provider")
METADATA = query("SELECT provider,protocol,base_url")
DELETE_ID = query("DELETE FROM provider_keys WHERE account_id=?1 AND id=?2")
DELETE_LEGACY = query("DELETE FROM provider_keys WHERE account_id=?1 AND provider=?2")
# Exact pre-0010 production statement, deliberately independent of new SQL.
OLD_LEGACY_UPSERT = "INSERT INTO provider_keys(id,account_id,provider,key_encrypted,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?5) ON CONFLICT(account_id,provider) DO UPDATE SET key_encrypted=excluded.key_encrypted,key_version=provider_keys.key_version+1,updated_at=excluded.updated_at RETURNING id,provider,updated_at"


class ProviderCredentialSqlTests(unittest.TestCase):
    db: sqlite3.Connection

    def setUp(self) -> None:
        self.db = sqlite3.connect(":memory:")
        for migration in sorted((ROOT / "migrations").glob("*.sql")):
            if migration.name < "0010":
                self.db.executescript(migration.read_text(encoding="utf-8"))
        self.db.execute("INSERT INTO accounts VALUES('alice','issuer','alice',0,0)")
        self.db.execute("INSERT INTO accounts VALUES('bob','issuer','bob',0,0)")
        self.db.execute("INSERT INTO provider_keys VALUES('legacy-router','alice','openrouter','opaque-old-ciphertext',4,10,20)")
        self.db.executescript((ROOT / "migrations/0010_generic_provider_credentials.sql").read_text(encoding="utf-8"))

    def tearDown(self) -> None:
        self.db.close()

    def create(self, owner: str, credential: str, protocol: str = "openai") -> list[tuple[object, ...]]:
        return self.db.execute(CREATE, (credential, owner, protocol,
            "https://api.example.com/v1", f"{owner}-{credential}", "ciphertext", 100)).fetchall()

    def test_migration_preserves_ids_ciphertext_version_and_time(self) -> None:
        row = self.db.execute("SELECT id,provider,protocol,base_url,name,key_encrypted,key_version,created_at,updated_at FROM provider_keys").fetchone()
        self.assertEqual(row, ("legacy-router", "openrouter", "openai",
            "https://openrouter.ai/api/v1", "OpenRouter", "opaque-old-ciphertext", 4, 10, 20))

    def test_two_protocols_allow_multiple_named_credentials_per_account(self) -> None:
        for protocol in ("openai", "anthropic"):
            for number in range(3):
                self.assertEqual(len(self.create("alice", f"{protocol}-{number}", protocol)), 1)
        self.assertEqual(len(self.db.execute(LIST, ("alice",)).fetchall()), 7)
        self.assertEqual(self.db.execute(LIST, ("bob",)).fetchall(), [])
        self.assertTrue(all("ciphertext" not in row for row in self.db.execute(LIST, ("alice",))))

    def test_rotation_keeps_id_namespace_and_creation_time(self) -> None:
        self.create("alice", "cred-a")
        rows = self.db.execute(UPDATE, ("alice", "cred-a", "openai",
            "https://api.example.com/v1", "Renamed", "replacement", 200)).fetchall()
        self.assertEqual(len(rows), 1)
        self.assertEqual(self.db.execute("SELECT id,provider,key_version,created_at,updated_at FROM provider_keys WHERE id='cred-a'").fetchone(),
            ("cred-a", "cred-a", 2, 100, 200))

    def test_cross_owner_reads_rotation_and_deletion_are_isolated(self) -> None:
        self.create("alice", "cred-a")
        self.assertIsNone(self.db.execute(METADATA, ("bob", "cred-a")).fetchone())
        self.assertEqual(self.db.execute(UPDATE, ("bob", "cred-a", "anthropic",
            "https://attacker.example.com/v1", "Overwrite", "stolen", 200)).fetchall(), [])
        self.db.execute(DELETE_ID, ("bob", "cred-a"))
        self.assertEqual(self.db.execute("SELECT key_encrypted FROM provider_keys WHERE id='cred-a'").fetchone(), ("ciphertext",))
        self.db.execute(DELETE_LEGACY, ("bob", "openrouter"))
        self.assertIsNotNone(self.db.execute(METADATA, ("alice", "legacy-router")).fetchone())

    def test_legacy_upsert_retains_pin_id_and_updates_ciphertext(self) -> None:
        for number in range(2):
            rows = self.db.execute(LEGACY, (f"discarded-id-{number}", "alice", "openrouter",
                "https://openrouter.ai/api/v1", "OpenRouter", f"rotated-{number}", 100 + number)).fetchall()
            self.assertEqual(rows[0][0], "legacy-router")
        self.assertEqual(self.db.execute("SELECT id,key_version,key_encrypted,created_at FROM provider_keys WHERE provider='openrouter'").fetchone(),
            ("legacy-router", 6, "rotated-1", 10))

    def test_atomic_cap_blocks_creation_but_not_existing_legacy_rotation(self) -> None:
        for number in range(99):
            self.assertEqual(len(self.create("alice", f"cred-{number}")), 1)
        self.assertEqual(self.create("alice", "over-limit"), [])
        rotated = self.db.execute(LEGACY, ("discarded", "alice", "openrouter",
            "https://openrouter.ai/api/v1", "OpenRouter", "rotation-at-cap", 300)).fetchall()
        self.assertEqual(rotated[0][0], "legacy-router")
        self.assertEqual(len(self.create("bob", "bob-credential")), 1)

    def test_exact_old_binary_upsert_rotates_without_changing_existing_pin(self) -> None:
        rotated = self.db.execute(OLD_LEGACY_UPSERT,
            ("ignored-old-generated-id", "alice", "openrouter", "old-runtime-rotation", 300)).fetchall()
        self.assertEqual(rotated, [("legacy-router", "openrouter", 300)])
        self.assertEqual(self.db.execute("SELECT id,protocol,base_url,name,key_encrypted,key_version,created_at FROM provider_keys WHERE id='legacy-router'").fetchone(),
            ("legacy-router", "openai", "https://openrouter.ai/api/v1", "OpenRouter", "old-runtime-rotation", 5, 10))

    def test_exact_old_binary_insert_gets_readable_protocol_metadata(self) -> None:
        created = self.db.execute(OLD_LEGACY_UPSERT,
            ("old-runtime-deepseek", "bob", "deepseek", "legacy-aad-ciphertext", 400)).fetchall()
        self.assertEqual(created, [("old-runtime-deepseek", "deepseek", 400)])
        self.assertEqual(self.db.execute("SELECT protocol,base_url,name,key_encrypted FROM provider_keys WHERE id='old-runtime-deepseek'").fetchone(),
            ("openai", "https://api.deepseek.com", "DeepSeek", "legacy-aad-ciphertext"))
        self.assertEqual(self.db.execute(METADATA, ("bob", "old-runtime-deepseek")).fetchone(),
            ("deepseek", "openai", "https://api.deepseek.com"))

    def test_public_metadata_hides_generic_internal_discriminator(self) -> None:
        self.create("alice", "cred-openai", "openai")
        self.create("alice", "cred-anthropic", "anthropic")
        listed = {row[0]: row for row in self.db.execute(LIST, ("alice",))}
        self.assertEqual(listed["cred-openai"][2:4], ("openai", "openai"))
        self.assertEqual(listed["cred-anthropic"][2:4], ("anthropic", "anthropic"))
        self.assertEqual(listed["legacy-router"][2:4], ("openrouter", "openai"))


if __name__ == "__main__":
    unittest.main()
