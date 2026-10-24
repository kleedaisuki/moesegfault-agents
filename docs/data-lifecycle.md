# Data lifecycle and checkpoint maintenance

## Ownership classes

| Class | Examples | Retention authority |
| --- | --- | --- |
| User data | Uploaded Space files, published immutable workspace versions, current workspace snapshot and R2 manifest, conversation archives, pinned instructions and skill revisions | Explicit account/session lifecycle; never blanket age expiry |
| Recoverable intermediate state | Unpublished file/version candidates, unreferenced session checkpoints, upload intents and transfer leases | Bounded lifecycle-specific recovery; checkpoint/version orphan collection after a 24-hour grace period |
| Financial provenance | Resource storage clocks, tariff snapshots, reservations, usage ledger | Billing retention policy; checkpoint GC never removes ledger rows |
| Observability | Traces, aggregate GC counters, consented diagnostic uploads | Observability retention and diagnostics consent; no raw private paths in global GC logs |

The R2 `manifest.json` pointer is authoritative for workspace restoration. A D1
session projection or DO cached manifest can lag publication and is not sufficient
proof that an archive is disposable. Session closure does not delete its current
workspace or conversation.

## Space data and immutable workspace versions

An account has one Space independent of its agent sessions. Files are mutable
only through display-name changes or explicit deletion; their original bytes,
digest and identity do not change. Published workspace versions are immutable
user data, including when their source session is closed or its current
checkpoint advances. The public user API cannot rename, replace or delete a
saved version. These versions have independent archive keys outside the legacy
session-checkpoint collector's namespace, so mixed-version sessions cannot
accidentally reclaim them. Repeated versions may share one owned immutable
archive; its physical bytes have one retained-storage clock.

File publication follows checksum-verified object storage, retained-byte
registration and operation accounting. Pending uploads reserve capacity before
storage begins. Known failures release content capacity after cleanup and settle
only known operation quantities. A process crash with an unknown dispatched
operation may leave a financial hold even after content recovery is finished;
that accounting uncertainty does not authorize deleting accepted user files or
keeping failed content capacity reserved forever. Owner-only file diagnostics
identify the operation without exposing private storage keys.

File recovery advances an indexed retry deadline before external I/O and bounds
each batch. Version-copy candidates reserve the same aggregate Space byte budget
as uploaded files. Their separate collector claims an expired transfer lease
before removing an unpublished object. Abandoned bytes continue consuming
capacity until physical deletion succeeds. Published file and version prefixes
must never receive a blanket R2 expiration policy.

Space resources reuse the existing account ledger and retained-byte reconciler
without creating a synthetic session. The nullable account resource schema is a
migration floor for the background storage reconciler: a pre-v0.3 reconciler
cannot decode account-only storage rows. Forward recovery must preserve these
rows rather than rolling back to an incompatible binary. Existing session
resource constructors and their owner/immutability constraints remain intact.

See [Space workflows and recovery](space.md) and
[workspace version publication](workspace-versions.md) for the user contract and
the durable commit points.

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
