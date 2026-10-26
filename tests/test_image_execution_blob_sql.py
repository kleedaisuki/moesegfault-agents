"""Execution image holders outlive observers and transfer only at terminal scope."""
from __future__ import annotations
import hashlib
import json
import pathlib
import re
import sqlite3
import unittest

ROOT: pathlib.Path = pathlib.Path(__file__).resolve().parents[1]
SOURCE: str = (ROOT / "crates/agents-worker/src/workspace_image_execution.rs").read_text(encoding="utf-8")
REGISTRY: str = (ROOT / "crates/agents-worker/src/workspace_blobs.rs").read_text(encoding="utf-8")


def query(source: str, prefix: str) -> str:
    return next(sql for sql in re.findall(r'\.prepare\(\s*"([^"\n]+)"', source) if sql.startswith(prefix))


def digest(value: object) -> str:
    return hashlib.sha256(json.dumps(value, separators=(",", ":")).encode()).hexdigest()


def source_id(artifact: str, execution: str = "operation") -> str:
    return "image-execution:" + digest(["session", "run", execution]) + ":" + digest(artifact)


BIND: str = query(SOURCE, "INSERT INTO workspace_blob_refs(account_id,kind,ref_id,object_key,session_id,response_id,execution_id,artifact_id,execution_generation)")
CLOSED: str = query(SOURCE, "SELECT COALESCE(execution_generation,'legacy') AS generation")
PREFETCH: str = query(SOURCE, "SELECT f.ref_id,f.artifact_id,f.execution_generation")
DEST: str = query(SOURCE, "INSERT INTO workspace_blob_refs(account_id,kind,ref_id,object_key,response_id,lease_until_ms,artifact_id)")
DRAIN: str = query(SOURCE, "DELETE FROM workspace_blob_refs WHERE account_id=?1 AND kind='image-execution'")
ACTOR: str = query(SOURCE, "INSERT INTO workspace_blob_refs(account_id,kind,ref_id,object_key,artifact_id)")
CHECK: str = query(SOURCE, "SELECT EXISTS(SELECT 1 FROM conversation_image_tombstones")
CLAIM: str = query(REGISTRY, "UPDATE workspace_blobs SET status='deleting'")
EXPIRE: str = query(REGISTRY, "DELETE FROM workspace_blob_refs WHERE rowid IN (SELECT rowid FROM workspace_blob_refs WHERE kind='image-delivery'")


class ImageExecutionBlobSqlTests(unittest.TestCase):
    def setUp(self) -> None:
        self.db: sqlite3.Connection = sqlite3.connect(":memory:")
        self.db.row_factory = sqlite3.Row
        for migration in sorted((ROOT / "migrations").glob("*.sql")):
            self.db.executescript(migration.read_text(encoding="utf-8"))
        for owner in ("a", "b"):
            self.db.execute("INSERT INTO accounts VALUES(?,?,?,0,0)", (owner, "issuer", owner))
            self.db.execute("INSERT INTO usage_accounts VALUES(?,10000,0,0,0)", (owner,))
        self.db.execute("INSERT INTO agents(id,account_id,name,config_json,created_at,updated_at) VALUES('agent','a','Agent','{}',0,0)")
        self.db.execute("INSERT INTO sessions(id,account_id,agent_id,do_id,status,config_snapshot_json,instructions_hash,created_at,last_input_at) VALUES('session','a','agent','do','active','{}','pin',0,0)")
        self.db.execute("INSERT INTO usage_reservations(id,account_id,session_id,idempotency_key,request_hash,reserved_microusd,state,created_at,updated_at) VALUES('run','a','session','run:run','hash',0,'reserved',0,0)")
        for key, sha in (("partial", "a"), ("final", "b"), ("unused", "c")):
            self.db.execute("INSERT INTO workspace_blobs(object_key,account_id,sha256,format_version,generation,archive_bytes,logical_bytes,entries,status,lease_until_ms,kind,content_type) VALUES(?,'a',?,0,0,100,100,1,'ready',0,'image','image/png')", (key, sha * 64))

    def tearDown(self) -> None:
        self.db.close()

    def bind(self, artifact: str, key: str, *, owner: str = "a", session: str = "session", run: str = "run") -> int:
        sha: str = self.db.execute("SELECT sha256 FROM workspace_blobs WHERE object_key=?", (key,)).fetchone()[0]
        return self.db.execute(BIND, (owner, source_id(artifact), key, session, run, "operation", artifact, sha, "100", "image/png")).rowcount

    def close(self) -> None:
        self.db.execute("UPDATE usage_reservations SET execution_finished_at=7200000 WHERE id='run'")

    def bind_conversation(self, conversation: str, item: str, key: str) -> None:
        sha: str = self.db.execute("SELECT sha256 FROM workspace_blobs WHERE object_key=?", (key,)).fetchone()[0]
        self.db.execute(ACTOR, ("a", conversation + ":image:" + digest(item), key, item, sha, "100", "image/png"))

    def transfer(self, assets: list[tuple[str, str, int | None, str]], *, store: bool = True,
                 conversation: str = "", expiry: int | None = None, now: int = 7200000) -> int:
        closed: sqlite3.Row | None = self.db.execute(CLOSED, ("run", "a", "session")).fetchone()
        if closed is None:
            raise ValueError("execution not finished")
        rows: list[sqlite3.Row] = self.db.execute(PREFETCH, ("a", "session", "run", "operation")).fetchall()
        if not rows:
            return 0
        sources: dict[str, sqlite3.Row] = {row["artifact_id"]: row for row in rows}
        for artifact, _item, _partial, key in assets:
            if artifact not in sources or sources[artifact]["object_key"] != key:
                raise ValueError("unknown or mismatched slot")
        if conversation:
            for _artifact, item, partial, key in assets:
                if partial is None:
                    fenced, held = self.db.execute(CHECK, ("a", conversation, item, conversation + ":image:" + digest(item), key)).fetchone()
                    if not fenced and not held:
                        raise ValueError("Conversation not ready")
        with self.db:
            for artifact, item, partial, key in assets:
                public: str = "image:" + digest(["response", artifact])
                targets: list[tuple[str, str, int | None]] = []
                if store:
                    targets.append(("response-artifact", public, None))
                if conversation and partial is None:
                    targets.append(("conversation-artifact", conversation + ":image:" + digest(item), None))
                if expiry is not None and now < expiry <= now + 3600000:
                    targets.append(("image-delivery", "delivery:" + digest(["delivery", artifact]), expiry))
                for kind, identity, deadline in targets:
                    self.db.execute(DEST, ("a", kind, identity, key, None if kind == "conversation-artifact" else "response", deadline,
                        sources[artifact]["ref_id"], "session", "run", "operation", artifact, "legacy", conversation,
                        item if kind == "conversation-artifact" else None))
            return self.db.execute(DRAIN, ("a", "session", "run", "operation", "legacy")).rowcount

    def test_active_long_task_has_no_observer_ttl(self) -> None:
        self.assertEqual(self.bind("item:result", "final"), 1)
        row: sqlite3.Row = self.db.execute("SELECT kind,lease_until_ms,execution_generation FROM workspace_blob_refs").fetchone()
        self.assertEqual(tuple(row), ("image-execution", None, "legacy"))
        self.assertEqual(self.db.execute(EXPIRE, ("36000000",)).rowcount, 0)
        self.assertEqual(self.db.execute(CLAIM, ("36060000", "final", "36000000")).rowcount, 0)
        with self.assertRaises(ValueError):
            self.transfer([("item:result", "item", None, "final")])

    def test_closed_run_blocks_late_bind_even_when_money_remains_reserved(self) -> None:
        self.bind("item:result", "final")
        self.close()
        self.assertEqual(self.bind("late", "partial"), 0)
        self.assertEqual(self.bind("item:result", "final"), 0)
        self.assertEqual(self.db.execute("SELECT state FROM usage_reservations").fetchone()[0], "reserved")
        with self.assertRaises(sqlite3.IntegrityError):
            self.db.execute("UPDATE workspace_blob_refs SET execution_generation='other' WHERE kind='image-execution'")

    def test_partial_and_final_transfer_is_atomic_and_retry_cannot_recreate(self) -> None:
        for slot, key in (("item:partial:0", "partial"), ("item:result", "final"), ("unused:result", "unused")):
            self.bind(slot, key)
        self.bind_conversation("conv_test", "item", "final")
        self.close()
        assets: list[tuple[str, str, int | None, str]] = [("item:partial:0", "item", 0, "partial"), ("item:result", "item", None, "final")]
        self.assertEqual(self.transfer(assets, conversation="conv_test", expiry=7201000), 3)
        counts: dict[str, int] = dict(self.db.execute("SELECT kind,COUNT(*) FROM workspace_blob_refs GROUP BY kind"))
        self.assertEqual(counts, {"conversation-artifact": 1, "image-delivery": 2, "response-artifact": 2})
        self.db.execute("DELETE FROM workspace_blob_refs WHERE kind IN ('response-artifact','image-delivery')")
        self.assertEqual(self.transfer(assets, conversation="conv_test", expiry=7202000), 0)
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM workspace_blob_refs").fetchone()[0], 1)

    def test_response_delete_suppresses_public_and_delivery_but_not_accepted_conversation(self) -> None:
        self.bind("item:result", "final")
        self.bind_conversation("conv_test", "item", "final")
        self.close()
        self.db.execute("INSERT INTO response_blob_tombstones VALUES('a','response')")
        self.assertEqual(self.transfer([("item:result", "item", None, "final")], conversation="conv_test", expiry=7201000), 1)
        self.assertEqual(self.db.execute("SELECT kind FROM workspace_blob_refs").fetchone()[0], "conversation-artifact")
        self.db.execute("INSERT INTO conversation_blob_tombstones VALUES('a','conv_test')")
        # Deleting Conversation metadata retains its items and accepted late
        # publication; only an item tombstone releases these image holders.
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM workspace_blob_refs WHERE kind='conversation-artifact'").fetchone()[0], 1)
        self.bind_conversation("conv_test", "late", "final")
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM workspace_blob_refs WHERE kind='conversation-artifact'").fetchone()[0], 2)

    def test_expired_delivery_drains_without_renewal_or_time_zero_ttl(self) -> None:
        self.bind("item:result", "final")
        self.close()
        self.assertEqual(self.transfer([("item:result", "item", None, "final")], store=False, expiry=7200000), 1)
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM workspace_blob_refs").fetchone()[0], 0)
        self.assertEqual(self.transfer([("item:result", "item", None, "final")], store=False, expiry=7201000), 0)

    def test_scope_unknown_slot_and_mapped_key_failure_preserve_execution_hold(self) -> None:
        self.assertEqual(self.bind("foreign", "final", owner="b"), 0)
        self.assertEqual(self.bind("wrong-session", "final", session="unknown"), 0)
        self.bind("item:result", "final")
        self.close()
        for assets in [[("unknown", "item", None, "final")], [("item:result", "item", None, "partial")]]:
            with self.assertRaises(ValueError):
                self.transfer(assets)
            self.assertEqual(self.db.execute("SELECT COUNT(*) FROM workspace_blob_refs WHERE kind='image-execution'").fetchone()[0], 1)
        body: str = SOURCE[SOURCE.index("pub(crate) async fn transfer_image_execution("):]
        self.assertLess(body.index("validate_assets("), body.index("db.batch("))
        self.assertIn("execution_generation", BIND)
        self.assertIn("execution_leases", BIND)


if __name__ == "__main__":
    unittest.main()
