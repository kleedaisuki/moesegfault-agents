# Agent configuration and session exports

These are account-owned data transfers, not a public marketplace. Publishing a
TOML agent through `magents` creates or replaces an executable configuration in
the signed-in account using the ordinary owner-scoped agent API. Downloaded TOML
contains editable configuration fields, not server IDs/revisions or API keys.
Provider credential references remain account-local; transferring a configuration
does not transfer its secret. See [CLI](cli.md) for the versioned TOML schema.

## Conversation export

`GET /v1/sessions/{id}/export?format=json|jsonl|markdown` is an authenticated read.
The default is JSON; unknown or repeated query fields return 400. Missing sessions
and another account's sessions both return 404. API keys have the same owner-read
authority as existing session reads. Cookie authentication remains origin-bound.

JSON has `schema_version=1`, `exported_at_ms`, the pinned `session` record,
`context={input_tokens,compact_count}`, source/pending counts, `messages` and
`pending_messages`. `messages` is the append-only source journal, **not** the
compacted model prompt. Tool call/result IDs, arguments, outputs and provider
continuation fields survive unchanged. Pending steering input is separate, never
misrepresented as already processed. JSONL begins with the same metadata under
`type=session`; subsequent records are `{type:message,sequence,message}` or
`{type:pending_message,sequence,message}`, each using its own zero-based sequence.
Markdown includes the metadata and complete structured messages in safely fenced
JSON blocks. These are fidelity-preserving exports, not automatic redaction of
secrets that a user may have intentionally placed inside their own conversation.
Application provider-key envelopes and authentication tokens are not exported.

The DO captures one point-in-time journal/pending snapshot without awaiting I/O.
Serialization then streams one complete message per chunk, rather than buffering
another complete JSON/Markdown document. The journal vector is cloned once for
snapshot isolation; the active inference context is not cloned for export. A run
can continue appending after this captured high-water mark. A new export captures
that later state. Export never starts a run, advances the user-input clock,
compacts history or modifies the journal. Message/count headers describe the
captured export, not subsequent appends.

Responses use attachment filenames and `Cache-Control: no-store`. Their types
are `application/json`, `application/x-ndjson` and `text/markdown`, respectively;
`X-Export-Message-Count` and `X-Export-Pending-Count` describe the two source lists.

## Workspace archive and file downloads

`GET /v1/sessions/{id}/workspace/archive` returns the complete committed **tar.zst**
checkpoint, not a ZIP, gzip stream or archive metadata response. It accepts no
query fields. The session must already be Closed with archival/checkpoint work
complete. An active, closing or checkpoint-error session returns 409
`workspace_not_ready`: finish the task and explicitly close it first. A disabled
or never-used workspace has no archive and returns 404 `workspace_not_found`.
There is no implicit close, Bash execution, container start, snapshot creation,
workspace mutation or inactivity-clock update in a download.

The owner is checked before forwarding to its DO. The DO requires its committed
workspace record to match the private stored manifest's format, generation,
timestamp, digest, size and entry count. The object path must be the exact
owner/session-derived prefix, digest and canonical UUID archive name. No request
can supply an R2 object key, and no public/presigned bucket URL is returned. A
missing/inconsistent manifest/object is 409, not a successful stale export.

R2's native readable stream is handed to the response directly: no Worker/Wasm
archive buffering, decompression or re-encoding. The response carries
`Content-Type: application/zstd`, `Content-Length`, `X-Snapshot-Sha256`,
`X-Snapshot-Generation`, `X-Snapshot-Format-Version`,
`X-Snapshot-Logical-Bytes` and `X-Snapshot-Entries`. Compressed archives are at most
256 MiB. The headers describe a captured immutable generation; later explicit
resume does not change the bytes of that admitted download.

Each archive GET uses a fresh `download:<uuid>` reservation independent of model
run/concurrency admission. It observes and settles one R2 Class B operation before
returning the native body; disconnecting the client does not undo an already
performed R2 read. Its admission refreshes the verified allowance window before
reservation, including when API-key authentication has not refreshed it and the
closed checkpoint was created in an earlier funding period. It does not carry
expired credit forward or issue duplicate grants on repeated downloads.
No model cost or container lease is created. Authentication,
DO metadata and ledger-management overhead follow the existing bounded
management-plane policy. Insufficient available resource credit returns 402.
An ambiguous operation or failed financial settlement retains its scoped
reservation for reconciliation, never pretending that unknown work was free or
occupying a model-run slot.

`magents` can save the complete archive or select an individual regular file from
it. Selection is client-side: a file such as `project/result.txt` is stored as
`data/project/result.txt` inside the archive. This avoids restarting a released
container or inflating large zstd data inside a Worker merely to download one
file. The trade-off is transferring the complete compressed checkpoint for a
single file. The CLI verifies the archive's compressed length/SHA-256 before
trusting its entries, bounds decompression and raw extension headers before the
tar parser can allocate, and never extracts links or writes archive-supplied
destination paths. Output is an explicit local file, atomically created without
clobbering an existing file. Full directory extraction is deliberately not an
implicit part of download.

## Verification and references

Focused native tests cover JSON/JSONL source round-tripping with arbitrary text,
pending/continuation preservation, Markdown fencing, filename and query
validation, closed-checkpoint readiness and owner/generation/object-key fences.
The compiled-Worker fixture checks all three transcript formats against an
actually executed tool journal, ownership/authentication and unchanged clocks,
status and wallet. Real staging checkpoint download and CLI file readback are
separate release acceptance gates; pure tests do not establish that R2 streaming
or native file publication succeeded.

- [Cloudflare R2 Workers usage](https://developers.cloudflare.com/r2/api/workers/workers-api-usage/)
  demonstrates authorization before private object download.
- [R2 Workers reference](https://developers.cloudflare.com/r2/api/workers/workers-api-reference/)
  exposes object bodies as native ReadableStreams.
- [tar entry documentation](https://docs.rs/tar/latest/tar/struct.Entry.html)
  describes parser-resolved paths and GNU/PAX handling; check paths after parsing
  while bounding extension framing before parsing can allocate.
- [Daemon snapshot implementation](../crates/workspace-daemon/src/snapshot.rs)
  is the authoritative existing format and decompression-boundary reference.
