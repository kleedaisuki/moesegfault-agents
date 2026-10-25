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
index configuration. Root must provision compatible AI/Vectorize bindings and
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

## Proposed persistence schema

No migration number has been allocated yet. The following tables are the intended
shared contract; root coordinates DDL after the already reserved `0016` blob work.

| Table | Required state |
| --- | --- |
| `vector_stores` | globally opaque ID, owner, name/metadata, created/active timestamps, expiry policy, deleting tombstone |
| `vector_store_files` | owner/store/file membership, immutable source SHA, attributes, chunking profile, in-progress/completed/cancelled/failed status, safe error, ingest generation/lease |
| `file_search_chunks` | owner/store/file/SHA/generation, ordered chunk index/text, byte range, vector ID, embedding model/dimension/version |
| `file_search_fts` | mature D1 FTS5 external-content index over published chunk text |

All identity relationships need same-owner constraints, not only a globally
unique foreign file/store ID. Content publication is guarded by the current lease
and source's still-processed SHA. A cancelled or superseded handler cannot publish
a later generation. Failed derived data is disposable, while its source file and
financial provenance are not.

## Ingestion, cancellation and deletion

Attaching a file creates recoverable `in_progress` membership. A bounded scheduled
pass claims a D1 lease, reads the owner-checked R2 object through existing metered
Space helpers, verifies length/digest, extracts bounded text, chunks, embeds and
indexes. Root alone adds the scheduler/router hooks. No detached HTTP `waitUntil`
is relied upon to finish a large indexing operation.

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
Creating an index or querying it never admits model execution or starts a user
workspace. These are metered resource operations.

## Chunking and formats

The first vertical slice ingests bounded plain UTF-8/structured text, retaining
document order and stable chunk identity. Auto chunking is server-defined and
can use documented byte-safe bounds. Native static token-size/overlap semantics
need a real, versioned tokenizer such as the standard cl100k retrieval tokenizer;
they must not silently reinterpret characters as model tokens. The embedding
model's own input guard and billing tokenization remain separate concerns.

PDF/office formats require bounded mature extractors or a trusted extraction
service. The indexer must not execute uploaded code, macros or package installers.
Unsupported/encrypted/corrupt formats yield explicit safe ingestion errors rather
than an empty successful index. Decompression and aggregate extracted-text limits
are enforced before embedding.

## Costs and acceptance

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
DeepSeek-only; no live embedding calls are authorized by this design note.

## Primary references

- [OpenAI file search](https://developers.openai.com/api/docs/guides/tools-file-search)
- [OpenAI retrieval](https://developers.openai.com/api/docs/guides/retrieval)
- [Vector Store search contract](https://developers.openai.com/api/reference/resources/vector_stores/methods/search)
- [D1 supported FTS5](https://developers.cloudflare.com/d1/sql-api/sql-statements/)
- [Vectorize metadata filtering](https://developers.cloudflare.com/vectorize/reference/metadata-filtering/)
- [Vectorize client API](https://developers.cloudflare.com/vectorize/reference/client-api/)
- [BGE-M3 model](https://developers.cloudflare.com/workers-ai/models/bge-m3/)
