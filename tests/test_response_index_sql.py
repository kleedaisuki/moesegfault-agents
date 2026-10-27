"""Exercise the production routing SQL, including owner and retry isolation."""

from pathlib import Path
import re
import sqlite3
import unittest

ROOT = Path(__file__).resolve().parents[1]
SOURCE = (ROOT / "crates/agents-worker/src/response_index.rs").read_text(encoding="utf-8")


def statement(prefix: str) -> str:
    matches: list[str] = re.findall(r'\.prepare\("([^"\n]+)"\)', SOURCE)
    return next(sql for sql in matches if sql.startswith(prefix))


INSERT = statement("INSERT")
BY_ID = statement("SELECT id,session_id,agent_id,request_hash FROM response_routes WHERE account_id=? AND id=")
BY_KEY = statement("SELECT id,session_id,agent_id,request_hash FROM response_routes WHERE account_id=? AND idempotency_hash=")


class ResponseIndexSqlTests(unittest.TestCase):
    def setUp(self) -> None:
        self.db: sqlite3.Connection = sqlite3.connect(":memory:")
        self.db.execute("PRAGMA foreign_keys=ON")
        self.db.executescript((ROOT / "migrations/0001_initial.sql").read_text(encoding="utf-8"))
        self.db.executescript((ROOT / "migrations/0013_response_index.sql").read_text(encoding="utf-8"))
        self.db.executemany(
            "INSERT INTO accounts VALUES(?, 'issuer', ?, 0, 0)",
            [("owner", "subject"), ("other", "other-subject")],
        )

    def tearDown(self) -> None:
        self.db.close()

    def reserve(self, response: str, owner: str = "owner", session: str = "session",
                request_hash: str = "body", key_hash: str | None = "key") -> None:
        self.db.execute(INSERT, (response, owner, session, "agent", request_hash, key_hash, 1234.0))

    def test_retry_race_selects_one_response_and_session(self) -> None:
        self.reserve("resp_first", session="first-session")
        self.reserve("resp_loser", session="loser-session")
        self.assertEqual(
            self.db.execute(BY_KEY, ("owner", "key")).fetchone(),
            ("resp_first", "first-session", "agent", "body"),
        )
        self.assertIsNone(self.db.execute(BY_ID, ("owner", "resp_loser")).fetchone())
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM response_routes").fetchone(), (1,))

    def test_mismatched_body_cannot_replace_retry_identity(self) -> None:
        self.reserve("resp_first")
        self.reserve("resp_second", request_hash="different")
        self.assertEqual(self.db.execute(BY_KEY, ("owner", "key")).fetchone()[3], "body")

    def test_same_retry_key_is_independent_across_owners(self) -> None:
        self.reserve("resp_owner")
        self.reserve("resp_other", owner="other")
        self.assertIsNone(self.db.execute(BY_ID, ("other", "resp_owner")).fetchone())
        self.assertEqual(self.db.execute(BY_KEY, ("other", "key")).fetchone()[0], "resp_other")

    def test_unkeyed_calls_reserve_distinct_identities(self) -> None:
        self.reserve("resp_one", key_hash=None)
        self.reserve("resp_two", key_hash=None)
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM response_routes").fetchone(), (2,))

    def test_identity_can_precede_session_but_cannot_precede_owner(self) -> None:
        self.reserve("resp_owned")
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM sessions").fetchone(), (0,))
        with self.assertRaises(sqlite3.IntegrityError):
            self.reserve("resp_unowned", owner="missing")


if __name__ == "__main__":
    unittest.main()
