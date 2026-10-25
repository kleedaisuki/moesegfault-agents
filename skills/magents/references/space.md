# Space files and workspace versions

Each account has one private Space across the CLI, dashboard and API. Use
`magents space --help` and the appropriate subgroup help. Select the intended API
origin consistently. Upload, deletion, commit and restore are external actions;
match them to the user's request rather than treating discoverability as consent.

## Manage user files

```sh
magents space show
magents space rename Research
magents space files upload ./brief.txt --content-type text/plain
magents space files upload ./screenshot.png --content-type image/png
magents space files list --limit 20
magents space files list --limit 20 --after FILE_ID
magents space files show FILE_ID
magents space files rename FILE_ID 'Project brief.txt'
magents space files download FILE_ID --output downloaded.txt
magents space files delete FILE_ID
```

Follow `last_id` only when `has_more` is true; do not fetch every file for a single
lookup. Upload accepts a regular file up to 64 MiB, also subject to the server's
plan/funding limits. `--name` selects its stored UTF-8 filename; it does not move or
rewrite local files. The native transfer is streamed with length/SHA-256 receipts,
not base64 JSON or a full memory copy. Upload failure may mean the reply was lost:
inspect the Space metadata and digest before deciding to upload again.

Raw upload defaults to `application/octet-stream`, not a filename-derived media
type. For an image used in a Responses `input_image` part or local-function
result, pass its actual supported type, for example `--content-type image/png`.
A `.png` name or later rename does not establish the file's media type.

Downloads require an explicit new local destination and never overwrite files or
links. Output is published only after complete length/checksum verification.
`--output -` intentionally selects binary stdout and is refused on an interactive
terminal. No server filename chooses the local output path. Transfers have bounded
deadlines and are not automatically retried.

Use an uploaded ID in a Responses `input_file` content part, alongside an
`input_text` message, through `magents responses create FILE`. Ownership and file
interpretation limits remain server responsibilities; arbitrary binary upload
does not mean every model can read its format. See [Responses](responses.md).

## Persist, discover and restore workspaces

```sh
magents sessions workspace commit SESSION_ID --name 'Before migration'
magents space versions list --limit 20
magents space versions show VERSION_ID
magents space versions download VERSION_ID --output workspace.tar.zst
magents sessions workspace restore SESSION_ID VERSION_ID
magents sessions workspace restore SESSION_ID VERSION_ID --operation-id OPERATION_ID
```

Commit/restore generate a random operation UUID once and print it before dispatch;
successful JSON also includes `operation_id`. Save it with the exact action and
arguments. On an uncertain reply, inspect state first and, if explicit recovery
is appropriate, reuse `--operation-id` for that identical action. A new ID means
new work. Never reuse one for another version/name/action or blindly retry with
fresh IDs. IDs are 1–128 ASCII letters, digits, `_` or `-`. The transmitted bodies
are `{"operation_id":"...","name":"..."}` (name optional) and
`{"operation_id":"...","version_id":"..."}` respectively.

Commit persists the workspace as an immutable account-wide version. Versions have
no user rename, overwrite or delete operation. Download verifies the compressed
archive receipt without extracting or starting a workspace. Do not invent a raw
storage URL or try to update a version using ordinary file APIs.

Commit and restore require an eligible idle workspace. Restore preserves the
current workspace as a safety version, then replaces its files from the selected
owned version in the same session without changing its conversation history/pin.
Historical Response branching is a separate candidate interface, not a reason to
restore or rewind the source session. Do not repeatedly retry a failed restore or emulate
one with destructive shell commands. Inspect session/version state first.
Persistence, transfers and compute remain subject to resource billing and limits.

For the existing closed-session archive and individual-file selector, read
[exports](exports.md). For lifecycle recovery, read [sessions](sessions.md).

Full contract: [CLI Space](https://github.com/kleedaisuki/moesegfault-agents/blob/main/docs/cli.md#space-and-files).
