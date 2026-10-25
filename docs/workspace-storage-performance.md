# Workspace archive I/O and metadata cost

## Workload and decision

The workspace daemon serializes quiescent trees into a bounded tar/zstd archive.
Dirty batches and explicit persistence operations use this path; historical
Responses workspace anchors will also need exact immutable snapshots. The
important costs here are compressed-archive disk traffic and large manifest
allocation, not model latency or a different digest algorithm.

At the accepted `6552a93` source, `SnapshotStore::create` writes the complete
compressed archive, flushes/fsyncs it, then opens and reads that file again only
to calculate SHA-256. It also deserializes its just-serialized manifest only to
obtain the number of entries. Both repeated passes are avoidable without changing
the archive format, checksum algorithm, metadata, publication fence, or ownership.

The implementation now hashes only the bytes successfully accepted by the
compressed output writer. Zstd finalization precedes digest finalization; flush,
file fsync, atomic temporary-file publication and archive-directory fsync retain
their previous order. Entry count comes from the same immutable BTreeMap before
serialization, instead of parsing that JSON into another map.

| Create-stage work | Previous | New |
| --- | --- | --- |
| Compressed archive output | `A` bytes written | Same `A` bytes written |
| Separate checksum pass | `A` archive bytes read | No archive read |
| SHA-256 input | All compressed bytes | The same successfully written bytes |
| Derive entry count | Parse the complete manifest JSON and allocate its entries | BTreeMap length |
| Packing/compression, logical file hashing, HTTP/R2 transfer | Unchanged | Unchanged |
| R2 retained bytes and API tariff | Unchanged | Unchanged |

This removes exactly one compressed-archive read per newly created checkpoint.
It is not an incremental filesystem, a claim of lower retained R2 bytes, or a
claim that all checkpoint latency falls by 50%. Read-only tools already avoid
creating checkpoints; that earlier behavior is not a new optimization here.

## Integrity and compatibility

The digest writer respects `std::io::Write` partial-write semantics: a failed
write contributes no bytes, and a short write contributes only its accepted
prefix. It never hashes an attempted buffer as though the sink accepted it all.
The compressed limit remains enforced by the existing limited writer.

The native archive stream is independently checked by R2's existing SHA-256
verification before the checkpoint pointer publishes. A daemon file altered
between generation and transfer must therefore fail publication. No R2 key,
manifest schema, snapshot format, source path, version identity or restore
behavior changes.

Restore deliberately retains its private-copy checksum pass **before** extraction.
Hashing only its incoming source while copying would not verify the private file
that extraction later reads. Replacing that barrier just to save I/O would change
the integrity contract; it was rejected. Likewise, changing to a faster unrelated
digest or introducing a new content-addressing/reference subsystem is unnecessary
for these two local costs.

## Reproduction and evidence boundary

The daemon's ordinary native tests now include:

```sh
cargo test --locked -p workspace-daemon snapshot::tests::compressed_digest_tracks
cargo test --locked -p workspace-daemon snapshot::tests::archive_digest_includes
cargo test --locked -p workspace-daemon snapshot::tests::checkpoint_digest_and_manifest_cost_receipt
```

The first test covers all byte values, different input segment sizes, forced
short writes and injected failures. The second independently reads generated
archives back to check the published digest and restores the original bytes,
including empty input and a multi-buffer binary file. Existing corruption,
archive-limit, manifest, traversal, symlink and atomic-restore tests remain.

The workload receipt is `.cache/perf/v3/snapshot-io.json`. It compares five
alternating baseline/streaming samples of the **same** real compressed checkpoint
bytes, with 128 KiB writes and a file fsync in both paths. Baseline read bytes are
counted by the actual checksum reader; the streaming path never opens the file
for hashing. The corpus is deterministic 8 MiB binary data. The cache is warm
from archive preparation/writes, intentionally making the old readback path an
optimistic baseline. A separate 10,000-entry manifest comparison measures parsing
versus direct length. The receipt records build mode and CI source SHA.

Timings are an archive-write/fsync/digest-stage experiment on the runner's native
filesystem, excluding packing, compression, HTTP/R2 transfer, model calls and
container startup. They are not a production latency SLO or an estimate of
physical device I/O after page-cache effects. No elapsed-time win is asserted in
the test: checksum equivalence and removed read bytes are deterministic gates;
timing samples remain available for equivalent comparison.

### First native CI measurement

[CI 40186260916](https://github.com/kleedaisuki/moesegfault-agents/actions/runs/40186260916)
tested exact source `1c2620161419b6ad7f6c1aeb55db689315202c59`.
The native Rust and real-container jobs completed successfully. All 42 daemon
tests passed, including the three new checksum/manifest cases; the native job's
`performance-native-v3` artifact retains `snapshot-io.json`. This is not a full
release, a production rollout, or acceptance of unrelated pending API work.

| Metric | Old readback path | Streaming digest path |
| --- | ---: | ---: |
| Same compressed archive | 8,389,560 bytes | 8,389,560 bytes |
| Checksum file reads, five samples | 41,947,800 bytes | 0 |
| Write/fsync/digest median | 178.134 ms | 177.985 ms |
| Write/fsync/digest mean | 175.473 ms | 176.749 ms |

These five warm-cache **debug-build** timing samples are mixed; they do not
demonstrate a latency improvement or a stable regression. The accepted mechanism
is eliminating one whole archive read, not claiming a favorable median as a
system-level speedup. Packing, compression and transfer remain outside this table.

For the 10,000-entry, 1,160,025-byte manifest, the debug parse median was 21.078 ms;
the direct-count median was 60 ns. The useful result is removing that parse and
entry allocation from production, not treating the nanosecond sample as a fleet
latency forecast. The implementation adds no checksum algorithm change or retained
R2-storage saving. A release-mode/end-to-end checkpoint benchmark would be needed
before stating an overall latency benefit.

No local Rust rebuild, paid model call, deployment, or restart of the live staging
baseline was used for this optimization. A subsequent test-only, thread-local
counter now guards the actual `create` call against reintroducing `hash_file`:
its independently read-back archive checksum remains a separate assertion.
That additional guard is queued for the next full-feature checkpoint and was
not part of the `1c262016` CI measurement above.

## Shared physical archive foundation (pending full-feature CI)

The next checkpoint removes the separate physical copy made when the first
immutable user version is published. This is the same owner-scoped blob/reference
model required by historical Responses branches, not a second filesystem CAS.
New checkpoints stream directly into a UUID-qualified `workspace-blobs/` object,
verify the compressed SHA-256 and size, and register one account-owned physical
storage clock. Current-session, immutable-version and response-boundary references
then share that object. Content reuse is scoped to owner, format, digest and
compressed byte length; a concurrent duplicate upload remains a recoverable
candidate until collection, rather than pretending the extra physical bytes never
existed.

| New content path | Before | New path | Evidence scope |
| --- | --- | --- | --- |
| Current checkpoint plus first saved version | Two archives, `2A` bytes | One archive, `A` bytes | Source/SQL mechanism; real checkpoint acceptance pending |
| First saved-version publication after upload | Extra archive GET + PUT (+ destination HEAD) | Reference/metadata only | Source/SQL mechanism; no latency claim |
| Historical child import | No compatible archived-state fork | Trusted DO seed + current reference, no archive copy | Public Worker journey queued |
| Further versions of the same content | Shared saved-version archive | Same physical owner archive | SQL quota/idempotency tests |

`A` is the compressed archive size; manifests and D1 metadata are additional.
The table is a deterministic operation/representation comparison, not measured
production savings. The first native CI measurement above predates this feature.
The forthcoming `.cache/perf/v3/workspace-blobs.json` intentionally distinguishes
actual Miniflare public fork/download/GC measurements from seeded uploads and an
analytical `2A` baseline. Its binary stand-in does not exercise native tar/zstd
restore. A real Worker+Container staging checkpoint/commit is still required for
upload-path acceptance; a seeded fixture alone cannot prove that path.

### Lifetime, compatibility and charging

- `0016_workspace_blobs.sql` owns the single registry. A candidate is journaled
  before R2 PUT. Only a verified complete archive becomes ready. D1 triggers fence
  reference insertion against an atomic `deleting` claim.
- Session publication holds both old and new strong references before changing
  the R2 pointer. Only after the R2 pointer and DO manifest commit are stale
  session-current references released. Failed publication retains recoverable
  data, and later successful publication reconciles those references.
- Branch import happens before financial run admission, so it performs **no R2
  I/O**. It stores a typed, owner-validated DO seed and strong current reference.
  First actual workspace use publishes/restores that seed under an admitted run
  or compute lease. Descendant sessions do not depend on their parent staying
  alive or on the parent response remaining publicly visible.
- Public Response deletion releases its own archive reference only after its DO
  tombstone is durable. Other responses, descendants and user versions retain
  independent references. User versions and their permanent references cannot be
  deleted or repointed. No user-data archive expires just because it is old.
- Old user versions and their exact bytes stay untouched. Legacy session archives
  reuse an already protected, published same-content version where possible,
  including its original physical clock; this lazy registration performs no R2
  archive transfer or clock re-registration. Otherwise session archives
  are copied at most once into the protected namespace before sharing: simply
  registering their old keys would not protect them from a rolling old GC. A
  promoted live manifest gets a new generation so its generation-qualified
  physical manifest clock cannot conflict with the previous object size.
- Physical R2 retained-byte billing keeps one clock per object key, with nullable
  session ownership for new shared owner objects. Adopted legacy version objects
  preserve their existing session-owned clock without changing its ownership.
  Extra references do not create new
  physical byte charges. Space's byte-capacity admission now includes hidden
  session/response archives, images, files and upload candidates, counted once per
  owner/physical key. This closes a hidden-history quota bypass: `usage.bytes`
  may be higher than a saved-version-only view. `version_bytes` remains saved
  versions only; file/version counts retain their established meaning. This is
  quota enforcement, not a change to the R2 byte-ms tariff.
- Collection first scans at most 64 indexed due rows, then atomically claims
  expired **unreferenced** objects. Referenced rows are deferred so a large
  permanent history does not turn each cron into a full scan. Collection repairs
  a missing physical clock via HEAD, deletes R2 bytes, closes the clock, then
  removes registry metadata. Failures retain retry state; never-materialized
  candidates do not invent retained-byte charges.

Cheap validation uses production SQL extracted from Rust, all migrations and
SQLite fault-boundary tests. It covers quota boundaries, owner fencing, dedup
races, reference-vs-delete ordering, failed-delete retries, and unique clocks.
These checks are complementary to, not replacements for, the queued real Worker
journey and real-container upload acceptance. No local heavy Rust build or live
model invocation was used.

The initial workspace-only pre-CI integration passed 120 SQL tests. After generic
blob/capacity integration, `python -m unittest discover -s tests -p
'test_*sql.py' -q` passed **146 tests**, including 14 immutable-version cases,
8 shared-blob GC/recovery cases and 6 legacy-checkpoint compatibility cases.
Rustfmt checks passed for the owned Rust modules and `node --check` passed for
both new fixture modules. An independent review found a stale-reference leak
after R2-pointer commit/cold recovery; recovery now reconciles only after pending
version restore finishes, with production-SQL fault-boundary regression coverage.
These are uncommitted working-tree checks, not Rust compilation or a GHA result.

### Generic image objects and capacity query measurement

The same engine also owns immutable generated image bytes. `OwnedArchiveRef`
retains strict workspace format-1/UUID/owner/digest validation; `OwnedBlobRef`
requires image kind, its separate owner namespace, PNG/JPEG/WebP MIME and a
16 MiB **raw** payload bound (roughly 22 MiB base64). There is no user-supplied raw
JSON escape hatch into a trusted reference. The internal `archive_bytes` column
is the physical payload size for both kinds; kind-specific constraints prevent
format confusion. There is one candidate/readiness/reference/deleting lifecycle,
not another image CAS.

Images can reserve byte capacity before a paid provider request, when their final
digest is not yet known. A capacity hold is virtual quota, never an invented R2
storage bill. A transactional candidate insert replaces that hold with its actual
byte size. Known failure releases it; an abandoned hold expires through bounded
maintenance. Temporary fork-transfer references similarly bridge source export
and child-current publication with a renewable 24-hour lease; permanent response,
version and current-session user data have no age TTL.

Space image Files are metadata links to that same immutable object. File deletion
releases its own reference but never directly deletes a response's image bytes.
The native image result is independent of that optional Files listing: if the
file-count limit races after generation, the checked result/strong blob remains
usable, `file_id` is omitted until a real link exists, and a bounded
`file_link_pending` diagnostic records deferred publication. Retry reuses the
known result rather than paying for generation again; explicit Response/File
tombstones must not be resurrected. Byte capacity is reserved before generation,
whereas optional file metadata does not require another file-slot hold.
Deleted metadata retains an immutable provenance key, not a foreign-key lifetime
hold that would block final collection. Active linked metadata fences collection
even if publication crashes before its explicit reference bind. Stale file refs
are reconciled in bounded indexed batches against explicit metadata tombstones,
not guessed expired user data.

An inexpensive query-plan probe found that grouping the UNION capacity view
before applying the owner filter made SQLite scan **all owners**. The shipped
shape exposes raw UNION rows, applies the owner predicate inside each source, then
groups each physical key with `MAX(bytes)` and sums. The local warm-cache probe
used Python 3.14.6 / SQLite 3.50.4, 250 owners × 16 blobs (4,000 total), and five
alternating samples with identical results for the target's 16 rows / 57,144 bytes:

| Capacity query | Median | Mean | Plan |
| --- | ---: | ---: | --- |
| Rejected grouped-view baseline | 1,137,200 ns | 1,189,500 ns | Global scan |
| Owner-filter-before-group | 47,000 ns | 55,600 ns | Covering owner index search |

The receipt is `.cache/perf/v3/space-capacity-query.json`; local source SHA is null
because this is uncommitted work. The ordinary native SQL CI suite regenerates
the receipt with `GITHUB_SHA`. Equality and indexed query shape are deterministic
test gates; elapsed time is not a threshold or a Cloudflare D1/end-to-end SLO.
This final pre-CI local receipt followed the complete 146-test SQL check. Earlier
local repeats had different wall times under concurrent runner load; indexed
owner selection and equal results, not one favorable timing sample, are the
accepted mechanism. This measurement concerns metadata admission/usage queries, not R2 transfer or the
still-pending real-container `2A`→`A` upload acceptance.

## Grounding

- Rust's [Write contract](https://doc.rust-lang.org/std/io/trait.Write.html)
  defines accepted-prefix and error semantics; the adapter composes with it.
- Zstd's [streaming writer](https://docs.rs/zstd/latest/zstd/stream/write/struct.Encoder.html)
  requires explicit finalization, which must complete before the digest is read.
- [FIVER](https://arxiv.org/abs/1811.01161) studies overlapping integrity hashing
  with data transfer, but also distinguishes write-side corruption from shared
  input I/O. That distinction is why restore's private-file check stays intact.
- [GogetaFS, FAST 2025](https://www.usenix.org/conference/fast25/presentation/pan)
  identifies duplicate metadata maintenance as a material I/O-path cost. Its
  system-specific performance numbers are not borrowed here; the relevant lesson
  is to remove an unnecessary representation/pass before adding dedup machinery.
- Cloudflare's [R2 Workers API](https://developers.cloudflare.com/r2/api/workers/workers-api-reference/)
  supplies native stream bodies and checksum-bearing PUTs; the design retains
  those platform paths rather than materializing archives in WASM. Its
  [consistency contract](https://developers.cloudflare.com/r2/reference/consistency/)
  makes completed direct-binding writes/deletes visible immediately, while D1
  reference claims still provide the cross-object lifetime fence R2 alone lacks.
