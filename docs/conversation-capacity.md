# Conversation capacity

Conversation retained JSON has an owner-wide logical-byte budget, independent
of Space's physical-object budget. Both currently use the plan's configured
storage-capacity value; they are **two independent budgets**, not one shared GB
pool. This is an admission limit, not a monetary storage tariff. No R2 byte-time
or unconfigured Durable Object storage price is fabricated for these bodies.

## What counts

`Payload::json` counts actual canonical UTF-8 JSON bytes, the conversation/object
identity bytes, and a fixed 256-byte logical-object allowance. This includes metadata, immutable item bodies and
retained receipt/index objects. An admission snapshot references existing item
IDs, so it does not charge their bodies again; the separately retained receipt's
encoded ID list itself still occupies capacity. R2/file references count only
their encoded reference representation, never the referred binary's byte size.

Immutable payload allocation IDs combine the authority key and content digest.
An identical retry reuses the same allocation; an unpublished rejected item can
later use a different payload without colliding with a previous failed attempt.
The mutable authority header uses monotone 4 KiB allocation blocks, not one full
header allocation per mutation. Small metadata updates reuse the existing blocks;
shrinking metadata never claims that obsolete chunks were physically removed.
Each live item prepays its bounded future tombstone slot, so hiding retained data
remains possible at a full budget. Revision-width growth is included as well.

## Ownership and failure protocol

1. The authority prepares exact bytes and a stable logical ID before writing.
2. A small durable publication intent records the complete quota identities
   **before** any D1 reservation. `reserve_batch` atomically checks the owner's
   counter and inserts all required immutable allocations/header blocks.
   Identical retries reuse the same reservation. A different digest/size under
   an existing ID is a conflict, not a new allocation.
3. Write bodies, then publish the authority header/receipt that exposes them.
4. `commit` changes reserved capacity to retained capacity, without increasing
   the total. A lost acknowledgement is retried idempotently.
5. On failure, read canonical publication state. A matching target header digest
   proves publication, including a lost storage acknowledgment. A matching old
   header proves non-publication; remove candidate body/index/chunks first, then
   refund only reserved allocations. A timeout, mixed header, missing transport
   result or old timestamp is not proof. Unknown publication keeps its intent and
   capacity hold; it has no TTL refund.
6. Item/conversation DELETE changes visibility and **does not refund capacity**:
   their bodies remain retrievable or referenced by earlier accepted snapshots.
   Only actual content removal with no remaining dependency allows
   `ContentRemoved`, including an already-retained allocation.

The D1 owner counters and payload transition triggers execute in the same SQLite
statement transaction. Separate Conversation DOs cannot both pass the capacity
check against an old total. Retained payloads cannot be downgraded to uncommitted
candidates, and unknown candidates never expire by TTL. Existing identical
reservations remain usable after a plan downgrade; new bytes are denied until
capacity is available. Release receipts remain as identity fences, so retries
cannot silently reuse an ID for different content.

## Integration

The capacity schema is additive in `0021_conversation_capacity.sql`; deployed
Conversation routing in `0015_conversations.sql` is not modified. `ConversationState` uses one publication wrapper for
items, provenance, admission receipts and header mutations. Admission snapshot
and visibility semantics remain unchanged. The D1 quota commit follows known
canonical publication; a failed quota acknowledgment preserves the successful
API mutation and exposes `X-Agents-Conversation-Capacity-Status: pending` rather
than inviting a duplicate append. The next funded actor call recovers it before
performing another mutation. Intermediate publication intents are removed after
the outcome is durable; they are not user history or telemetry.

All helpers take the existing `ConversationOperation`, so D1 metadata is measured
under the already-validated actor grant and its caller's account/Session funding.
No new hold is invented inside the authority. Quota rejection is a typed 409 with
`error.capacity_budget: conversation_retained_logical_bytes` and independent
reserved/retained/limit counters. Space physical object usage is not subtracted
from this logical budget. Reads and snapshot projection allocate no new bodies;
they may reconcile an already-published pending quota acknowledgment.

An older experimental header without the quota marker is adopted from canonical
retained keys before its next write. Existing bytes may record a grandfathered
overage, but new allocations still enforce the current plan. This adoption never
rewrites or deletes user payloads, and a lost acknowledgment is retried from the
unchanged legacy marker. No automatic tombstone/user-data GC is introduced.

Automatic Responses reserve their full canonical append envelope before user
journal acceptance and paid inference. Actual terminal publication atomically
converts that escrow into retained allocations and refunds the unused envelope.
See [the escrow integration](conversation-escrow-integration.md).

## Verification

`python -m unittest tests.test_conversation_quota_sql` executes the production SQL
against all actual migrations. Its nine tests check owner-wide
capacity, cross-owner isolation, idempotent commit/release/retry, downgrade
compatibility, immutable identity, no timeout refunds, retained-body protection,
500 randomized lifecycle transitions, and two real concurrent SQLite connections.
The small Rust unit tests cover encoded UTF-8 size, reference-not-binary
measurement and deletion-preserving header bounds. The new isolated actual-Worker
journey is `tests/worker/conversation-quota.test.mjs`; it checks immutable-byte
idempotency, bounded metadata allocation, no tombstone refund, two-actor capacity
races, owner isolation, lost canonical-header acknowledgments, and cold recovery
after quota-commit failure. Its fault shim exists only in the Miniflare fixture
module graph. This journey has syntax validation only until the captain supplies
a newly compiled Worker artifact; no old artifact is used as acceptance evidence.


## Why a database reservation, not per-DO counters

A budget spanning independent Conversation actors is a numeric invariant, not a
local cache statistic. [Balegas et al., *Extending Eventually Consistent Cloud
Databases for Enforcing Numeric Invariants*](https://arxiv.org/abs/1503.09052)
studies why independently updated counters need coordination or escrow rights to
preserve a bound. Here the existing D1 database is already the shared admission
authority, so a single conditional write plus [SQLite transition
triggers](https://www.sqlite.org/lang_createtrigger.html) is simpler than introducing
an escrow/replication protocol. The capacity predicate and counter change belong
to one statement, not a Worker-side read-then-increment sequence. Cloudflare's
[database API](https://developers.cloudflare.com/d1/worker-api/d1-database/)
also provides transactional batches, but this helper deliberately avoids a
multi-statement capacity decision or a cross-DO transaction assumption.


Quota commands currently use the primary `D1Database` binding directly. According
to Cloudflare's [read-replication contract](https://developers.cloudflare.com/d1/best-practices/read-replication/),
queries outside the Sessions API stay on the primary. If this path later adopts
replica reads, the reservation receipt read must use a sequentially consistent
session/bookmark after its write, not an unconstrained replica.

## Explicit irreversible purge

`POST /v1/conversations/{id}/purge` is a platform extension separate from
metadata/item DELETE. It requires deleted metadata, no writer and no reserved
response capacity. An owner-bound durable marker fences all later reads and
internal appends before removing every local item, provenance, image-reference
and admission-snapshot body, including chunks. Image references are released
idempotently; independent Response/Session copies are untouched. Only confirmed
body removal releases their logical allocation rows. Header blocks and the
deleted identity/marker remain retained and charged. A cold interrupted purge
resumes from the same marker; no TTL or visibility flag authorizes a refund.
The new controlled Worker journey covers retained metadata deletion, foreign
purge denial, real body reclamation, inaccessible purged bodies and cold retry.
It remains uncompiled until the next composed Actions artifact.
