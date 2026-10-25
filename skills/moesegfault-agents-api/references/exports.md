# Files, Space, and exported artifacts

Use the same trusted-backend, owner-scoped platform bearer authentication as session reads. These are private transfers, not public artifact links. All paths below are relative to `https://agents.moesegfault.dev`, not an SDK base already containing `/v1`.

The examples use the established Files/Space path. Expanded native media,
remote-source freezing, hosted artifacts and retrieval indexes require their
selected adapter/runtime acceptance; a parser or successful upload alone does
not establish deployed model consumption. Do not carry old attachment limits or
unsupported-type lists into every new native transport without checking it.

## SDK Files and response attachments

Files live in the caller's single account Space, independent of any agent or session. The examples share `client`, `origin`, and `agent_id` from the [Responses setup](responses.md). Uploading storage does not itself ask a model to process the file.

```python
uploaded = client.files.create(
    file=("notes.txt", b"Build passed. Next: review the release checklist.\n", "text/plain"),
    purpose="user_data",
)
print(uploaded.id, uploaded.bytes)
attached = client.responses.create(
    model=agent_id,
    input=[{
        "role": "user",
        "content": [
            {"type": "input_file", "file_id": uploaded.id},
            {"type": "input_text", "text": "Summarize the attached notes."},
        ],
    }],
)
if attached.status != "completed":
    raise RuntimeError(f"Attachment response ended as {attached.status}")
print(attached.output_text)
```

`client.files.list(limit=20, order="desc", purpose="user_data")` lists nondeleted file metadata with standard SDK pagination; `client.files.retrieve(uploaded.id)` reads metadata, and `client.files.delete(uploaded.id)` explicitly deletes that mutable Space entry. Status is `uploaded` while publication is pending, `error` after failure, and `processed` only when committed. Additive `diagnostics` gives safe lifecycle/operation IDs; pending/failed content is not downloadable or usable as model input. Inspect an interrupted upload's original receipt rather than assuming failure and duplicating storage. File names are display metadata, never an object key or client-selected workspace path. A purpose label is not a promise of hosted Assistants, Batch, fine-tuning, or vision support.

Download bytes without decoding or buffering an entire large file:

```python
from pathlib import Path

destination = Path("downloaded-notes.txt")
with client.files.with_streaming_response.content(uploaded.id) as download:
    with destination.open("xb") as output:
        for chunk in download.iter_bytes():
            output.write(chunk)
```

Use an application-chosen destination; do not overwrite existing files or trust a remote filename as a path. Validate declared length and `X-Content-Sha256` when publishing downloaded content; the metadata also carries `sha256`. Content uses safe disposition, `nosniff`, and a digest ETag. Neither a file ID nor an archive ID is an access capability. Missing and other-owner IDs return the same 404.

The SDK multipart path is bounded to 16 MiB file bytes (plus bounded framing) and requires known Content-Length. Larger uploads use the raw path up to 64 MiB with `filename`/`purpose` query fields and `Content-Length`, `Content-Type`, and `X-Content-Sha256` headers. Space quota and resource admission apply independently; do not guess limits from upstream OpenAI. Display the singleton's actual usage/limit metadata.

File storage accepts arbitrary bytes; model consumption is separate. The
established text/fallback path renders bounded UTF-8 or imports supported binary
content into an enabled workspace with a safe materialization receipt, rather
than pretending a text-only model read a PDF or image natively. Native adapters
must retain their actual file/media semantics. Use owned IDs or the accepted
inline path by default; remote URLs require an activated owner-safe resolver with
network/redirect checks, not an arbitrary client fetch used to simulate server
support. A renamed/deleted Space entry cannot rewrite already-admitted bytes.
Attachments remain untrusted data, not privileged instructions.

An established inline file uses `{"type":"input_file","filename":"notes.txt","file_data":"data:text/plain;base64,aGVsbG8="}`; raw base64 is also accepted. Supply either `file_id` or the matching filename/data form, never conflicting sources. Its bounded publication uses real quota/metering and an operation receipt, not another upload after an ambiguous reply. Prefer Files for reuse/large content; inspect selected request/decoded-content limits instead of inflating inline JSON or duplicating base64 across retained journals.

Vision is explicit: use a supported `input_image` with an owned `file_id`, or a
bounded supported raster data URL, and a genuinely vision-capable model/adapter.
Use the selected contract's detail and resource limits. Uploading an image as a
generic file does not enable vision, image generation, audio or another hosted
service. Rich modalities and remote inputs are candidate capabilities until their
resolver/provider journey is accepted, not a reason to advertise the whole full
contract as unsupported.

## Account Space and immutable versions

These backend HTTP paths use the platform bearer key; human cookie callers additionally use existing origin/CSRF protection. The account is derived from authentication, never a caller-supplied tenant ID.

| Intent | Request |
|---|---|
| Read singleton and quotas | `GET /v1/space` |
| Rename singleton | `PATCH /v1/space` with `{"name":"Release work"}` |
| Manage the same SDK files | `/v1/space/files` and `/v1/space/files/{id}` aliases |
| Rename file metadata only | `PATCH /v1/space/files/{id}` with `{"filename":"release-notes.txt"}` |
| List account-wide versions | `GET /v1/space/workspace-versions?limit=50&after=<last-id>` |
| Read immutable version metadata | `GET /v1/space/workspace-versions/{id}` |
| Stream immutable tar.zst archive | `GET /v1/space/workspace-versions/{id}/content` |
| Commit idle open session | `POST /v1/sessions/{id}/workspace/commit` with `{"operation_id":"checkpoint_01","name":"Before migration"}` |
| Restore idle open session | `POST /v1/sessions/{id}/workspace/restore` with `{"operation_id":"restore_01","version_id":"<owned-version-id>"}` |

File lists accept `limit` 1–100, `after`, `order` (`asc`/`desc`), and `purpose`. Version lists use newest-first `after` pagination with `limit` 1–100 and `has_more`/`last_id`/`next_cursor`; they span all of the owner's sessions. Do not assume the first page contains everything or fetch an entire bucket.

Versions are immutable: no rename, overwrite, or user deletion. Their public metadata includes `id`, `object:"workspace.version"`, `name`, source `session_id`, `created_at_ms`, generation, SHA-256, format version, archive/logical byte counts, and entry count—not private storage pointers. Use the authenticated content route and validate the snapshot headers as for session archives below.

Workspace agents can use registered `workspace_commit`, `workspace_versions`, and `workspace_restore` tools. Commit preserves the current workspace as a named version; restore first preserves current work as a safety version, then installs the selected owned version. Failed preservation must prevent replacement. Restore updates the selected existing workspace without mutating/deleting its source version. Historical Response branching separately creates an independent workspace from a trusted retained boundary. Space file changes are not automatic workspace synchronization. No operation should use Bash to bypass the durable preservation boundary.

The management mutations require a workspace-enabled, open, idle session; closed sessions need explicit resume. Persist the required `operation_id` (1–128 ASCII letters/digits/underscores/hyphens) before sending, and reuse it only to recover the same action. A new intended action needs a new ID. The Responses idempotency header is not a substitute. Commit returns a full version object; restore returns `{restored:<version>,safety_version_id}`; both include the management operation ID. Do not silently restore a workspace while implementing a read/download request.

Space is a service extension, not an imaginary `client.spaces` OpenAI resource. Use ordinary backend HTTP at the origin with `/v1` exactly once:

```python
import os
import httpx

with httpx.Client(
    base_url=origin,
    headers={"Authorization": f"Bearer {os.environ['MAGENTS_API_KEY']}"},
    timeout=30.0,
) as management:
    space = management.get("/v1/space")
    space.raise_for_status()
    print(space.json()["name"], space.json()["usage"])
    versions = management.get("/v1/space/workspace-versions", params={"limit": 20})
    versions.raise_for_status()
    for version in versions.json()["data"]:
        print(version["id"], version["name"], version["archive_bytes"])
```

## Source conversation

```http
GET /v1/sessions/<session-id>/export?format=jsonl
Authorization: Bearer <platform-api-key>
```

Supported formats are `json` (default), `jsonl`, and `markdown`; repeated or unknown query parameters return 400. Other-owner and absent sessions both return 404. Responses are attachments with `Cache-Control: no-store`.

Export contains the append-only **source journal**, including original tool calls/results and provider continuation fields, not just the compacted prompt or latest summary. Pending steering is kept separately; never label it processed. JSON has schema version 1, session pin, context/count metadata, `messages`, and `pending_messages`. JSONL starts with a `type:session` metadata record, followed by `type:message` and `type:pending_message` records with separate zero-based sequences. Markdown preserves structured records inside fenced JSON.

An export captures a point-in-time snapshot; a running session may append afterward. Reading/exporting neither starts a run nor resets the inactivity clock. `X-Export-Message-Count` and `X-Export-Pending-Count` describe the captured snapshot. Authentication/provider-key envelopes are excluded, but conversation text is not automatically redacted: users may have placed secrets there. Apply application authorization, retention, and sharing policy before displaying or redistributing it.

## Committed workspace archive

```http
GET /v1/sessions/<session-id>/workspace/archive
Authorization: Bearer <platform-api-key>
```

No query parameters are accepted. This returns raw **tar.zst** bytes with `Content-Type: application/zstd`, not JSON, ZIP, or gzip. The session must already be closed and its checkpoint committed. Request safe closure with `DELETE /v1/sessions/{id}` only when the user intends to finish; downloads do not implicitly close, checkpoint, start containers, or run Bash.

| Response | Client action |
|---|---|
| 409 `workspace_not_ready` | Wait for close/checkpoint completion; inspect state rather than launching another run |
| 404 `workspace_not_found` | Workspace disabled or never used; do not invent an archive |
| 402 | Resource-credit admission failed; surface allowance requirement |
| 200 | Stream to an explicit destination and verify captured archive metadata |

Preserve `Content-Length`, `X-Snapshot-Sha256`, generation, format version, logical bytes, and entry count. Validate length and SHA-256 before trusting content. The response is an immutable admitted generation even if the session later resumes. Use this authenticated download endpoint, not a fabricated raw R2 interface or public/presigned URL. Snapshot metadata is not an access capability. Each archive read can incur resource charges even without a model run.

Single-file selection is client-side after downloading the full checkpoint; workspace `project/result.txt` appears as `data/project/result.txt`. Bound decompression and archive metadata parsing; reject traversal, links, and unsafe destination paths. Do not automatically unpack archive-supplied paths over application files. A transcript export is independent of workspace closure and should not force it.

Canonical specification: [export contracts](https://github.com/kleedaisuki/moesegfault-agents/blob/main/docs/exports.md).
