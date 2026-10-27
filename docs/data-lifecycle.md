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

A newly imported historical branch has a deliberate pre-publication exception:
its owner-validated seed and strong archive reference are durable in the child
DO, and `workspace:seed-pending` remains set until first admitted workspace use
publishes its R2 manifest. Import itself performs no unfunded R2 operation and
does not start a container. The seed is not a disposable cache entry.

## Space data and immutable workspace versions

An account has one Space independent of its agent sessions. Files are mutable
only through display-name changes or explicit deletion; their original bytes,
digest and identity do not change. Published workspace versions are immutable
user data, including when their source session is closed or its current
checkpoint advances. The public user API cannot rename, replace or delete a
saved version. Legacy `space-versions/` archives remain untouched. New live
checkpoints, saved versions and historical response boundaries share immutable
owner blobs outside the legacy session-checkpoint collector's namespace; their
physical bytes have one account-owned retained-storage clock. Strong references
are acquired before pointer publication, and stale current references are
released only after committed publication or successful restore recovery.

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

Deferred binary attachments acquire owner-scoped file references before a
response is durably accepted. While any accepted response still needs a source
file, deletion returns `409 file_in_use`; it does not quietly remove bytes that
a queued or running task has not imported yet. References are released only
after canonical terminal response state commits, including confirmed cancellation,
not on observer disconnect, a cancellation request, or a guessed timeout.
Only the owning session DO may reconcile an orphan reference against its
authoritative response state, serialized with admission. A failed state read is
not proof that a reference is disposable. The underlying file's retained-byte
clock continues normally while referenced. Reference/admission metadata is
bounded control-plane overhead included in service pricing, not a second
storage operation or an independent financial reservation.

The v0.3 service rollback floor includes migration 0014 and pin-aware deletion,
upload-abort claims and authoritative DO reconciliation. Do not serve accepted
references with a pre-pin v0.3 binary: database guards protect status-transition
writers, but cannot intercept an older handler that deletes an R2 object before
checking its database state. The first v0.3 rollout must include the complete
pin-aware implementation. This service migration floor does not change the
existing v0.2 SDK, CLI or Chat Completions client contracts.

See [Space workflows and recovery](space.md) and
[workspace version publication](workspace-versions.md) for the user contract and
the durable commit points.

## Shared blob discovery and service migration floor

Migration 0016 adds the single `workspace_blobs`/`workspace_blob_refs` registry.
Despite its historical table name, this is one typed owner-object engine for
workspace archives and immutable generated images. The image kind has its own
MIME/size/key constraints; it cannot bypass workspace format-1 validation.
New candidate rows precede archive PUT, and readiness follows checksum/size
verification and physical storage registration. An indexed due scan examines at
most 64 rows per sweep; D1 atomically claims only expired objects with no strong
references or published-version row. The claim rejects late reference insertion.
HEAD repairs a missing storage clock, R2 deletion precedes clock closure, and
only then is candidate metadata removed. Referenced user data is never aged out.

Space capacity counts hidden archives, images, files and candidates once per
owner/physical object key, including bytes still deleting. Its raw UNION view is
filtered by owner before grouping; grouping the whole view first caused a global
scan in the measured SQLite query plan. Provider capacity holds count against the
same limit before a paid image call but do not create physical storage charges.
Actual candidates atomically replace holds. Fork-transfer leases protect an
exported archive until its child has a durable independent current reference.
Only those intermediate holds/transfer leases have a 24-hour TTL.

Private WebSocket parent grants use the same transfer-reference engine with a
namespaced connection/response identity and their exact expiry, capped at 60
minutes rather than the normal fork-transfer day. Retrying a seal never extends
the original holder expiry. Connection authority is denied at expiry regardless
of later GC; disconnect cleanup releases only that private holder, never an
accepted child's current reference or source user data.

Generated image Files are metadata links. Explicit File deletion releases only
the file's strong reference; response results retain independent immutable bytes.
Active linked metadata also guards the pre-reference publication crash window.
Deleted-file provenance is retained without an unconditional blob foreign key,
so a metadata tombstone cannot pin physical bytes forever. Indexed bounded
maintenance repairs stale file references by checking explicit tombstones.
Deferred image metadata publication treats a deleted/deleting publication as a
terminal tombstone (`file_publication_deleted`). Both lookup and atomic insertion
honor all prior states; a partial active-row unique index alone would permit
resurrection after user deletion. The underlying canonical response image can
remain usable through its independent reference without republishing the File.

Published version references cannot be removed. A response reference is released
only after its owning DO durably tombstones that public response; descendant
sessions and other responses hold independent references. Failed cleanup remains
retryable rather than reversing the tombstone or deleting needed bytes.

Once a session publishes an owner-blob manifest, restoration requires a
blob-aware service binary and migration 0016. An older binary cannot decode the
trusted reference contract; rollback must preserve this service migration floor
or use forward recovery. Protected namespaces prevent old collectors from
deleting user data but do not make older restoration code compatible. SDK/CLI
snapshot schemas and public archive format remain unchanged.

## Legacy write-ahead checkpoint discovery

Existing session-prefix uploads retain their `workspace_checkpoint_intents`
discovery journal in D1. This captures uploads that failed halfway, uploads whose
billing registration failed, and checkpoints whose manifest never committed.
Keys contain hashed owner and session components, a compressed-content digest,
and a fresh UUID; an upload key is never reused. New owner blobs use the shared
registry above rather than creating a parallel legacy intent.

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
