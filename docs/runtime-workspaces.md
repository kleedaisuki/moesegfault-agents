# Rust Durable Object workspace bridge

The session Durable Object owns a single Cloudflare Container. `crates/agents-worker/src/workspace.rs` connects the session harness to the credential-isolated daemon described in [workspace.md](workspace.md). The DO caller must serialize workspace startup, execution, checkpoint, and release. Explicit cancellation is allowed to run while execution is pending.

## Ownership and lifecycle

- Allocate the container lazily for the first Bash call. Time tools, model calls, and checkpointing an unused workspace do not allocate compute.
- Generate a random daemon bearer token in DO storage and pass only that token to the container supervisor. R2, model-provider, and account credentials never enter the container.
- Wait for authenticated `/health` readiness. A running VM is not necessarily an HTTP-ready daemon.
- Persist the restored daemon `boot_id`, not an in-memory initialized flag. DO eviction does not imply container restart: restoring an old archive over a still-live process would lose the latest user changes.
- The session owner chains alarms while runs are active. It must not treat absence of newly appended messages as permission to terminate ongoing execution. The ten-minute input-inactivity rule is evaluated after the run finishes.
- Before intentional release, checkpoint dirty state. If checkpoint publication fails, do not destroy the container. Platform crashes, forced shutdown, rollouts, and unplanned VM loss can still lose changes since the last successful batch; R2 is not a synchronous filesystem.

The current deployment uses the `default` container scheduling policy, whose image and instance size are defined in Wrangler. The newer `durable_object` policy requires an `image` or `containerSnapshot` startup option; changing policy is an application migration, not an interchangeable configuration toggle.

## Checkpoint transaction

1. When dirty, batch after eight tool executions or at least 64 MiB of conservative dirty bytes. The daemon fingerprints metadata to avoid snapshots for read-only tools. Deletions still count as dirty even when the resulting tree is empty.
2. Fetch a compressed archive with known content length, SHA-256, logical bytes, archive bytes, entry count, and format version.
3. Pass the platform's native response stream directly to R2. Do not collect the archive into WASM memory or copy every chunk through Rust.
4. Store the immutable archive under a SHA-256/unique-upload key. The unique suffix prevents a deleted content hash from accidentally reusing an already-closed storage billing identity. Supply SHA-256 to R2 so upload verification precedes publication.
5. Publish `manifest.json` last. This is the commit point: failed uploads leave the previous manifest intact.
6. Persist the committed metadata in DO storage, then delete the previous archive if its key differs. Cleanup failure does not invalidate the new checkpoint.
7. Acknowledge the exact archive digest to the daemon. It resets dirty tracking only if the snapshot revision still matches the workspace revision. Reset the DO tool counter only after acknowledgement.

Restore reads the manifest, validates its version and tenant/session prefix, checks archive size, and streams R2's body directly into the daemon. The daemon verifies the SHA-256 and archive safety constraints before switching the current workspace.

The live manifest and archive are **user data**, not expiring intermediate state. Do not attach blanket object TTLs to their prefix. A periodic orphan collector may remove unreferenced old archives after a grace period, but must first read each session's current manifest. Failed uploads or a crash between R2 publication and DO metadata persistence can leave such orphans. Conversation archives, diagnostics, and telemetry have separate ownership and retention rules.

## Execution recovery and integration

`execute_bash_with_id(tool_call_id, arguments)` hashes the stable harness tool-call ID into a safe daemon ID. Before POST, the bridge durably records the daemon boot ID under the dispatch ID. A repeated dispatch never causes another POST: it queries the daemon cache, or returns `tool_outcome_unknown` if the result is unavailable. A changed boot ID also returns `tool_outcome_unknown`, never a replay. POST conflicts and transport failures query the daemon's execution cache; a still-running command is polled. `cancel(tool_call_id)` derives the same daemon ID. The dispatch fence covers a crash after journal persistence but before HTTP dispatch by preferring uncertainty over duplicate side effects. It does not claim durable exactly-once execution.

Completed Bash output is returned even if a batch checkpoint fails, with `checkpoint_pending: true` and an authenticated-user `checkpoint_error`, to prevent an infrastructure retry from re-executing user side effects. Private filenames from unsupported entries may appear in this user-visible error, but not global logs or telemetry. The counter remains pending and release still fails closed. A successful batch returns `checkpoint` operation/byte statistics. `snapshot_metadata()` exports the last committed `agents_core::WorkspaceSnapshot` for the session record.

## Warm compute and resource accounting

`workspace_lease.rs` owns a resource reservation independent of model runs. Its idempotency key begins `workspace:`, not `run:`, so keeping an idle workspace warm does not consume a concurrent-run slot. It reserves thirty minutes of the **published Container `wall_ms` tariff**, plus `RUN_RESOURCE_RESERVATION_MICROUSD` for checkpoint maintenance. This is an explicit wall-time platform tariff, not a fabricated CPU measurement. The allocation tariff includes idle controller overhead. The workspace clock is Container-only; active Durable Object orchestration has one owner, `SessionInner::observed_step`, and is not charged again by a warm lease.

The lease stores the full validated pricing snapshot. When less than fifteen minutes remain, reserve the next lease **before** measuring and settling the old interval. The margin funds ten minutes of idle time plus a bounded five-minute operation. `ensure_compute_lease()` does nothing for an unused or stopped container; the first Bash call admits a lease before startup. The session owner invokes this hook before each provider step and bounds provider/tool operations to five minutes. Parent alarms must continue driving long runs and perform idle shutdown at the user-input deadline. A failed renewal stops further admission; it must not silently continue unfunded work.

The observed container clock starts before successful provisioning and closes after intentional destroy resolves, or splits at a lease renewal. Its event ID and fixed interval end are persisted before ledger writes, making retries idempotent. An unexpected VM exit has no trustworthy actual exit timestamp: its budget hold remains pending for delayed reconciliation, rather than inventing a runtime. A new daemon can use a new funded lease while the old uncertainty remains explicit.

`for_run(run_id)` selects the active run reservation for R2 operations. Without a run ID, idle checkpoint maintenance uses the warm lease. Each GET/PUT operation has a durable pending observation before the side effect, followed by its observed `class_b`/`class_a` operation count. Failed or uncertain calls remain pending, preventing automatic settlement. DELETE is not charged as a Class A/B operation.

Successful snapshot uploads and manifest generations register retained sizes with `reconcile.rs`. Successful overwrite closes the previous manifest generation's storage clock; successful deletion closes the old archive clock. Merely becoming non-current does not stop storage billing. Scheduled byte-millisecond reconciliation continues after the compute lease closes. It does not delete user data when funding fails. Registration follows known successful publication: a crash between R2 success and D1 registration remains an explicit pending operation/orphan reconciliation seam, not an exactly-once cross-service transaction.

## Rust SDK compatibility detail

The published `worker` 0.8.7 source has a recursive conversion in `impl From<ContainerStartupOptions> for js_sys::Object` (`options.clone().into()`), so `container.start(Some(options))` cannot safely be used. The bridge invokes the same native `Container.start()` through `js_sys::Reflect` with an explicit options dictionary. Container discovery, TCP-port fetches, and destroy continue to use the typed Rust SDK. Remove this narrow workaround when upgrading to a version whose conversion is fixed.

## Verification and upstream references

The bridge passed `cargo +stable check -p agents-worker --target wasm32-unknown-unknown` and `cargo +stable test -p agents-worker --lib workspace::` (4 passed). These pure tests cover bounded SHA-256 parsing, every byte's upper/lowercase hex roundtrip, path-key isolation, and compute-lease renewal boundaries. Container execution, snapshot corruption rejection, and restore behavior require daemon integration and the deployed session journey, not just the Rust type check.

References checked on 2026-10-23:

- [Cloudflare Durable Object Container API](https://developers.cloudflare.com/containers/api/durable-object-container/): direct lifecycle ownership, startup readiness, and startup options.
- [Cloudflare container lifecycle](https://developers.cloudflare.com/containers/concepts/architecture/): ephemeral disk and platform shutdown behavior.
- [Cloudflare scheduling policies](https://developers.cloudflare.com/containers/configuration/scheduling-policy/): default versus Durable Object-managed image selection.
- [R2 Workers API](https://developers.cloudflare.com/r2/api/workers/workers-api-reference/): native readable streams, checksums, and object operations.
- [`worker` 0.8.7 container source](https://docs.rs/worker/0.8.7/src/worker/container.rs.html): native Rust API and startup conversion defect.

## Durable lease wire format and tool failure boundary

The complete lease is stored as canonical JSON text at all three DO keys (candidate, current and by-ID), with compatible reads for older structured values. This is not a floating-point or tariff-unit conversion: the staging R2 byte-time denominator `2592000000000000000` and signed monetary amounts remain exact. The default `serde_wasm_bindgen` serializer rejects `u64` above JavaScript's safe integer range before issuing a storage write; embedding the raw pricing structure therefore previously prevented even the first Container lease from being admitted. Reservation hashes continue to use the unchanged canonical Lease representation.

Only model-correctable argument/unknown-tool failures become actionable tool-result messages. Workspace availability, storage-codec, pricing, budget and uncertain-execution failures stop the run with bounded safe codes instead of asking the model to blindly retry infrastructure. An ordinary nonzero Bash exit remains a normal tool result, preserving the agent's ability to inspect stderr and correct a command. Before a fatal stop, the harness persists a matching error result for the failed call and `not_executed` results for every remaining serial call in that assistant tool group, together with the terminal error. A restored run or a later explicit user append therefore cannot replay an old unresolved command or submit an invalid tool-message sequence. Operator logs contain only known tool names, fixed error families/codes and opaque run/session identifiers, never argument strings or output.
