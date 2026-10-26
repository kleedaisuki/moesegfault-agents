"""Only the Conversation actor may publish its permanent final-image holder."""
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


def ref_id(conversation: str, item: str) -> str:
    return conversation + ":image:" + digest(item)


ACTOR: str = query(SOURCE, "INSERT INTO workspace_blob_refs(account_id,kind,ref_id,object_key,artifact_id)")
DISCARD: str = query(SOURCE, "DELETE FROM workspace_blob_refs WHERE account_id=? AND kind='conversation-artifact' AND ref_id=? AND artifact_id=?")
ITEM_MARK: str = query(SOURCE, "INSERT INTO conversation_image_tombstones(")
CHECK: str = query(SOURCE, "SELECT EXISTS(SELECT 1 FROM conversation_image_tombstones")
EXEC: str = query(SOURCE, "INSERT INTO workspace_blob_refs(account_id,kind,ref_id,object_key,session_id,response_id,execution_id,artifact_id,execution_generation)")
DEST: str = query(SOURCE, "INSERT INTO workspace_blob_refs(account_id,kind,ref_id,object_key,response_id,lease_until_ms,artifact_id)")
DRAIN: str = query(SOURCE, "DELETE FROM workspace_blob_refs WHERE account_id=?1 AND kind='image-execution'")
PUBLIC_MARK: str = query(REGISTRY, "INSERT INTO response_blob_tombstones(")


class ConversationImageBlobSqlTests(unittest.TestCase):
    def setUp(self) -> None:
        self.db: sqlite3.Connection = sqlite3.connect(":memory:")
        for migration in sorted((ROOT / "migrations").glob("*.sql")):
            self.db.executescript(migration.read_text(encoding="utf-8"))
        for owner in ("a", "b"):
            self.db.execute("INSERT INTO accounts VALUES(?,?,?,0,0)", (owner, "issuer", owner))
            self.db.execute("INSERT INTO usage_accounts VALUES(?,10000,0,0,0)", (owner,))
            self.db.execute("INSERT INTO workspace_blobs(object_key,account_id,sha256,format_version,generation,archive_bytes,logical_bytes,entries,status,lease_until_ms,kind,content_type) VALUES(?,?,?,0,0,100,100,1,'ready',0,'image','image/png')", ("image-" + owner, owner, "ab" * 32))
        self.db.execute("INSERT INTO agents(id,account_id,name,config_json,created_at,updated_at) VALUES('agent','a','Agent','{}',0,0)")
        self.db.execute("INSERT INTO sessions(id,account_id,agent_id,do_id,status,config_snapshot_json,instructions_hash,created_at,last_input_at) VALUES('session','a','agent','do','active','{}','pin',0,0)")
        self.db.execute("INSERT INTO usage_reservations(id,account_id,session_id,idempotency_key,request_hash,reserved_microusd,state,created_at,updated_at) VALUES('run','a','session','run:run','hash',0,'reserved',0,0)")
        self.source: str = "image-execution:" + digest(["session", "run", "operation"]) + ":" + digest("item:result")
        self.db.execute(EXEC, ("a", self.source, "image-a", "session", "run", "operation", "item:result", "ab" * 32, "100", "image/png"))

    def tearDown(self) -> None:
        self.db.close()

    def bind(self, conversation: str = "conv_a", item: str = "item", owner: str = "a") -> int:
        return self.db.execute(ACTOR, (owner, ref_id(conversation, item), "image-" + owner, item, "ab" * 32, "100", "image/png")).rowcount

    def check(self, conversation: str = "conv_a", item: str = "item") -> tuple[int, int]:
        return self.db.execute(CHECK, ("a", conversation, item, ref_id(conversation, item), "image-a")).fetchone()

    def close(self) -> None:
        self.db.execute("UPDATE usage_reservations SET execution_finished_at=1000 WHERE id='run'")

    def target(self, kind: str, *, conversation: str = "conv_a", item: str = "item") -> int:
        identity: str = ref_id(conversation, item) if kind == "conversation-artifact" else "destination-" + kind
        return self.db.execute(DEST, ("a", kind, identity, "image-a", None if kind == "conversation-artifact" else "response", 2000 if kind == "image-delivery" else None,
            self.source, "session", "run", "operation", "item:result", "legacy", conversation, item if kind == "conversation-artifact" else None)).rowcount

    def release(self, conversation: str = "conv_a", item: str = "item", owner: str = "a") -> None:
        with self.db:
            self.db.execute(ITEM_MARK, (owner, conversation, item))
            self.db.execute(DISCARD, (owner, ref_id(conversation, item), item))

    def drain(self) -> int:
        return self.db.execute(DRAIN, ("a", "session", "run", "operation", "legacy")).rowcount

    def test_actor_holds_final_before_publication_and_source_cleanup_only_reuses(self) -> None:
        self.assertEqual(self.bind(), 1)
        self.assertEqual(self.check(), (0, 1))
        row: tuple[object, ...] = self.db.execute("SELECT artifact_id,response_id,execution_id,execution_generation,lease_until_ms FROM workspace_blob_refs WHERE kind='conversation-artifact'").fetchone()
        self.assertEqual(row, ("item", None, None, None, None))
        self.close()
        self.assertEqual(self.target("conversation-artifact"), 1)
        self.assertEqual(self.drain(), 1)
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM workspace_blob_refs").fetchone()[0], 1)

    def test_unpublished_rollback_has_no_fence_and_cleanup_cannot_recreate(self) -> None:
        self.bind()
        self.assertEqual(self.check(), (0, 1))
        self.db.execute(DISCARD, ("a", ref_id("conv_a", "item"), "item"))
        self.close()
        self.assertEqual(self.target("conversation-artifact"), 0)
        self.assertEqual(self.check(), (0, 0))
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM conversation_image_tombstones").fetchone()[0], 0)
        self.assertEqual(self.bind(), 1)
        self.assertEqual(self.check(), (0, 1))

    def test_canonical_item_delete_fences_late_actor_and_terminal_handoff(self) -> None:
        self.bind()
        self.release()
        with self.assertRaises(sqlite3.IntegrityError):
            self.bind()
        self.assertEqual(self.check(), (1, 0))
        self.close()
        self.assertEqual(self.target("conversation-artifact"), 0)
        self.assertEqual(self.drain(), 1)
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM workspace_blob_refs").fetchone()[0], 0)
        with self.assertRaises(sqlite3.IntegrityError):
            self.db.execute("DELETE FROM conversation_image_tombstones")

    def test_public_response_delete_does_not_delete_existing_conversation_final(self) -> None:
        self.bind()
        self.db.execute(PUBLIC_MARK, ("a", "response"))
        self.close()
        self.assertEqual(self.target("response-artifact"), 0)
        self.assertEqual(self.target("image-delivery"), 0)
        self.assertEqual(self.target("conversation-artifact"), 1)
        self.drain()
        self.assertEqual(self.db.execute("SELECT kind FROM workspace_blob_refs").fetchone()[0], "conversation-artifact")

    def test_missing_promised_holder_blocks_release_before_batch(self) -> None:
        self.close()
        self.assertEqual(self.check(), (0, 0))
        # This is the actual scalar guard read by Rust before constructing drain.
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM workspace_blob_refs WHERE kind='image-execution'").fetchone()[0], 1)
        body: str = SOURCE[SOURCE.index("pub(crate) async fn transfer_image_execution("):]
        self.assertLess(body.index("complete: false"), body.index("DELETE FROM workspace_blob_refs"))
        pending: str = body[body.index("holder.fenced == 0 && holder.held == 0"):body.index("let mut statements")]
        self.assertIn("results: receipts", pending)
        self.assertIn("holder.fenced == 0 && holder.held == 0", body)

    def test_conversation_item_and_owner_isolation(self) -> None:
        self.bind("conv_a", "item")
        self.bind("conv_a", "other")
        self.bind("conv_b", "item")
        self.bind("conv_a", "item", "b")
        self.assertEqual(self.db.execute(ACTOR, ("b", ref_id("conv_a", "foreign"), "image-a", "foreign", "ab" * 32, "100", "image/png")).rowcount, 0)
        self.release("conv_a", "item")
        remaining: set[tuple[str, str]] = set(self.db.execute("SELECT account_id,ref_id FROM workspace_blob_refs WHERE kind='conversation-artifact'"))
        self.assertEqual(remaining, {("a", ref_id("conv_a", "other")), ("a", ref_id("conv_b", "item")), ("b", ref_id("conv_a", "item"))})


if __name__ == "__main__":
    unittest.main()
