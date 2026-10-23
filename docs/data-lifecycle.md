# Data lifecycle and checkpoint maintenance

## Ownership classes

| Class | Examples | Retention authority |
| --- | --- | --- |
| User data | Current workspace snapshot, current R2 manifest, conversation archives, pinned instructions and skill revisions | Explicit account/session lifecycle; never blanket age expiry |
| Recoverable intermediate state | Immutable uploaded checkpoint not referenced by the current manifest; upload intents | Bounded scheduled garbage collection after a 24-hour grace period |
| Financial provenance | Resource storage clocks, tariff snapshots, reservations, usage ledger | Billing retention policy; checkpoint GC never removes ledger rows |
| Observability | Traces, aggregate GC counters, consented diagnostic uploads | Observability retention and diagnostics consent; no raw private paths in global GC logs |

The R2 `manifest.json` pointer is authoritative for workspace restoration. A D1
session projection or DO cached manifest can lag publication and is not sufficient
proof that an archive is disposable. Session closure does not delete its current
workspace or conversation.

## Write-ahead checkpoint discovery

Before each immutable archive PUT, `workspace::checkpoint` writes
`workspace_checkpoint_intents` in D1. Failure to persist the intent prevents the
PUT. This captures uploads that fail halfway, uploads whose billing registration
fails, and checkpoints whose manifest never commits. Keys contain hashed owner
and session components, a compressed-content digest, and a fresh UUID; an upload
key is never reused. No per-file metadata operations or bucket scans are added.

The regular Workers scheduled event selects at most 64 due records using the
`(next_check_ms, object_key)` index and a durable keyset cursor. It requests
collection from the owning session DO, not directly from the bucket. Busy
sessions defer without interrupting work. The DO excludes runs, checkpoint
publication, archival, admission and alarm execution while collection is in
progress. This fence matters: an age check and an R2 pointer read alone could
race a delayed publisher and delete the snapshot just before it becomes current.

Inside that fence, collection revalidates ownership and the grace period, reads
the authoritative manifest, and retains any archive it references. A malformed
or unreadable manifest fails closed. Current objects are checked again no sooner
than 24 hours later; changing the pointer eventually makes an old object eligible.
R2 strongly consistent reads are required, without CDN or external cached reads.

## Retry boundaries and cost

1. Intent without an object: idempotent R2 deletion succeeds and removes the intent.
2. Failed deletion: intent and retained-storage clock remain; the next scan retries.
3. Deletion followed by a crash: retry repeats the idempotent delete, then closes
   the retained-storage clock. No clock is closed merely because a snapshot is old.
4. Clock closure followed by a crash: retry preserves the immutable removal time
   and then removes the intermediate intent.
5. Failed per-record requests advance the scan cursor, so one unavailable session
   cannot starve every later record. A completed pass wraps and retries them.

Immediate best-effort deletion after successful publication remains a latency
optimization; the durable intent is the recovery path. The old intent may remain
until a scheduled sweep, even when the old object is already absent. Financial
rows remain for reconciliation. Maintenance manifest reads, D1 bookkeeping and DO
requests are platform overhead included in service pricing; they do not fabricate
user reservations. Cloudflare does not charge R2 DELETE operations. User-triggered
checkpoint writes retain their normal metering path.

Existing pre-migration objects do not acquire intents retroactively: collection
never performs a speculative full-bucket sweep. Deploy migration 0008 before the
worker that writes intents.

## Verification and platform references

`tests/test_checkpoint_gc_sql.py` runs the real migration and production SQL with
SQLite, exercises current/conversation retention, upload and deletion crash
boundaries, tenant isolation and randomized indexed cursor coverage. Rust tests
validate hashed owner/session archive scope. Cloudflare staging is still required
to verify real scheduled dispatch, DO exclusion and R2 failures end to end.

- [R2 consistency](https://developers.cloudflare.com/r2/reference/consistency/):
  reads and deletes observe completed operations globally.
- [Rules of Durable Objects](https://developers.cloudflare.com/durable-objects/best-practices/rules-of-durable-objects/):
  storage input gates alone do not serialize external asynchronous I/O.
- [R2 pricing](https://developers.cloudflare.com/r2/pricing/): delete operations
  are free; reads still have an operational cost.
