# Persisted workspace versions

Every account's Space contains immutable workspace versions from all its
sessions. A version is user data, not a temporary checkpoint or telemetry. Its
name, identity and archive never change, and user APIs cannot overwrite, rename
or delete it. Downloads do not start a workspace or advance the input clock.

## User and agent operations

| Operation | Contract |
| --- | --- |
| Persist workspace | Built-in `workspace_commit` with optional `name` |
| Find versions | Built-in `workspace_versions` with optional `after`, `limit` |
| Restore workspace | Built-in `workspace_restore` with `version_id` |
| List in Space | `GET /v1/space/workspace-versions?after=<id>&limit=50` |
| Inspect version | `GET /v1/space/workspace-versions/{id}` |
| Download | `GET /v1/space/workspace-versions/{id}/content` |

The list is newest first with a stable `(created_at_ms, id)` cursor, at most
100 records per page. It returns `object: list`, `data`, `has_more`, `first_id`,
`last_id` and `next_cursor`. A cursor must belong to the authenticated account.
Each record contains `id`, `object: workspace.version`, `name`, `session_id`,
`created_at_ms`, `generation`, `sha256`, `format_version`, `archive_bytes`,
`logical_bytes` and `entries`; private storage keys are never returned.

Downloads stream the compressed workspace archive, with `Content-Length` and
`X-Snapshot-Sha256` for integrity, plus format, generation, logical-byte and entry
headers matching existing workspace downloads. The native CLI can validate and
select individual files without loading the archive into Worker memory.

Model-facing tool results use a compact functional view (`id`, `name`,
`created_at_ms`, `size_bytes`, `sha256`), with pagination on version lists and
`safety_version_id` after restore. Archive format and transfer headers belong to
the management/download API, not the agent's tool vocabulary.

## Publication and recovery

The session DO serializes workspace operations. An explicit persist starts an
unused workspace if needed, captures the current state, and publishes its
immutable archive using the existing checksum-verified native stream. Versions
live in a separate immutable namespace: old deployed binaries' session-checkpoint
collectors cannot delete them during a rolling deployment or rollback. A durable
upload candidate reserves aggregate Space quota before transferring bytes. Its
exact source and intended metadata survive a lost acknowledgement. Publication
follows the independent archive and retained-byte receipt, before moving the
session checkpoint. An already-published owner archive with identical SHA-256
and byte count is reused without another copy. Repeated tool dispatch uses the
stable operation ID and returns the original version.

Both eager old-checkpoint cleanup and the scheduled orphan collector check
version references before deleting an archive as defense in depth. A first
version copy has its own physical retained-byte clock; later versions reusing
that archive do not create duplicate storage charges or consume duplicate bytes.
Storage accounting continues after the session closes; insufficient funding
does not authorize deletion of user data.

Pending upload candidates consume the same aggregate byte budget as files.
Version metadata also has a plan-bounded count. A bounded maintenance sweep
reclaims unpublished candidates after twenty-four hours, first claiming the
expired transfer lease atomically so a concurrent retry cannot publish a deleted
object. Published versions are never age-expired. A missing source after a failed
copy is an explicit error, not permission to silently snapshot different state
under the original operation identity.

Restore is a destructive operation only on the *live view*, never on the saved
history: the current workspace is first persisted as a safety version. The
selected version must belong to the same account but may come from a different
session. Its native stream is copied to the destination session's immutable
checkpoint namespace with checksum verification. This preserves the existing
session/download prefix contract without exposing storage details to agents.

A durable restore intent precedes the daemon's atomic generation switch. Only
after that switch succeeds does the DO publish its new checkpoint manifest.
Interrupted restores are reconciled before any later Bash or checkpoint;
repeating the same replacement is safe at that fence, whereas repeating later
commands would not be. The saved result precedes intent removal. No working
directory is overwritten through Bash and no archive is extracted in Worker
memory. A failed upload or validation leaves the previous canonical workspace
and its safety version available. A failed post-switch publication retains the
intent and blocks later mutation until recovery can complete.

R2 operations use the existing run or workspace-maintenance reservation;
published archives use existing retained-byte accounting. Explicit downloads
reserve and settle their observed R2 GET before returning the native body.

This design relies on [R2 strong read-after-write consistency](https://developers.cloudflare.com/r2/reference/consistency/)
and the [Workers streaming object API](https://developers.cloudflare.com/r2/api/workers/workers-api-reference/).
It does not make a cross-service atomic-transaction claim: unpublished archive
candidates remain discoverable through the existing write-ahead orphan journal.
