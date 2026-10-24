# Conversations and workspace downloads

Use the same API origin and owned session as the task. These are private account data transfers, not publication. Conversation/tool text and workspace files can contain user secrets; export is not automatic redaction. Choose an authorized local destination and do not dump private contents into chat.

## Conversation journal

```sh
magents sessions export SESSION_ID --output conversation.json
magents sessions export SESSION_ID --format jsonl --output conversation.jsonl
magents sessions export SESSION_ID --format markdown --output conversation.md
```

Choose one format appropriate to the requested result. Exports preserve original source messages, tool calls/results, provider continuation fields, and separate pending steering; they are not merely a compacted prompt or UI summary. A read captures a point-in-time view: a running session may append later, and a later export can differ. It does not append input, start a run, compact history, or reset inactivity. Conversation export does not require closing a session.

## Committed workspace

```sh
magents sessions show SESSION_ID
# Only when the user intends to finish and close this work:
magents sessions close SESSION_ID
# After status/checkpoint readiness is confirmed:
magents sessions workspace download SESSION_ID --output workspace.tar.zst
magents sessions workspace file SESSION_ID src/main.rs --output main.rs
```

An archive requires an already closed session with a clean committed checkpoint. Active, closing, or checkpoint-pending/error states are not permission to return stale files. Inspect readiness and resolve persistence failure; do not repeatedly close or resume just to force a download. A disabled or never-used workspace has no archive. Download itself never starts a container, takes a new snapshot, resumes work, or resets inactivity. It reads the existing committed `tar.zst`, not ZIP or gzip. The read can incur R2 resource charges and credit admission without model or container execution.

The full archive contains workspace entries under `data/` and an internal manifest. Single-file selection transfers that same full compressed archive, so consider transfer cost for large checkpoints. Specify an exact normalized UTF-8 relative regular-file path such as `src/main.rs`, not `data/src/main.rs`. Absolute paths, parent traversal, non-normalized paths, and symlink/hardlink extraction are rejected. Single-file download is not recursive directory extraction.

## Destination and integrity guarantees

Named outputs never overwrite existing files or links; choose a new destination instead of deleting an existing result as a workaround. The CLI stages output privately beside the destination, verifies the transfer, and publishes only on success. Workspace reads verify compressed length/SHA-256; single-file selection also validates archive bounds and the final manifest's type/size/digest. Failed checks leave no partial target. Transfers have a five-minute total deadline and no automatic retry.

`--output -` explicitly selects stdout; binary workspace output is refused on an interactive terminal. Use it only for an intentional safe pipeline, not to expose archive bytes in tool logs. Downloading the archive does not automatically unpack it; keep any requested later extraction within the user's chosen destination and safe path boundary.

Full contracts: [CLI downloads](https://github.com/kleedaisuki/moesegfault-agents/blob/main/docs/cli.md), [export semantics](https://github.com/kleedaisuki/moesegfault-agents/blob/main/docs/exports.md).
