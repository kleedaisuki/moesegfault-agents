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

At implementation handoff, formatting/diff checks pass. No local Rust rebuild,
new paid model call, deployment or timing acceptance is claimed. Root must run
the exact-source native/container CI and retain its receipt before accepting the
optimization. The live staging baseline must not be restarted for this work.

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
