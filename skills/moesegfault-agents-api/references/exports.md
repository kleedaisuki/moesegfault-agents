# Transcripts and workspace artifacts

Use the same trusted-backend, owner-scoped platform bearer authentication as session reads. These are private transfers, not public artifact links. All paths below are relative to `https://agents.moesegfault.dev`, not an SDK base already containing `/v1`.

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
