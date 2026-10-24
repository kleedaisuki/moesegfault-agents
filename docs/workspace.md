# Workspace daemon and checkpoint protocol

The session Durable Object is the control plane. It owns R2 credentials, provider
keys, workspace authorization, checkpoint publication, billing and idle
shutdown. The Linux container contains only the Rust daemon, developer tools and
one session's workspace. Neither the daemon nor arbitrary bash receives provider
keys, R2 credentials or an account token.

## Internal HTTP contract

Port 8080. Every endpoint, including health, requires `Authorization: Bearer
<MAGENTS_DAEMON_TOKEN>`. The token must contain at least 32 bytes and is compared
using constant-time digest comparison. It is an internal session-scoped secret,
not the user's API key. The daemon accepts only one execution/checkpoint/restore
at a time; conflicts return 409. Request bodies except restore are capped at 32 KiB.

| Method and path | Contract |
| --- | --- |
| `GET /health` | `{ "status": "ok", "boot_id": "<uuid>" }`; boot identity changes on process restart. |
| `POST /v1/exec` | `{ "id": "run-1", "command": "git status", "cwd": null, "timeout_ms": 120000, "max_output_bytes": 1048576 }`. |
| `GET /v1/exec/{id}` | Final result, 202 `{id,status:"running"}`, or 404. |
| `DELETE /v1/exec/{id}` | Explicit cancellation; 204 when signaled, otherwise 404. |
| `GET /v1/status` | `{ "running": false, "dirty_bytes": 1234 }`. |
| `GET /v1/snapshot` | Known-length streamed `application/zstd` body and checkpoint headers below. |
| `POST /v1/restore` | Streamed archive body, required `X-Snapshot-Sha256`; returns checkpoint metadata. Invalid archives return 422 without changing the current generation. |
| `POST /v1/checkpoint-ack` | `{ "sha256": "..." }`; 204 after matching the latest generated snapshot. Call only after durable R2 publication. |
| `PUT /v1/files/{id}` | Stream a Space attachment into `uploads/{id}/content`. Requires `Content-Length` (up to 64 MiB) and `X-Content-Sha256`; returns `{file_id,path,bytes,sha256}`. Identical content is idempotent; conflicts never overwrite user files. |

Execution results contain `id`, nullable `exit_code`, UTF-8-lossy `stdout` and
`stderr`, `timed_out`, `cancelled`, `truncated` and `duration_ms`. The combined
stdout/stderr retention budget is bounded while both pipes continue draining;
binary data remains in workspace files rather than a JSON tool result. Timeout
is 1–600000 ms; output budget is 1–4194304 bytes. Command text is capped at 24 KiB.
`cwd` is relative to the current workspace generation and opened through a
directory capability; absolute/parent components and escaping links are rejected.

Attachment imports use the same workspace gate and UID quiescence as checkpoint
operations. The daemon first streams into a protected temporary file and verifies
the exact byte count and compressed-independent file digest. It then publishes a
capability-relative, no-clobber hard link from that protected temporary file,
fsyncs the directory, and updates dirty tracking. File IDs contain only bounded
ASCII letters, digits, underscores and hyphens; user filenames are metadata, not
paths. Existing symlinks in the destination chain are rejected. A client
disconnect or invalid body never publishes an incomplete file. The capability
and no-clobber primitives are provided by [cap-std](https://docs.rs/cap-std/4.0.3/cap_std/fs/struct.Dir.html),
not a string-prefix filesystem sandbox.

Accepted executions run independently of the HTTP connection. Reconnect using
the same execution ID and GET status rather than retrying the POST. The last 128
accepted IDs and successful terminal results are held in memory. This is not a
durable exactly-once protocol: the DO must persist tool-call intent/result and
must not automatically rerun an ambiguous side-effecting tool call after restart.

Snapshot headers are `Content-Length`, `X-Snapshot-Sha256`,
`X-Snapshot-Logical-Bytes`, `X-Snapshot-Archive-Bytes`, `X-Snapshot-Entries` and
`X-Snapshot-Format-Version: 1`. SHA-256 covers the exact compressed body. Restore
returns `{format_version, sha256, logical_bytes, archive_bytes, entries}`.

`dirty_bytes` is intentionally conservative, not a filesystem write counter.
After each execution a single bounded, sorted metadata scan fingerprints the
root and relative entries: type, size, permissions, Linux device/inode,
ctime/mtime with nanoseconds, and raw symlink targets. It never reads file
contents; directory metadata captures even empty-file add/delete churn. Linux
ctime cannot be restored by unprivileged bash, so resetting mtime does not hide
a same-size write. An unchanged fingerprint adds **zero** dirty bytes, including
pure reads of large repositories. A changed fingerprint adds the tree's logical
size (at least one byte, so deletions count). Scan failures mark 512 MiB dirty.
This supports a 64 MiB batch trigger without filesystem watchers or per-command
remote I/O. A nonempty existing workspace is conservatively dirty at daemon
startup; successful restore establishes a clean metadata baseline.
A checkpoint acknowledgment clears this estimate only if its exact SHA and
workspace revision still match. A late acknowledgment never discards newer
mutations. A boot ID prevents restoring an old R2 checkpoint over a still-running
container when only its DO was evicted.

## Data ownership and failure protocol

`MAGENTS_STATE_DIR` defaults to `/var/lib/magents`:

```
CURRENT                         root-owned UUID pointer
generations/<uuid>/             current user workspace (UID/GID 1000)
generations/.restore-*/          private, incomplete restoration
archives/                       root-only temporary checkpoint files
```

Creation walks capability-relative handles and writes a bounded tar stream into
zstd level 3. Network I/O is one packed archive upload, not one request per file.
The archive stores paths below `data/` and a final `.magents-manifest.json` with
per-entry length, SHA-256, type and raw filename/target bytes encoded as hex.
Non-UTF-8 Linux filenames survive round trips. File modes are masked to 0777;
setuid bits, archive ownership and device metadata are never restored.

Restoration copies the body to a private bounded file, checks its compressed
digest before extraction, then checks every entry against the final manifest.
Absolute/parent paths, duplicate entries, unsafe links, foreign-owned hardlinks and device
files are rejected, never silently dropped. Extraction happens only in a new
private generation, not into a live workspace. Files/directories are fsynced,
the new generation is renamed into place, and a fsynced temporary CURRENT file
is renamed over the pointer followed by directory fsync. Interrupted restores
leave CURRENT pointing at the previous generation; orphan private directories
are not canonical and can be garbage-collected. Old generations must not be
treated as user-visible history and can be cleaned after a successful commit.

Relative symlinks contained inside the workspace are preserved without
dereferencing. Links are created only after all regular entries and the manifest
are verified. The full link graph is resolved with a 40-hop cap; cycles, chained
escapes and entries below symlink parents are rejected. Absolute symlinks,
including virtual-environment links to system Python, are currently unsupported
and cause a visible failed checkpoint. Recreate such environments after restore
or use a relative contained layout; do not claim persistence succeeded and then
release the container. Hardlinked regular files owned by the dedicated workspace
UID 1000 are materialized as individually bounded regular-file copies. Contents
and permissions persist, but hardlink identity is not preserved: editing one
restored file does not mutate another. This supports ordinary package/build
caches without letting the root archiver read a linked inode owned by root or
another UID. The ownership policy is checked against opened file metadata before
reading; root-/foreign-owned hardlinked inodes are explicitly rejected. Production
snapshots require Unix ownership metadata; Windows tests are trusted portable
format/algorithm smoke tests, not an alternative runtime security boundary.

Named pipes (FIFOs) preserve their type and permissions as zero-payload tar
entries. The daemon never opens or reads a FIFO while scanning, archiving,
restoring or fsyncing; only its containing directory needs fsync. Unix domain
sockets are ephemeral IPC endpoints, not persistent user file contents, and are
explicitly omitted. Their owning processes are already terminated by quiescence;
applications recreate sockets when started. Character/block devices remain
unsupported. File and directory modification times persist at integral-second
GNU tar precision, applied through no-follow opened descriptors after children
are complete. Nanoseconds and symlink/FIFO timestamps are not preserved in this
format. Pre-epoch mtimes fall back to the epoch; oversized source times clamp to
year 9999 and untrusted larger archive times are rejected without publication.

Archive limits are 100000 entries, 128 path components, 512 MiB expanded content,
16 MiB manifest, and 256 MiB HTTP compressed input. The expanded tar stream is
also bounded. A streaming raw-tar guard validates header framing/checksums before
the tar parser allocates extension payloads: GNU longname/longlink headers are
capped at 64 KiB, PAX/sparse entries are rejected, and raw header count is bounded.
The zstd decoder's window is capped at 8 MiB. These are deployment safeguards, not
user context-window or session limits. Checkpoint creation uses the exact same
256 MiB compressed writer bound as restoration input; an incompressible workspace
cannot produce a successfully published but unrestorable checkpoint.

The DO publishes an immutable R2 archive **before** replacing the session's R2
`manifest.json`. That single manifest write is the current bridge's durable
commit point and restoration authority; the DO's saved snapshot metadata is a
cache of it, not a separately authoritative pointer. The DO then updates its
metadata and acknowledges the checkpoint. It stops the container only after that
durable commit. Upload failure leaves the live container and previous R2 manifest
intact; retry uses a
new immutable candidate rather than overwriting a good checkpoint. R2 orphan
candidates and abandoned local generations are reclaimable intermediate state,
not telemetry or canonical user data. The daemon unlinks a generated archive as
soon as its streaming file descriptor is opened, including on client disconnect.

## Execution isolation

The daemon is a minimal root supervisor. Every bash process drops to dedicated
UID/GID 1000, clears supplementary groups and environment, sets `no_new_privs`,
starts its own process group, and receives process/file/output/time limits.
Only safe PATH, HOME, LANG and TERM values are supplied. Bash uses
`--noprofile --norc`; no daemon token is inherited. A shell is not a security
boundary by itself: the tenant boundary is the dedicated Cloudflare container.

After each execution and before any checkpoint/restore, the daemon kills the
entire process group and all remaining processes of the dedicated UID using
Linux pidfds, including descendants that escaped with `setsid()`. This prevents
background mutation during checkpointing and avoids PID-reuse signaling races.
Tini reaps orphans. Long-lived workspace background services are deliberately
not supported by this first protocol. **Never run the daemon with sudo directly
on a shared development/CI host**: its UID cleanup is container-wide. Use the
Docker image. Platform container memory/CPU/disk quotas remain necessary to
bound malicious workloads; rlimits and output retention are not a complete
sandbox or resource-accounting solution.

### Process-exit race handling

The release run [40065762512](https://github.com/kleedaisuki/moesegfault-agents/actions/runs/40065762512)
exposed an intermittent HTTP 500 in the detached-`setsid` journey after earlier
green runs. The container remained healthy and later tests passed; the old
sanitized logs did not retain the underlying errno. Inspection found a concrete
gap: `/proc/<pid>/status` used `read_to_string` and ignored only `NotFound`, while
a task exiting **after** its status file is opened produces `ESRCH`, not `ENOENT`.

A deterministic, unprivileged Linux probe reproduced the exact lifecycle using
only its own `/bin/sleep` child: open status and pidfd, kill and reap the child,
then read the already-open status handle. On WSL Linux 6.18 the results were
`status read: ESRCH (3)`, `status reopen: ENOENT (2)`, and
`pidfd signal after reap: ESRCH (3)`. This agrees with the
[kernel procfs lifetime contract](https://www.kernel.org/doc/html/latest/filesystems/proc.html).
The native regression repeats this sequence without running UID-wide cleanup on
the shared CI host. It verifies that `ESRCH` is not Rust's `ErrorKind::NotFound`.

Per-process discovery, status reads and pidfd signaling now share one exit
classification: only `ENOENT`/`ESRCH` mean the task is already gone. Permission,
descriptor/resource, unsupported-kernel and unknown errors remain fatal; a
global `/proc` enumeration failure is not treated as a clean workspace. Only a
successfully signaled task counts as needing a follow-up cleanup pass. Kernel
UID/state fields are parsed as ASCII bytes rather than decoding untrusted process
names as UTF-8. Zombie/dead states cannot write and are excluded.

`workspace.cleanup_race` and `workspace.cleanup_error` events contain only numeric
`errno` and a fixed stage from `pidfd_open`, `process_status`, `pidfd_signal`, and
`quiesce_exhausted`. They never contain process names, commands, paths, user output
or the full error chain. Container acceptance retains the original detached
writer assertion and adds 120 independent short-lived/`setsid` executions, each
requiring HTTP 200 without retries, followed by an assertion that no delayed
writer created a file. At exact revision
`8caff37333cf148a62c804e75cc9189203e8f167`,
[CI 40067168672](https://github.com/kleedaisuki/moesegfault-agents/actions/runs/40067168672)
passed all 33 native daemon tests (including the real stale-status-FD regression)
and all 16 real container HTTP cases, including the 120-execution stress case.
The current fixture sanitizer does not yet export the new cleanup event fields;
their content-free logging contract is distinct from that untested export path.

Request logs contain execution IDs, safe endpoint/method/status, durations,
traceparent and generated request IDs, but never commands, tool output, file
names, archive bodies or credentials. The DO owns distributed trace propagation
and export. Workspace state and manifests are user data; temporary archives and
unreferenced generations are intermediate state; structured spans are telemetry.

## Engineering references and verification

- [Cloudflare container lifecycle](https://developers.cloudflare.com/containers/concepts/architecture/): disk is ephemeral; persistence must precede shutdown. Native snapshots are image-version coupled and currently have implicit retention, so portable R2 workspace archives remain the durable application contract.
- [tar-rs security](https://docs.rs/tar/latest/tar/#security) and [builder symlink handling](https://docs.rs/tar/latest/tar/struct.Builder.html): extraction into mutable live trees and default dereferencing are unsafe. This implementation performs explicit private extraction and capability-relative reads instead of `Archive::unpack`.
- [cap-std filesystem API](https://docs.rs/cap-std/latest/cap_std/fs/index.html): capability-based I/O contains path traversal and raced directory changes.
- [Tokio subprocess behavior](https://docs.rs/tokio/latest/tokio/process/struct.Command.html): dropping a child does not normally terminate it; tracked tasks, explicit cleanup and `kill_on_drop` cover separate ownership paths.
- [Finding Crash-Consistency Bugs with Bounded Black-Box Crash Testing, OSDI 2018](https://www.usenix.org/conference/osdi18/presentation/mohan): small operation sequences around fsync expose real persistence failures; CURRENT publication keeps the crash boundary small and directly testable.

Portable tests cover size/entry limits, manifest verification, corruption,
rollback, ordinary round trips, decompression allocation attacks, metadata dirty
detection and read-only dirty-state invariance. Linux-only tests cover raw
filenames, links and restoring mtime without hiding a ctime mutation. The daemon's
real HTTP user journey must run inside the image in GitHub
Actions, exercising execution, cancellation, output limits, environment
isolation, checkpoint upload/download, restart, restoration and tamper rollback.
