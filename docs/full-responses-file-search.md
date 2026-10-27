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

## Staging native service profile

The scoped operator configuration pins `@cf/baai/bge-m3`, revision
`workers-ai-bge-m3-2026-10-26`, 1024 dimensions and an 8192-byte per-input cap.
Bindings are `AI` and `VECTORIZE`, with fixed namespace `agents-staging`.
The dedicated `moesegfault-agents-file-search-staging-v1` native index was created
with 1024 dimensions/cosine; actual subsequent metadata readback confirmed string
indexes for `owner_hash`, `store_id`, `file_id` and `chunk_id`, before any vectors
were inserted. Immediate post-create readback was empty while native index
mutations propagated; no creation was repeated. This is provisioning evidence,
not a real embedding/retrieval acceptance claim.

Snapshot `2026-10-26-platform-retrieval-v1` preserves existing numerical resource
rates and uses the same new price revision for all its tariffs. These four
**platform allocation** tariffs use the existing `worker` resource bucket:

| Unit | MicroUSD numerator / quantity denominator | Meaning |
| --- | --- | --- |
| `embedding_input_bytes` | 12 / 1000 | Actual submitted UTF-8 bytes; not estimated provider tokens or neurons |
| `queried_dimensions` | 1 / 100 | Actual query dimensions |
| `written_dimensions` | 1 / 100 | Actual native written dimensions |
| `allocated_vector_dimension_ms` | 50000 / 259200000000000000 | Reserved capacity over a fixed platform 30-day period; not a vendor per-ms invoice |

[Cloudflare's BGE-M3 model page](https://developers.cloudflare.com/workers-ai/models/bge-m3/)
describes the native embedding service and its separately token-priced vendor
rate; it does not authorize converting UTF-8 bytes into vendor token usage.
[Vectorize pricing](https://developers.cloudflare.com/vectorize/platform/pricing/)
lists queried/stored dimensions and account-level included allocations. The
operator snapshot above is explicit platform pricing, not an assertion of the
account's exact marginal Cloudflare invoice or unused free allowance.

Deployment selects `runtime_features=web_search,file_search` explicitly so the
previously accepted Search remains available without rerunning it. Code, images
and 4 GiB remain disabled. FileSearch-only also has an explicit allowlist value;
when FileSearch is unselected, its AI/Vectorize bindings and service profile are
removed from the limited deployment rather than leaving management routes
silently active. Production configuration is unchanged. Schema scope is 0017 and
0024; this profile does not claim 0025 document formats are deployed.

The provisioning receipt is `.temp/file-search-staging/provision.json` in the
clean integration checkout. It records zero embedding, vector-upload and
language-model data calls. The next acceptance is exactly the existing small
UTF-8 native SDK journey after the integration owner supplies a deployable green
source; diagnostic-only Wasm is not a promotion candidate.


## Native conversion MIME and diagnosis correction (2026-10-27)

The first actual staging PDF upload (593 bytes, text layer) reached native
conversion but failed before embedding. Its original attempt is not replayed.
The prior SDK integration created an untyped Blob. Upstream workerd serializes
`blob.type` to `files[].mimeType`, so an empty MIME is a real transport defect;
its successful response also uses `mimeType`, not only legacy `mimetype`.

The replacement uses the **same native AI binding's `fetch` method**, endpoint
and JSON envelope implemented by [workerd ToMarkdownService](https://github.com/cloudflare/workerd/blob/main/src/cloudflare/internal/to-markdown-api.ts).
It does not use global Fetch, an external API token, or an OCR/model fallback.
PDF and Office types are determined from bounded actual container bytes. Embedded
image conversion stays disabled. Legacy response spelling remains compatible,
but conflicting MIME spellings fail rather than overriding one another.

[The upstream wrapper](https://github.com/cloudflare/workerd/blob/main/src/cloudflare/internal/ai-api.ts)
throws `AiInternalError` after reading non-success JSON, discarding status and
headers. The native transport now preserves phase, actual HTTP status, request
ID, Cloudflare ray, content type/encoding, declared and received bytes, input and
response hashes, timing, body completeness, and a numeric upstream error code.
Partial body evidence is preserved on interrupted reads; a response is never
called complete merely because some bytes arrived. JSON parser line/column and
validation errors are separately observable. Extraction metadata is persisted
before decoding or publishing text, with original owner/source/generation and
`dispatched` status fences. Migration 0030 is additive; 0025 is unchanged.

Only bounded structured evidence is logged; provider messages, source document
text, authorization and response bodies are not copied into shared traces.
A nullable opaque `provider_receipt_id` reserves the link to separately encrypted
recovery bodies. A null ID does **not** mean a raw body was retained. Native free
text conversion does not fabricate model tokens from converter estimates.

Focused proof source: `8ef93ab` plus `04f81b8`. Local SQL owner/generation and
immutable first-receipt test passed (one test). Topic-only CI source `0a2a53f`
is intentionally non-promoting: it runs two new Rust contracts and one new
compiled-Worker rejected HTTP conversion, not the previously accepted UTF-8,
PDF/DOCX, chunking, embedding, search or batch-funding journeys. Actual repaired
Cloudflare PDF acceptance remains pending until the repaired Worker is deployed.


Focused proof completed successfully in [GHA 40446113827](https://github.com/kleedaisuki/moesegfault-agents/actions/runs/40446113827),
source `0a2a53fbee523462948e6d38678e19c314dd0a1a`. Retained Worker artifact
`12709051148`; verification artifact `12709120598`. Both new Rust contracts
executed exactly once (306 ms and 300 ms); the one compiled-Worker/SDK journey
passed in 3,954 ms. The fresh 593-byte PDF received a controlled HTTP 422 with
numeric error 10042, request ID `native-document-rejected-1`, and ray
`abc123-SIN`. Its immutable owner/generation receipt retained these fields;
repeated status and maintenance did not repeat conversion. Embedding/model calls
were zero. This proves the changed real Worker integration and error receipts,
not successful actual Cloudflare conversion; the latter remains a new staging
acceptance item.

Build-path timings explain this slice's elapsed time: `cargo install worker-build`
2m40s and Worker release build 4m36s; total Worker job 11m58s. Selected behavioral
checks consumed approximately 4.56 seconds. This is evidence for caching the
pinned build tool and compiled artifacts, not justification for more regressions.


## Alarm lifetime fixes the later paid-result receipt failure

The repaired native converter succeeded on a **new** staging input, not a replay
of the original failure. Worker `e4e677702f469a676022e40039cd84f9a0c097cb`,
assembly `302adceed320af8e7093100a8a15e9c0f860c779`, accepted a distinct 593-byte
PDF (SHA-256 `63e07f3f678fb9907cab5a121d13fc7fe856f112151d1c9bbe29f9ab393ee1e9`).
Native conversion completed with HTTP 200 in 98 ms, request 1,013 bytes and response
185 bytes. Its immutable 0030 metadata survived the subsequent index failure.
This is actual native conversion acceptance, not semantic search acceptance.

The later failure was **not** an upstream BGE failure or Vectorize visibility.
Readonly inspection of original generation `a85052b1-b90c-4b2b-943d-31e696a27870`
proved `embed:0:1` had observed 34 input bytes and settled exactly 1 microusd.
The parent turn `93e7ce2ed2ef433789312bdd3135fa21` retained a pending
`receipt:0:read` for vector persistence, then recorded a failed turn at 27,216 ms.
Earlier turns had similarly interrupted document-result and chunk-stage D1
acknowledgments. The scheduler selected its entire effect future against a
26-second timer and **dropped** the future at the deadline. The next claimant
saw the durable `embedding` fence and conservatively failed the job, but called
it `outcome_unknown` despite the known provider observation. The paid result was
lost between provider completion and durable publication. No repeated embedding,
reupload, direct SQL finance correction, or fabricated D1 row count was used.
The original File and Store were deleted through normal APIs; pending original
D1 observations retain their own unresolved financial evidence.

### Native execution lifetime, not a larger HTTP timeout

[Cloudflare's platform limits](https://developers.cloudflare.com/durable-objects/platform/limits/)
give HTTP `waitUntil` only 30 seconds after a response, but Durable Object alarm
handlers have a 15-minute wall-time lifetime. Removing only the local timer would
therefore leave a second platform cancellation trap. `FileIngest` uses the native
alarm lifetime instead:

- HTTP/cron kickoff stores only an immutable owner/store/file/generation key and
  schedules an alarm; no vectors, document content, or credentials enter actor
  snapshots. A hash of all four fields gives each generation its own actor.
- D1 remains authoritative for eligibility, source hash, generation, lease,
  funding and every external-effect fence. Claims also bind the expected actor
  generation; a stale actor cannot claim a successor's work.
- One alarm admits a bounded batch under an eight-minute **cooperative** budget.
  Once admitted, the provider result and its D1 receipt finish; a timeout never
  drops an in-flight receipt. Pending visibility schedules the next alarm.
- Actual actor wall time uses the existing `durable_object/wall_ms` tariff,
  not a duplicate outer `worker/wall_ms` charge. Cron and enqueue remain bounded
  scheduling overhead. The existing D1 lease/funding state machine is reused.
- Cold `embedding` fences with a known provider observation now report
  `file_search_embedding_receipt_unavailable`; genuinely unobserved dispatches
  retain `file_search_embedding_outcome_unknown`. Neither is re-embedded.

This is a small native scheduling boundary, not a new workflow framework.
[Durable execution research](https://www.usenix.org/conference/osdi20/presentation/zhang-haoran)
helps explain the distinction between saved control state and an acknowledged
external effect; it does not grant exactly-once semantics to opaque vendor HTTP.
The additive Wrangler migration is `v4-file-ingest-alarms` and the binding is
`FILE_INGEST`. No deployed D1 migration is rewritten.

Pure source `8185f9e`, fixture `55b776c`, and known-receipt classification `327f61f`
are the increment. The new controlled journey delays the D1 acknowledgment for
**33 seconds after vector rows are committed**, exercising the actual Rust alarm
without another real provider or language-model call. Existing retrieval fixture
bindings are updated mechanically, not an invitation to rerun accepted journeys.
Initial topic run 40450695349 failed in planning before compilation because an
unrelated inherited composed selection was not an ancestor. Replacement topic
run 40450872599 (`2f0658b`) isolates the finite two-check proof; its CI-only planner
must not be merged into deployable source. Proof outcome is pending here.

## File management accounting round trips (2026-10-27 increment)

A new actual Cloudflare PDF user journey on source `e4e6777` observed upload
5,952 ms, Vector Store creation 13,384 ms, attach 19,960 ms, and two membership
polls 9,757/9,895 ms. The native PDF conversion itself returned HTTP 200 in 98 ms.
That journey had no per-D1 management spans, so these timings do **not** establish
an internal latency ratio or blame conversion/embedding for the management delay.

Source inspection found the same nineteen binding round trips per measured D1
statement already established by the Conversation investigation. This increment
reuses its accepted `require_d1_batched` / `record_d1_batched` implementation for
Vector Store CRUD, attachment/membership queries, and the shared service-profile
read/pin path. No business SQL, source file bytes, background ingest state machine,
provider call, tariff, ledger key, account/Session binding or API contract changes.
Other FileSearch indexing/GC callers retain their existing paths. Space upload's
R2 publication is not claimed optimized by this D1-only change.

New `resource.d1.batch` events preserve operation/event/Session correlation,
funding/observation elapsed time and exact rows read/written. Those structural
fields are explicitly retained by telemetry; they are not discarded as arbitrary
content. Future Cloudflare diagnosis can separate actual SQL from accounting
without another native conversion or embedding call.

The focused new benchmark `tests/worker/file-management-perf.test.mjs` compares
actual compiled Workers, native D1/R2 and OpenAI SDK file/store management at a
fixed 20 ms request-D1 round-trip delay. It explicitly defers background ingestion
discovery on both sides: only queued attachment/observation/removal is in scope.
It must prove unchanged business SQL count, actual application D1 row quantities,
pinned exact D1 charges, zero remaining holds and zero external/model/embedding
calls. The already accepted Conversation benchmark and seven SQL tests are not
rerun. Controlled timing is not a real-Cloudflare latency SLO.
