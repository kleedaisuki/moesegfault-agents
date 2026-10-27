# Space, files and workspace versions

Every account has one private Space. Its identity is stable across API keys,
browser sessions and native CLI logins. Creating or renaming a Space does not
create an agent, start a workspace or change a conversation's activity clock.
Space names and file names are display metadata, never authorization grants or
storage paths. Access always uses the authenticated account's owner identity.

## User workflows

| Method | Endpoint | Behavior |
| --- | --- | --- |
| GET | `/v1/space` | Read the Space, name and storage/count limits |
| PATCH | `/v1/space` | Rename with `{ "name": "Research" }` |
| POST | `/v1/files` | OpenAI SDK multipart upload (`file`, `purpose`) |
| GET | `/v1/files` | Paginated file metadata |
| GET | `/v1/files/{id}` | Read one file's metadata and lifecycle diagnostics |
| GET | `/v1/files/{id}/content` | Download its original bytes |
| DELETE | `/v1/files/{id}` | Delete the upload; repeated confirmed deletion is safe |
| PATCH | `/v1/space/files/{id}` | Rename with `{ "filename": "notes.md" }` |
| GET | `/v1/space/workspace-versions` | List immutable saved workspace versions |
| GET | `/v1/space/workspace-versions/{id}` | Read a version's metadata |
| GET | `/v1/space/workspace-versions/{id}/content` | Download the saved workspace |

`/v1/space/files` and its child paths alias the Files endpoints. Uploaded file
names can repeat: a new upload has a new identity and never replaces another
file's contents. Rename changes only display metadata; content, digest and file
identity are immutable. Names cannot contain path separators or control bytes.
Deleting an upload does not delete a saved workspace version or change an already
materialized workspace file.

An accepted response retains the source files it needs to materialize into its
workspace until the owning session has durably committed completion or
cancellation. Text/image bytes already copied into the durable execution input
do not need a live source-object reference. Deleting a retained upload returns
`409 file_in_use`; retry after the response finishes or is cancelled. This also
protects background execution after its HTTP observer disconnects. Retention has
no time-based expiry: a long task does not lose its inputs merely because it is
old. Metadata renames remain allowed because file content and identity do not
change.

File lists accept `limit` (1–100, default 20), `after`, `order` (`asc` or default
`desc`) and optional `purpose`. Cursors are owned file IDs; ordering uses the
creation timestamp and ID together, so equal timestamps do not skip entries.
The response is `{object:"list",data:[...],has_more,first_id,last_id}`. Each
processed file has this SDK-compatible representation:

```json
{
  "id": "file-0123456789abcdef0123456789abcdef",
  "object": "file",
  "bytes": 123,
  "created_at": 1792860000,
  "updated_at": 1792860000,
  "filename": "notes.md",
  "content_type": "text/markdown",
  "purpose": "user_data",
  "status": "processed",
  "sha256": "<64 lowercase hex characters>"
}
```

The additive `content_type` field preserves the upload's declared media type,
including after renaming. It is not a claim that file bytes have been decoded or
verified as that media format; clients must not infer media type from filenames.

An interrupted upload remains discoverable: SDK status is `uploaded` while
publication is pending and `error` after failure. The additive `diagnostics`
object contains its bounded `upload_status` and opaque `operation_id`, not
internal errors or object keys. Pending/failed contents cannot be downloaded or
used as model input. A failed upload can be deleted through the ordinary API.
Active transfers must first finish or reach their bounded recovery deadline.

## Upload paths and bounds

The standard SDK flow needs no provider credential:

```python
from pathlib import Path
from openai import OpenAI

client = OpenAI(base_url="https://agents.moesegfault.dev/v1", api_key="<platform key>", max_retries=0)
uploaded = client.files.create(file=Path("notes.md"), purpose="user_data")
print(uploaded.id)
```

Multipart uploads are bounded to 16 MiB of file content plus 64 KiB of multipart
framing. They use the platform's native FormData/File and SHA-256 implementation;
the complete file is not copied through WASM. This path does buffer the native
multipart body, so it is deliberately smaller than the streaming upload path.

For larger files, `magents` uses:

```text
POST /v1/space/files?filename=<percent-encoded UTF-8>&purpose=user_data
Content-Type: application/octet-stream
Content-Length: <exact byte count>
X-Content-Sha256: <64 lowercase hex characters>

<original binary body>
```

Both paths require a known Content-Length. Raw uploads support up to 64 MiB,
including an empty file with the empty-body digest. A native fixed-length stream
enforces the byte count, and R2 verifies SHA-256 before accepting the object.
An abortable native pipe bounds upload input to 120 seconds. The handler awaits
both transfer and storage outcomes after cancellation; it does not free a live
candidate's quota merely because an observer disconnected.

`purpose` accepts `user_data`, `assistants`, `agents`, `vision`, `batch`,
`fine-tune` and `evals` as storage tags. Supporting these tags does not create
batch, fine-tuning or other unrelated APIs. Files persist until explicitly
deleted; no expiring-upload promise is silently inferred from a tag. Multipart
fields other than `file` and `purpose`, including an unsupported explicit expiry
request, are rejected rather than silently discarded.

## Capacity and accounting

| Plan | Space bytes | Uploaded files | Workspace versions |
| --- | ---: | ---: | ---: |
| Free | 256 MiB | 100 | 100 |
| Go | 1 GiB | 1,000 | 1,000 |
| Plus | 10 GiB | 5,000 | 5,000 |
| Pro | 100 GiB | 50,000 | 50,000 |

These are deployment safeguards, separate from spendable credit. Uploads reserve
space bytes atomically before storage begins. Space usage includes active upload
reservations, uploads pending deletion, distinct saved workspace archives and
version candidates not yet removed. Several versions referencing the same
physical archive count those archive bytes once. Metadata/version count limits
remain independent. A downgrade does not delete data: existing content remains
readable, but additions exceeding the current plan are rejected.

Resource accounting reuses the existing ledger. Space operations and retained
objects have an account owner and no fabricated session. Successful R2 storage
publication registers its retained-byte clock; deletion closes that clock only
after physical deletion succeeds. R2 operations are reserved and observed before
settlement, independently of agent execution concurrency. Outstanding storage
debt prevents additions, not authenticated reads or deletion. Egress is not
invented as a charge: the configured R2 operation/storage tariffs apply.
Bounded attachment-pin admission, ownership checks and retention bookkeeping are
control-plane overhead included in service pricing, not separate synthetic D1
charges or new financial holds. Actual R2 operations and retained bytes remain
metered as before.

## Recovery and data classes

The file row reserves capacity before upload. Publication requires a verified
object, retained-byte registration and observed operation accounting before the
row becomes `processed`. The row records a definitive stored/failed result
before financial finalization, making retries independent of the HTTP observer.

A known failed transfer deletes its candidate, settles the known dispatched
operation count (zero for a proven pre-dispatch setup failure) and frees content
quota. A process crash is different: recovery waits for the transfer deadline
and grace period, checks the candidate, then publishes a verified object or
removes an absent/invalid candidate. An unknowable external-operation outcome
retains its financial reconciliation hold rather than fabricating zero usage;
its failed file metadata exposes the operation ID. Content quota is not held
forever by that separate accounting uncertainty.

Scheduled recovery is bounded and advances each candidate's retry time before
I/O. One unavailable object or account cannot monopolize the oldest batch.
Uploads and immutable versions are user data, not telemetry. Incomplete upload
rows, transfer leases and abandoned candidates are recovery state; diagnostics
contain only IDs and safe lifecycle codes. No blanket R2 expiry applies to
accepted files or versions.

Attachment references are inserted atomically before canonical response
acceptance while the owning session holds its admission/persistence mutex. A
missing, deleting, cross-owner or changed file aborts the entire reference set.
The same database interlock prevents deletion from winning after retention has
been acquired. References are released only after authoritative DO state proves
terminal/cancelled execution, or proves a pre-acceptance reference was never
accepted; a missing routing read or elapsed time is not such proof. Interrupted
release retains the input and retries instead of withholding an already-known
terminal answer. Upload-abort cleanup also claims its own candidate before
physical deletion and cannot delete a subsequently published or retained file.

## Using files and saved workspaces with agents

Responses inputs reference an owned file ID. Small UTF-8 files can enter the
model prompt as bounded text. Binary or larger attachments require a workspace
and are copied into a dedicated safe upload location after execution admission;
they are not interpreted as shell commands or placed at arbitrary user paths.
Images are separately validated by the Responses/provider adapters. Inline
file-data publication uses a response/item-specific idempotency key so a retry
cannot duplicate a completed upload or substitute different bytes under that
key.

Agents use functional workspace tools: `workspace_commit` saves a version,
`workspace_versions` discovers versions across the owner's Space, and
`workspace_restore` restores an owned version. Versions have no user-facing
update/delete endpoint. Restore preserves the current workspace first and uses
the daemon's verified, atomic generation switch; it does not overlay an archive
onto a dirty live directory. The saved version, session's current checkpoint and
physical workspace generation have distinct lifetimes. See
[workspace persistence](runtime-workspaces.md) for the underlying durability and
failure contract.

## Implementation references

- [Cloudflare R2 Workers API](https://developers.cloudflare.com/r2/api/workers/workers-api-reference/):
  native streams, checksums, strongly consistent publication and deletion.
- [Cloudflare R2 Workers usage](https://developers.cloudflare.com/r2/api/workers/workers-api-usage/):
  avoid body re-consumption and whole-file copies under Worker memory limits.
- [Cloudflare R2 error codes](https://developers.cloudflare.com/r2/api/error-codes/):
  a known rejected operation is distinct from a disconnected observer or crash.

Downloads hand the native R2 response body to the client without per-chunk WASM
copying. After the baseline passed actual-Worker CI, the content endpoint adds
bounded conditional and partial reads without changing ordinary SDK downloads.

## Conditional and partial downloads

`GET /v1/files/{id}/content` and its Space alias use the quoted immutable SHA-256
as their strong `ETag`. Renaming a file does not change this identity. The caller
must still be authenticated and own a `processed` file before any precondition
is evaluated: a guessed ID, failed upload or deleted file never receives `304`.
Responses remain private and `no-store`; this is explicit client revalidation,
not a shared CDN cache or permission to reuse authenticated data across owners.

| Request | Result | R2 GET operations |
| --- | --- | ---: |
| Ordinary GET | `200`, full original bytes | 1 |
| Matching `If-None-Match` (strong, weak, list or `*`) | `304`, no body | 0 |
| Satisfiable single byte range | `206`, selected original bytes | 1 |
| Invalid/unsatisfiable single byte range | `416`, small JSON error and `Content-Range: bytes */<size>` | 0 |
| Mismatching/weak/date `If-Range` | `200`, full original bytes | 1 |
| Unknown range unit or multiple ranges | `200`, full original bytes | 1 |

Supported forms are `bytes=10-19`, `bytes=10-` and `bytes=-10`. Range units are
case-insensitive. End positions and suffix lengths clamp to the file boundary;
an offset beyond the last byte, reversed bounds, a zero-length suffix, decimal
overflow and any single range against an empty file return `416`. Multiple ranges
are intentionally ignored instead of creating a multipart representation.
`If-None-Match` is evaluated first. `If-Range` permits a range only when it equals
the exact strong ETag; unsupported dates and weak validators fall back to a full
response so a client cannot assemble bytes from different versions.

Partial responses include `Accept-Ranges: bytes`, the selected `Content-Range`
and selected `Content-Length`. R2 receives the normalized offset/length and streams
only that range directly to the client. **`X-Content-Sha256` always describes the
whole original file, not the selected range**; verify it after assembling the
complete file. Each actual R2 GET still records one Class B operation, with no
synthetic egress charge. A `304` or rejected range performs no storage GET and
opens no download reservation. The relevant HTTP semantics are defined in
[RFC 9110](https://www.rfc-editor.org/rfc/rfc9110.html#name-range).
