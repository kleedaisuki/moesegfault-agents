"""Run production skill statements against migrated SQLite, not a parallel model."""
from pathlib import Path
import hashlib
import json
import random
import re
import sqlite3
import unittest

ROOT = Path(__file__).resolve().parents[1]
SOURCE = (ROOT / "crates/agents-worker/src/skills.rs").read_text(encoding="utf-8")
SQL = re.findall(r'"((?:SELECT|INSERT|UPDATE|DELETE) [^"\n]+)"', SOURCE)


def query(prefix, exact=False):
    matches = [sql for sql in SQL if sql == prefix or (not exact and sql.startswith(prefix))]
    assert len(matches) == 1, (prefix, matches)
    return matches[0]


LIST = query("SELECT s.name,s.version,v.instructions")
SELECTED = query("SELECT v.name,v.version,v.instructions")
FIND = query("SELECT name FROM skills")
VERSION = query("INSERT OR IGNORE INTO skill_versions")
CREATE = query("INSERT INTO skills")
UPDATE = query("UPDATE skills SET")
DELETE = query("DELETE FROM skills")
DELETE_VERSIONS = query("DELETE FROM skill_versions WHERE account_id=?1 AND name=?2", exact=True)
PRUNE = query("DELETE FROM skill_versions WHERE account_id=?1 AND name=?2 AND version")


def digest(name, instructions):
    return hashlib.sha256(b"magents.skill.v1\0" + name.encode() + b"\0" + instructions.encode()).hexdigest()


class SkillsSqlTests(unittest.TestCase):
    def setUp(self):
        self.db = sqlite3.connect(":memory:")
        for migration in sorted((ROOT / "migrations").glob("*.sql")):
            self.db.executescript(migration.read_text(encoding="utf-8"))
        for owner in ("alice", "bob"):
            self.db.execute("INSERT INTO accounts VALUES(?, 'issuer', ?, 0, 0)", (owner, owner))
        self.db.commit()

    def tearDown(self):
        self.db.close()

    def write(self, owner, name, instructions, create=True):
        version = digest(name, instructions)
        with self.db:
            self.db.execute(VERSION, (owner, name, version, instructions, 0, int(create)))
            result = self.db.execute(CREATE if create else UPDATE, (owner, name, version, 0)).fetchall()
            self.db.execute(PRUNE, (owner, name))
        return result

    def delete(self, owner, name):
        with self.db:
            result = self.db.execute(DELETE, (owner, name)).fetchall()
            self.db.execute(DELETE_VERSIONS, (owner, name))
        return result

    def test_real_journey_create_list_update_delete_and_account_isolation(self):
        self.write("alice", "review", "Alice private text")
        self.write("bob", "review", "Bob private text")
        self.assertEqual(self.db.execute(LIST, ("alice",)).fetchall(), [("review", digest("review", "Alice private text"), "Alice private text")])
        self.write("alice", "review", "Alice revised", False)
        self.assertEqual(self.db.execute(LIST, ("bob",)).fetchall()[0][2], "Bob private text")
        self.assertEqual(self.delete("bob", "not-alice"), [])
        self.assertEqual(self.delete("alice", "review"), [("review",)])
        self.assertEqual(self.db.execute(FIND, ("alice", "review")).fetchall(), [])
        self.assertEqual(self.db.execute(LIST, ("bob",)).fetchall()[0][2], "Bob private text")
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM skill_versions WHERE account_id='alice'").fetchone(), (0,))

    def test_immutable_version_and_cross_owner_head_reference(self):
        self.write("alice", "secret", "Private")
        version = digest("secret", "Private")
        with self.assertRaisesRegex(sqlite3.IntegrityError, "immutable_skill_version"):
            self.db.execute("UPDATE skill_versions SET instructions='replaced' WHERE account_id='alice'")
        self.db.rollback()
        with self.assertRaisesRegex(sqlite3.IntegrityError, "FOREIGN KEY"):
            self.db.execute(CREATE, ("bob", "secret", version, 0))

    def test_resolution_fetches_only_requested_account_owned_instructions(self):
        for owner in ("alice", "bob"):
            for name in ("a", "b", "c"):
                self.write(owner, name, f"{owner}-{name}")
        names = json.dumps(["a", "c", "a", "unknown"])
        resolved = self.db.execute(SELECTED, ("alice", names)).fetchall()
        self.assertEqual([(row[0], row[2]) for row in resolved], [("a", "alice-a"), ("c", "alice-c")])

    def test_quota_is_atomic_account_scoped_and_update_still_works(self):
        for i in range(50):
            self.write("alice", f"skill-{i}", "instructions")
        with self.assertRaisesRegex(sqlite3.IntegrityError, "skill_limit"):
            self.write("alice", "overflow", "must roll back")
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM skill_versions WHERE account_id='alice'").fetchone(), (50,))
        self.write("alice", "skill-0", "changed", False)
        self.write("bob", "overflow", "Bob has own quota")
        self.delete("alice", "skill-1")
        self.write("alice", "replacement", "replacement")

    def test_update_after_concurrent_delete_does_not_leave_orphan(self):
        self.write("alice", "review", "old")
        self.delete("alice", "review")
        self.assertEqual(self.write("alice", "review", "new", False), [])
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM skill_versions WHERE account_id='alice'").fetchone(), (0,))

    def test_existing_pins_survive_arbitrary_registry_changes(self):
        rng = random.Random(20261023)
        self.write("alice", "review", "original instructions")
        pin = tuple(self.db.execute(LIST, ("alice",)).fetchone())
        # Sessions own value snapshots, so registry revisions need not accumulate.
        for i in range(1000):
            text = f"revision-{rng.randrange(10000)}"
            self.write("alice", "review", text, False)
            revision = digest("review", text)
            self.assertEqual(self.db.execute("SELECT version,instructions FROM skill_versions WHERE account_id=? AND name=?", ("alice", "review")).fetchall(), [(revision, text)])
            self.assertEqual(self.db.execute("SELECT COUNT(*) FROM skills WHERE account_id='alice'").fetchone(), (1,))
            self.assertEqual(pin, ("review", digest("review", "original instructions"), "original instructions"))
        self.delete("alice", "review")
        self.assertEqual(pin[2], "original instructions")

    def test_account_erasure_removes_registry_heads_and_versions(self):
        self.write("alice", "review", "old")
        self.write("alice", "review", "new", False)
        self.write("bob", "review", "retain")
        self.db.execute("DELETE FROM accounts WHERE id='alice'")
        self.assertEqual(self.db.execute(LIST, ("alice",)).fetchall(), [])
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM skill_versions WHERE account_id='alice'").fetchone(), (0,))
        self.assertEqual(len(self.db.execute(LIST, ("bob",)).fetchall()), 1)

    def test_database_enforces_name_and_utf8_byte_bounds(self):
        for name, instructions in [("Bad", "text"), ("../x", "text"), ("x" * 65, "text"), ("x", "é" * 24577)]:
            with self.assertRaises(sqlite3.IntegrityError):
                self.write("alice", name, instructions)


if __name__ == "__main__":
    unittest.main()
