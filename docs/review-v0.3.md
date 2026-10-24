# Independent v0.3 security and compatibility review

This is an evolving review record, not a release-acceptance receipt. Source files
are being implemented concurrently; incomplete scaffolding is not treated as a
confirmed finished-code defect. Findings require an executable path and a
violated external or documented internal contract.

## Review boundaries

The review covers owner-scoped response/file/version routing, durable execution
and retry boundaries, local function results, compaction instructions, upload and
restore safety, accounting and retained storage, and v0.2 compatibility. Responses
protocol review is recorded separately in `review-v0.3-responses.md` when available.

## Initial checks

- The native SQLite suite passed **87/87** against the evolving schema after
  account-resource, response-index and immutable-version tests were added.
  This is schema/SQL evidence, not a compiled Worker or deployed-user journey.
- Migration 0011 rebuilds resource observations and retained-storage tables to
  support account-owned files without inventing session identities. Its account
  specialist tests cover populated prior data and rollback of a failed schema
  transaction; independent old-writer and retained-reference review continues.
- Workspace versions use owner-filtered lookup and immutable D1 metadata. Current
  code protects published archive references in both checkpoint replacement and
  the older checkpoint collector. Restore first publishes a safety version and
  persists a replacement intent before replacing the daemon workspace. These are
  implementation observations; crash/fault coverage remains to be checked.

## Operational compatibility boundary to resolve in release guidance

### P1: published workspace versions must not share legacy collectible keys

The initial implementation publishes a version by referencing the current
`workspaces/.../snapshots/...` checkpoint key. New collectors check version
references, but the v0.2.0 `checkpoint_gc::collect` deletes any due non-current
snapshot, and its checkpoint replacement also deletes the previous archive
without that lookup. An old isolate or rollback can therefore destroy a version
the user was promised was immutable. The affected sites are
`workspace.rs::publish_version` and `workspace_versions::publish`, with the older
delete paths in `workspace.rs::checkpoint_active` and `checkpoint_gc.rs::collect`.

Correction sent to the Space lead: publish version bytes into a distinct
version-owned immutable key namespace outside legacy checkpoint intents, keep an
independent retained-storage clock, and make publication/retry recoverable. Reuse
an already protected owner/digest archive for unchanged commits rather than
copying on every version. A new collector reference check alone is insufficient.
Status: **source corrected; runtime verification pending**. Version publication now
copies into owner-scoped `space-versions/...` keys and reuses an existing protected
owner/digest archive for unchanged content. A separate pending-upload journal
admits quota, fences expiry and prevents unpublished archives from accumulating
indefinitely. Legacy checkpoint keys remain separate and compatible.

### P1: interrupted upload needs a terminal, non-starving recovery path

In the initial `space.rs::upload`, the quota-counted `uploading` row and pending
maintenance observation precede the R2 PUT. A rejected checksum or disconnected
body exits through `await?`. `recover` only handles an existing matching object;
missing/mismatched objects remain `uploading` indefinitely, and users cannot
delete those rows through the public delete endpoint. Such rows consume byte/file
quota and retain their reservation. The oldest 32 unchanged rows also repeatedly
occupy the entire recovery query and can prevent later valid uploads from being
published.

Correction sent to the Space lead: bound and journal upload lifetime, distinguish
live transfer from an abandoned operation, give failed transfers a terminal quota
state while preserving truthful uncertain accounting, and use a cursor/backoff
that cannot starve later rows. Required regression includes failed PUT or checksum
rejection followed by recovery and a later successful upload. Status: **source
corrected; runtime verification pending**. The current code records known upload
outcomes and operation counts, aborts transfers against an absolute deadline,
backs off each recovery claim before I/O, and frees content quota after an
abandoned unknown operation while retaining its honest financial hold. Recovery
also distinguishes the pre-funding and pre-observation crash windows: those prove
no PUT could have been dispatched and can close without a fabricated charge.

The owner also added an explicit expired-deadline check before native dispatch;
a zero-delay timeout created after starting PUT would not have been such a fence.
The new boundary helper rejects equality and expired admission. Compiled
failure-injection tests must confirm the final implementation, not merely the
successful upload path.

### P2: upload must refresh account allowance before reserving

The initial `upload` calls `maintenance_account_reservation` without allowance
synchronization. `account_plan` reads subscriptions but does not grant current-cycle
credit. A long-lived API key can therefore receive 402 when upload is its first
operation in a new allowance period, despite the available entitlement. The file
download path already synchronizes. Correction sent to the owner: synchronize
before upload reservation, and apply the established retained-debt admission
policy for newly retained uploads. Status: **source corrected; runtime verification
pending**. Upload now synchronizes allowance and rejects retained-storage debt
before reserving additional resources.

### P1: recheck complete admission state after attachment resolution

The initial Responses admission checked closed/archive state before resolving
attachments, but its final post-await guard checked only run/head/input revision
and the transient `closing` flag. An idle archive could finish during file I/O,
reset `closing`, and leave the same head/revision with a Closed record. The code
then changed the instruction prefix, appended historical instructions and cleared
pending local calls before the domain's append operation rejected the Closed
session. That could lose a pending function continuation on a rejected request.

Status: **source corrected; compiled verification pending**. Both yielding
preparation boundaries now recheck archive preparation/pending/closing and Closed
state. Before changing the prefix or consuming pending calls, admission preflights
append/start on a small record clone without another await. No full-journal clone
or second execution engine is needed to enforce the all-or-nothing boundary.

Once account-scoped storage rows exist, a whole-Worker rollback to the v0.2.0
binary cannot safely be assumed. Its global retained-storage sweep deserializes
`session_id` as `String`, whereas v0.3 permits `NULL`. Session-scoped old SQL remains
compatible, but the old global sweep would reject a batch containing an account
file. The architecture now explicitly identifies this rollback floor: restore a
build with the nullable account-resource reader or ship a forward patch, not the
unmodified v0.2 scheduled Worker. This is distinct from data migration loss or a
current-build tenant-isolation defect.

## Relevant platform constraints

Cloudflare D1 keeps foreign keys enabled in migrations and uses implicit
transactions; temporary violations require `defer_foreign_keys`, not disabling
`foreign_keys`. Inspect the resulting foreign-key graph and populated data, not
only whether a fresh schema creates successfully.
[D1 foreign-key guidance](https://developers.cloudflare.com/d1/sql-api/foreign-keys/)

R2 writes/deletes are strongly consistent after the operation resolves. Immutable
object identity and owner-scoped publication metadata must still bridge D1/DO/R2
failure boundaries; consistency alone is not a cross-service transaction.
[R2 Workers API](https://developers.cloudflare.com/r2/api/workers/workers-api-reference/)

## Pending before approval

Review final sources, exact-source compiled Worker tests and real SDK/file/version
journeys. No release approval is given by this initial record.
