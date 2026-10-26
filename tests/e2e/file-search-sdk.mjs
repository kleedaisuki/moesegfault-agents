// Actual deployed SDK journey. No fixture vectors, index seeding, or LLM calls.
// The authenticated operator runs this only against an approved staging profile.
import assert from 'node:assert/strict';
import { createHash, randomUUID } from 'node:crypto';
import { readFile, writeFile, mkdir } from 'node:fs/promises';
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
const seed = process.env.FILE_SEARCH_TRACE_SEED;
if (seed) assert.match(seed, /^[1-9][0-9]*:[1-9][0-9]*$/);
const trace = seed ? createHash('sha256').update(seed).digest('hex').slice(0, 32) : null;
const sdk = new OpenAI({ baseURL, apiKey: process.env.FILE_SEARCH_API_KEY, maxRetries: 0, timeout: 30000,
  fetch: (input, init = {}) => {
    const url = new URL(typeof input === 'string' ? input : input.url ?? input);
    assert.equal(url.origin, parsed.origin);
    assert.ok(!/\/(?:responses|completions|messages)(?:\/|$)/.test(url.pathname), 'Language-model routes are forbidden in this native retrieval journey');
    const headers = new Headers(input instanceof Request ? input.headers : undefined);
    new Headers(init.headers).forEach((value, name) => headers.set(name, value));
    if (trace) headers.set('traceparent', `00-${trace}-${randomUUID().replaceAll('-', '').slice(0, 16)}-00`);
    return fetch(input, { ...init, headers, redirect: 'error' });
  } });
const document = process.env.FILE_SEARCH_DOCUMENT ?? 'text';
assert.ok(['text', 'pdf'].includes(document), 'Choose only the approved text or checked-in native PDF input');
const isPdf = document === 'pdf';
const marker = `${isPdf ? 'native-pdf' : 'retrieval'}-${randomUUID()}`;
const text = `${marker}\nThe project recovery code is violet-orbit-742.\n检索来源是用户上传的文件，不是遥测。\n`;
const bytes = isPdf ? await readFile(new URL('../fixtures/retrieval/text-layer.pdf', import.meta.url)) : Buffer.from(text);
if (isPdf) assert.equal(bytes.length, 593, 'The bounded PDF probe uses only its checked-in text-layer fixture');
const expectedText = isPdf ? 'silver-lantern-513' : 'violet-orbit-742';
const operatorSource = process.env.FILE_SEARCH_OPERATOR_SOURCE ?? null;
if (operatorSource) assert.match(operatorSource, /^[a-f0-9]{40}$/);
const started = Date.now();
const report = { contract: isPdf ? 'file-search-native-pdf-text-v1' : 'file-search-upload-ingest-query-delete-v1', baseURL, marker,
  input_kind: document, input_bytes: bytes.length, operator_source: operatorSource,
  started_at: new Date(started).toISOString(), source: process.env.FILE_SEARCH_EXPECTED_BUILD ?? null,
  trace_id: trace, language_model_submissions: 0, status: 'started', phase: 'intent', pending: null, checks: [], ids: {}, cleanup_failures: [] };
const directory = path.resolve('.temp/file-search-journey');
const output = path.resolve(process.env.FILE_SEARCH_RECEIPT ?? path.join(directory, `${marker}.json`));
assert.ok(output.startsWith(path.resolve('.temp') + path.sep), 'Receipt must stay in this checkout temporary directory');
await mkdir(path.dirname(output), { recursive: true });
await writeFile(output, `${JSON.stringify(report, null, 2)}\n`, { flag: 'wx' });
const save = () => writeFile(output, `${JSON.stringify(report, null, 2)}\n`);
let source;
let store;
try {
  if (report.source) {
    const response = await fetch(parsed.origin + '/healthz', { redirect: 'error' });
    const health = await response.json();
    assert.equal(health.status, 'ok'); assert.equal(health.build_id, report.source, 'Serving Worker changed before native data work');
  }
  report.phase = 'upload_intent'; report.pending = { kind: 'source_upload', sha256: createHash('sha256').update(bytes).digest('hex') }; await save();
  source = await sdk.files.create({ file: await toFile(bytes, `${marker}.${isPdf ? 'pdf' : 'txt'}`, { type: isPdf ? 'application/pdf' : 'text/plain' }), purpose: 'assistants' });
  report.ids.file = source.id; report.pending = null; await save();
  report.phase = 'store_create_intent'; report.pending = { kind: 'store_create' }; await save();
  store = await sdk.vectorStores.create({ name: marker, metadata: { journey: marker } });
  report.ids.store = store.id; report.pending = null; await save();
  report.phase = 'attach_intent'; report.pending = { kind: 'native_ingest', store: store.id, file: source.id }; await save();
  const attached = await sdk.vectorStores.files.create(store.id, { file_id: source.id,
    attributes: { journey: marker, year: 2026 },
    chunking_strategy: { type: 'static', static: { max_chunk_size_tokens: 200, chunk_overlap_tokens: 50 } } });
  assert.equal(attached.id, source.id);
  assert.equal(attached.vector_store_id, store.id);
  report.pending = null; report.checks.push('owned_sdk_upload_and_attach'); await save();
  report.phase = 'native_visibility_wait'; await save();
  const deadline = Date.now() + 240000;
  let indexed;
  do {
    indexed = await sdk.vectorStores.files.retrieve(source.id, { vector_store_id: store.id });
    if (indexed.status === 'completed') break;
    assert.equal(indexed.status, 'in_progress', JSON.stringify(indexed.last_error));
    await new Promise(resolve => setTimeout(resolve, 2000));
  } while (Date.now() < deadline);
  assert.equal(indexed.status, 'completed', 'Background ingestion did not become query-visible within its bounded deadline');
  report.checks.push('background_ingestion_completed'); report.phase = 'native_query_intent'; report.pending = { kind: 'native_hybrid_query' }; await save();
  const result = await sdk.vectorStores.search(store.id, { query: isPdf ? expectedText : 'What is the project recovery code?',
    max_num_results: 5, filters: { type: 'eq', key: 'journey', value: marker } });
  assert.ok(result.data.some(hit => hit.file_id === source.id && hit.filename === source.filename &&
    hit.content.some(part => part.type === 'text' && part.text.includes(expectedText))));
  assert.ok(result.data.every(hit => hit.file_id === source.id));
  report.pending = null; report.checks.push(isPdf ? 'native_pdf_text_conversion_index_and_lookup' : 'actual_semantic_hybrid_query_with_source_identity'); await save();
  if (!isPdf) {
  const filtered = await sdk.vectorStores.search(store.id, { query: 'recovery code',
    filters: { type: 'eq', key: 'journey', value: 'not-this-owned-document' } });
  assert.deepEqual(filtered.data, []);
  report.checks.push('attributes_prefilter');
  }
  report.phase = 'source_delete'; await save();
  const deleted = await sdk.files.delete(source.id);
  assert.equal(deleted.deleted, true);
  source = undefined;
  if (!isPdf) {
  const after = await sdk.vectorStores.search(store.id, { query: 'project recovery code' });
  assert.deepEqual(after.data, [], 'A stale vector must not resurrect deleted source text');
  report.checks.push('source_delete_immediately_invalidates_results');
  } else { report.checks.push('new_pdf_source_delete'); }
  report.phase = 'store_delete'; await save();
  const removed = await sdk.vectorStores.delete(store.id);
  assert.equal(removed.deleted, true);
  store = undefined;
  report.checks.push('owned_store_delete');
  report.status = 'passed'; report.phase = 'completed'; await save();
} catch (error) {
  report.status = 'failed';
  report.error = { name: error?.name ?? 'Unknown', status: error?.status ?? null,
    code: typeof error?.code === 'string' && /^[A-Za-z0-9_]{1,96}$/.test(error.code) ? error.code : null };
  process.exitCode = 1;
} finally {
  if (store) await sdk.vectorStores.delete(store.id).catch(error => report.cleanup_failures.push({ kind: 'store', id: store.id, status: error.status ?? null }));
  if (source) await sdk.files.delete(source.id).catch(error => report.cleanup_failures.push({ kind: 'file', id: source.id, status: error.status ?? null }));
  if (report.cleanup_failures.length) { report.status = 'cleanup_pending'; process.exitCode = 1; }
  report.elapsed_ms = Date.now() - started;
  await save();
  console.log(JSON.stringify({ status: report.status, checks: report.checks, elapsed_ms: report.elapsed_ms }));
}
