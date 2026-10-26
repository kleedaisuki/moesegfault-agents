# Platform file search and owned Vector Stores

Active design/implementation contract, not acceptance. The user requires a
platform-default `file_search` service over owned Space data. An upstream BYOK
search service alone does not satisfy this requirement.

## Retrieval and authority

Space R2 objects and immutable file digests remain the source. D1 owns Vector
Store metadata, membership/attributes, ingest leases, chunk text and lexical FTS5
indexes. Cloudflare Vectorize owns the derived semantic index; Workers AI supplies
real embeddings. No fabricated vector or character-based token charge is allowed.

The proposed default semantic profile is the documented multilingual BGE-M3
model, with exact model ID/version and dimensions pinned in each ingest job and
index configuration. The deployment owner provisions compatible AI/Vectorize bindings and
declare resource tariffs before claiming default semantic acceptance. Until then,
lexical fixtures prove only lexical retrieval, not semantic search.

Query preparation first resolves **all requested stores to the authenticated
owner**. Attribute predicates and live membership produce eligible file IDs.
Vectorize queries use a fixed namespace with mandatory indexed `owner_hash`,
`store_id` and eligible `file_id` filters; this avoids depending on the service's
1,000-namespace ceiling. Vector IDs include store/file/content/chunk identity.
Results are still rechecked against D1 owner, active store/membership, processed
Space file and exact source digest before returning any text. Delayed vector
deletion must never resurrect deleted data.

Native metadata indexes are created before inserting vectors. Filtering eligible
IDs before semantic ranking may require bounded query batches; metadata filters
must not be simulated only by dropping foreign/nonmatching top results afterward.
The final owner/current-file check is defense against asynchronous index lag,
not a substitute for pre-filtering.

## Native contracts

`file_search.rs` contains the shared tool options, bounded typed comparison/
compound filters, safe bound SQL compilation, result projections and weighted
reciprocal-rank fusion. SQL attribute names are values compared through
`json_each`, never interpolated identifiers or hand-escaped JSON paths.

Supported native tool fields are `vector_store_ids`, `max_num_results`, `filters`
and `ranking_options`. The official search ranker enum currently includes `none`,
`auto` and `default-2024-11-15`; `none` disables a later reranking stage, not source
authorization. A proprietary upstream ranker version must not be silently
advertised as the same model when the local implementation uses another ranker.
Auto uses the configured platform policy and reports meaningful real rank scores.
Hybrid weights actually control lexical/semantic rank contributions; zero-weight
channels do not insert zero-score matches. Scores are normalized fusion ranks,
not mislabeled BM25 or cosine probabilities.

The hosted result uses `file_search_call` and its native query/status/result
fields. Direct Vector Store search projects content as text parts. Citation
references use the actual owned file ID and name; an outer model's final text
must be anchored independently, never by copying unrelated answer offsets.

## Persistence schema

Root allocated `migrations/0017_file_search.sql`. It adds the following owner-scoped
derived state, with same-owner insert constraints and immutable source identities.
This source change does not itself establish that staging has applied the migration.

| Table | Required state |
| --- | --- |
| `vector_stores` | globally opaque ID, owner, name/metadata, created/active timestamps, expiry policy, deleting tombstone |
| `vector_store_files` | owner/store/file membership, immutable source SHA, attributes, chunking profile, in-progress/completed/cancelled/failed status, safe error, ingest generation/lease |
| `file_search_chunks` | owner/store/file/SHA/generation, ordered chunk index/text, byte range, vector ID, embedding model/dimension/version |
| `file_search_fts` | mature D1 FTS5 external-content index over published chunk text |
| `vector_store_file_batches` | owned batch identity, creation time and cancellation |

All identity relationships need same-owner constraints, not only a globally
unique foreign file/store ID. Content publication is guarded by the current lease
and source's still-processed SHA. A cancelled or superseded handler cannot publish
a later generation. Failed derived data is disposable, while its source file and
financial provenance are not.

## Ingestion, cancellation and deletion

Attaching a file creates recoverable `in_progress` membership. The HTTP response
defers an owner/store/file-scoped kickoff through the real request context; status
GETs may wake that already-admitted task, never create a new indexing intention.
Each kickoff examines at most four due jobs under a 26-second work deadline.
Active D1 leases prevent competing polls from repeating the same batch. The
existing 15-minute maintenance pass is recovery, not the normal initial latency.
An indexing turn reads the owner-checked R2 object through existing metered Space
helpers, verifies length/digest, extracts bounded text, chunks, embeds and indexes.
No detached HTTP task is relied upon to finish a large indexing operation.

Vectorize upserts are asynchronously visible. Do not call a file completed merely
because its mutation request returned: visibility/expected generation must be
confirmed, or the job remains in progress for reconciliation. Only published
matching generations are searchable. A crash resumes derived work by stable
file/hash/generation identity, not another user upload or a fabricated Session.

Deleting a Vector Store membership does not delete the Space file. Deleting its
Space file excludes it immediately through live joins and cancels derived ingest;
scheduled cleanup removes stale chunks/vectors. No fake response attachment pin
is created for a vector job—the existing pin reconciler is response-specific.

The intended OpenAI-compatible management surface covers stores, attached files,
their status/attributes/content, file batches/cancellation, and direct search.
Store expiry uses its own `last_active_at`, never the Session's user-append clock.
Creating an index or querying it never admits an agent language-model run or starts
a user workspace. Embedding inference is a separately metered retrieval resource.

## Chunking and formats

UTF-8 text is the first executable slice (source and extracted text currently
bounded to 16 MiB). Auto (800/400) and native static chunking use `text-splitter`
0.33.1 with a fallible custom sizer over the existing official DeepSeek plain-text
tokenizer. The vocabulary SHA is
`b64f58e1ec67577c2fc4b6b93689b12da288a974e46c088bad52ae6e5defe5f8`.
The synchronous borrowing interface reuses an already-warm Flash/Pro vocabulary;
no cloned vocabulary or tokenizer borrow survives across asynchronous I/O.
Chunker/vocabulary revision is durable per membership. These are explicitly
retrieval tokens, not BGE or billable language-model tokens. Provider input-byte
guards remain independent and reject oversized chunks rather than truncate.

PDF/office formats require bounded mature extractors or a trusted extraction
service. The indexer must not execute uploaded code, macros or package installers.
Unsupported/encrypted/corrupt formats yield explicit safe ingestion errors rather
than an empty successful index. Decompression and aggregate extracted-text limits
are enforced before embedding.

## Costs and acceptance

The implemented entry points are `file_search_routes::handle` (authenticated HTTP),
`file_index_scheduler::{run,kickoff}` (bounded recovery/request wakeup), and `file_ingest::search_queries`
(the shared harness query). Narrow registration patches go directly to the
integration owner, without another approval chain.
HTTP resource admission reuses `ConversationOperation`, rather than introducing
another budget ledger. Ingestion uses the durable generation-scoped
`file-ingest:<generation>` resource reservation before R2/AI/Vectorize effects;
each actual retry has distinct attempt observation keys. Missing tariffs/bindings
fail closed. Unknown observations retain the hold instead of fabricating usage.

Each store pins its non-secret service profile on first attachment; memberships
retain their generation's copy. The scheduler does not silently
replace a queued generation's embedding model or dimension/revision with a later
default. Sealed source chunks avoid repeated R2 reads; visible batches avoid
repeated embedding. Native batch attach is a D1 transaction with whole-batch owner
validation, not a loop that can expose partial cross-owner ingestion. Logical
detachment is separate from ingest cancellation. Expired/deleted/current-source
gates are applied again after slow semantic queries and multi-query fusion.

`tests/test_file_index_sql.py` exercises actual source SQL with SQLite/FTS5:
owner/deletion/generation/profile isolation, immutable identities, FTS deletion,
expiry/detach, safe reattachment, atomic mixed-owner batches, and lease/visibility publication fences.
These tests do not substitute for a real Worker upload/ingest/query/citation/delete
journey. HTTP cursor/batch surface, actual configured bindings, final harness
citations, common document conversion, and index storage cleanup/accounting still
require end-to-end acceptance; source code is not evidence that provisioning is live.

`tests/e2e/file-search-sdk.mjs` is the isolated deployed SDK journey: it uploads
synthetic text, requests static token chunking, polls the real background index,
queries source-identified evidence and attributes, deletes the source, verifies
immediate invalidation, and deletes the store. It does not seed indexes, fabricate
vectors, invoke an agent LLM, or call a hidden cron endpoint. An authorized operator
supplies `FILE_SEARCH_API_KEY`; the URL is restricted to staging (or explicitly
enabled loopback). It writes a bounded, credential-free receipt in `.temp/`.

Workers AI's documented native embedding response contains shape/vector data but
does not guarantee usage tokens. Root must choose exact provider usage/matching
tokenization or an explicitly reviewed resource allocation unit; no invented
tokens. Vector dimensions and query/upsert counts are known measurements. New
AI/Vectorize resource kinds/units are coordinated with the financial owner, not
inserted as unmetered side effects or observations on settled model reservations.

Acceptance must include actual owned-store ingestion/search, Unicode filters,
lexical/semantic differentiation, real hybrid weighting, stale Vectorize results
after file deletion, cancelled/stale lease non-publication, source digest checks,
cross-owner/mixed-store rejection, native SDK shapes, bounded format errors and
conserved resource holds. Pure fixture vectors prove rank math, not live model
availability or semantic quality. Paid language-model tests remain direct
DeepSeek-only. The active task authorizes small, purposeful real Workers
AI/Vectorize staging journeys; this note does not claim those have passed.

## Recoverable indexing and deletion increment

Derived chunks carry `pending → embedding → embedded → upserting → submitted →
visible` phases. Actual embedding vectors and upsert receipts survive visibility
delays. Unknown embeddings fail explicitly rather than purchasing another call.
Ambiguous upserts are observed by stable, generation-bound IDs and saved vectors;
actual query visibility does not require inventing a vendor mutation ID.

Closed metadata-filter batches of sixteen IDs reduce 32-vector visibility from
32 queries to two. No turn sleeps for 31 seconds inside a 26-second work budget:
an invisible generation releases its lease and resumes through ordinary status
polling or maintenance without re-embedding or repeating a known upsert.

Migration 0017 queues retirement on source deletion, store deletion/expiry,
membership removal and generation replacement. Cleanup uses the original pinned
profile. D1 text/FTS rows are removed after native deletion and `getByIds`
confirmation. An ambiguous upload that has not appeared retains its identity,
instead of discarding the only record of a potentially late vector. Derived
cleanup never deletes an independent Space file.

`session_hosted_file_search.rs` retains owned evidence before canonical
publication under a separate resource-only funding identity. Publication retries
do not query/embed again. Exact file markers produce `file_citation` annotations
only when present in the actual answer and backed by completed owned results.
Text deltas are unchanged; Unicode character positions are not UTF-8 byte offsets.

`tests/worker/file-search.test.mjs` covers actual Rust management, asynchronous
ingestion, delayed visibility, owner isolation, harness search/citation,
current-SDK original-wire validation, replay and source deletion. Its two-
dimensional vectors are controlled transports, not real semantic-quality proof.

Native boundary invariants are executable, not fixture normalization. With
serde_json arbitrary precision enabled, generic serde-wasm-bindgen serialization
of a JSON Value can expose private Number structs instead of JavaScript
numbers. Vector service arguments therefore pass through the native JSON
parser. Numeric vector entries and topK remain numeric at the actual binding.

Public file_search_call objects are distinct from private model tool results.
The public item retains structured results; the private continuation carries
serialized JSON text, matching the existing Code/Search output-content contract.
An object-valued tool message must not reach native text/part translation.

## Allocated vector capacity clock

The independent 0024 increment uses `worker/allocated_vector_dimension_ms`:
platform reserved index capacity, not vendor tokens, active CPU, or Cloudflare's
millisecond invoice. Allocation is the sealed generation's exact
`chunk_count × embedding_dimensions`; confirmed physical cleanup closes it.

The public-rate conversion is 50,000 microUSD per 100 million dimensions per
platform 30-day period: numerator `50000`, denominator `259200000000000000`.
Account-wide included allowances are not represented as individual free calls.
Queried/written dimensions remain separate from the storage-capacity charge.

Each clock pins its tariff and journals its interval before funding/ledger
append. Retry settles the same identity before advancing time. Fractional units
carry between observations: whole tariff quanta are charged while open and the
last remainder rounds only once on closure, not every fifteen minutes. Long
intervals split at the signed-ledger limit without losing units. Debt retains its
pending interval; unknown quantities never become zero. Source and SQL checks
are not compiled/live acceptance of this independent clock increment.

## Bounded document extraction

The optional operator `conversion_allocation` enables native Workers AI
`toMarkdown` for byte-identified PDF, DOCX and XLSX, not extension-based
routing. It uses the documented document parser path; image inputs and empty
text are rejected, with no image captioning/OCR or generative fallback. A
controlled transport fixture is not proof of real document-parser output.

Rich input is capped at four MiB and converted UTF-8 text at one MiB. Office
archives are inspected with the mature ZIP parser after a bounded end-record
check: at most 2048 entries, one-MiB central directory, sixteen-MiB advertised
expansion, no encryption, ZIP64, symlinks, traversal, macros or unsupported
codecs. Canonical internal filenames follow the detected container. Native
output must report the expected MIME type and text format.

Migration 0025 is disposable derived text, independent of source Files and
financial evidence. Dispatch is persisted before the native call; successful
text is reused after restart. An unknown result is never automatically
purchased again. Current owner, digest, generation and lease fence dispatch
and result publication. Cleanup removes the cache only after its generation's
chunks have actually disappeared.

The operator tariff `worker/conversion_input_bytes` bills a declared platform
conversion allocation, never the converter's estimated `tokens` field or
purported actual model tokens. Returned input bytes are recorded once;
unknown service outcomes retain their observation instead of inventing zero.

Chunk staging retains only UTF-8 byte ranges for the full source and owns text
for one 32-chunk D1 batch at a time. This removes the additional overlapping
full-source copy beside the warm DeepSeek tokenizer. The existing tokenizer,
semantic splitter, exact offsets and provider byte cap remain unchanged. A
local RAII admission guard allows one active ingestion buffer per isolate;
durable leases and ordinary polling/maintenance still provide cross-isolate
recovery and horizontal concurrency. Cancellation releases admission directly.

## Primary references

- [OpenAI file search](https://developers.openai.com/api/docs/guides/tools-file-search)
- [OpenAI retrieval](https://developers.openai.com/api/docs/guides/retrieval)
- [Vector Store search contract](https://developers.openai.com/api/reference/resources/vector_stores/methods/search)
- [D1 supported FTS5](https://developers.cloudflare.com/d1/sql-api/sql-statements/)
- [Vectorize metadata filtering](https://developers.cloudflare.com/vectorize/reference/metadata-filtering/)
- [Vectorize client API](https://developers.cloudflare.com/vectorize/reference/client-api/)
- [Vectorize public pricing](https://developers.cloudflare.com/vectorize/platform/pricing/)
- [BGE-M3 model](https://developers.cloudflare.com/workers-ai/models/bge-m3/)
- [Markdown conversion binding](https://developers.cloudflare.com/workers-ai/features/markdown-conversion/usage/binding/)
- [Document conversion behavior](https://developers.cloudflare.com/workers-ai/features/markdown-conversion/how-it-works/)
- [Conversion options](https://developers.cloudflare.com/workers-ai/features/markdown-conversion/conversion-options/)
- [Fallible upstream text splitter](https://github.com/benbrandt/text-splitter/tree/v0.33.1)
