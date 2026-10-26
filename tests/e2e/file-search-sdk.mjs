// Actual deployed SDK journey. No fixture vectors, index seeding, or LLM calls.
// The authenticated operator runs this only against an approved staging profile.
import assert from 'node:assert/strict';
import { randomUUID } from 'node:crypto';
import { writeFile, mkdir } from 'node:fs/promises';
import path from 'node:path';
import OpenAI, { toFile } from 'openai';

const baseURL = process.env.FILE_SEARCH_BASE_URL ?? 'https://agents-staging.moesegfault.dev/v1';
const parsed = new URL(baseURL);
assert.ok(parsed.origin === 'https://agents-staging.moesegfault.dev' ||
  (process.env.FILE_SEARCH_ALLOW_LOCAL === '1' && ['localhost', '127.0.0.1'].includes(parsed.hostname)),
'This journey never targets production');
assert.ok(!parsed.username && !parsed.password && !parsed.search && !parsed.hash && /^\/v1\/?$/.test(parsed.pathname),
  'Use a credential-free /v1 API URL');
assert.ok(process.env.FILE_SEARCH_API_KEY, 'Supply the authorized staging account key through FILE_SEARCH_API_KEY');
const sdk = new OpenAI({ baseURL, apiKey: process.env.FILE_SEARCH_API_KEY, maxRetries: 0, timeout: 30000 });
const marker = `retrieval-${randomUUID()}`;
const text = `${marker}\nThe project recovery code is violet-orbit-742.\n检索来源是用户上传的文件，不是遥测。\n`;
const started = Date.now();
const report = { contract: 'file-search-upload-ingest-query-delete-v1', baseURL, marker,
  started_at: new Date(started).toISOString(), checks: [], ids: {} };
let source;
let store;
try {
  source = await sdk.files.create({ file: await toFile(Buffer.from(text), `${marker}.txt`, { type: 'text/plain' }), purpose: 'assistants' });
  report.ids.file = source.id;
  store = await sdk.vectorStores.create({ name: marker, metadata: { journey: marker } });
  report.ids.store = store.id;
  const attached = await sdk.vectorStores.files.create(store.id, { file_id: source.id,
    attributes: { journey: marker, year: 2026 },
    chunking_strategy: { type: 'static', static: { max_chunk_size_tokens: 200, chunk_overlap_tokens: 50 } } });
  assert.equal(attached.id, source.id);
  assert.equal(attached.vector_store_id, store.id);
  report.checks.push('owned_sdk_upload_and_attach');
  const deadline = Date.now() + 240000;
  let indexed;
  do {
    indexed = await sdk.vectorStores.files.retrieve(source.id, { vector_store_id: store.id });
    if (indexed.status === 'completed') break;
    assert.equal(indexed.status, 'in_progress', JSON.stringify(indexed.last_error));
    await new Promise(resolve => setTimeout(resolve, 2000));
  } while (Date.now() < deadline);
  assert.equal(indexed.status, 'completed', 'Background ingestion did not become query-visible within its bounded deadline');
  report.checks.push('background_ingestion_completed');
  const result = await sdk.vectorStores.search(store.id, { query: 'What is the project recovery code?',
    max_num_results: 5, filters: { type: 'eq', key: 'journey', value: marker } });
  assert.ok(result.data.some(hit => hit.file_id === source.id && hit.filename === source.filename &&
    hit.content.some(part => part.type === 'text' && part.text.includes('violet-orbit-742'))));
  assert.ok(result.data.every(hit => hit.file_id === source.id));
  report.checks.push('actual_semantic_hybrid_query_with_source_identity');
  const filtered = await sdk.vectorStores.search(store.id, { query: 'recovery code',
    filters: { type: 'eq', key: 'journey', value: 'not-this-owned-document' } });
  assert.deepEqual(filtered.data, []);
  report.checks.push('attributes_prefilter');
  const deleted = await sdk.files.delete(source.id);
  assert.equal(deleted.deleted, true);
  source = undefined;
  const after = await sdk.vectorStores.search(store.id, { query: 'project recovery code' });
  assert.deepEqual(after.data, [], 'A stale vector must not resurrect deleted source text');
  report.checks.push('source_delete_immediately_invalidates_results');
  const removed = await sdk.vectorStores.delete(store.id);
  assert.equal(removed.deleted, true);
  store = undefined;
  report.checks.push('owned_store_delete');
  report.status = 'passed';
} catch (error) {
  report.status = 'failed';
  report.error = error instanceof Error ? error.message : String(error);
  throw error;
} finally {
  if (store) await sdk.vectorStores.delete(store.id).catch(() => {});
  if (source) await sdk.files.delete(source.id).catch(() => {});
  report.elapsed_ms = Date.now() - started;
  const directory = path.resolve('.temp/file-search-journey');
  await mkdir(directory, { recursive: true });
  await writeFile(path.join(directory, `${marker}.json`), `${JSON.stringify(report, null, 2)}\n`);
  console.log(JSON.stringify({ status: report.status, checks: report.checks, elapsed_ms: report.elapsed_ms }));
}
