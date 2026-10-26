import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { mkdir, mkdtemp, readFile, readdir, writeFile } from 'node:fs/promises';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import OpenAI, { toFile } from 'openai';
import { Miniflare, convertV4MiniflareOptions } from 'miniflare';
import { createBrowserPrincipal } from './browser-principal.mjs';
import { createIdentityFixture } from './auth-journey.mjs';
import { dispatchSDKFetch } from './sdk-miniflare-fetch.mjs';

const root = fileURLToPath(new URL('../../', import.meta.url));
const build = path.resolve(root, process.env.WORKER_BUILD_DIR ?? 'crates/agents-worker/build');
const unit = 'allocated_vector_dimension_ms';

test('compiled Rust Worker: vector capacity carries units until confirmed native cleanup', { timeout: 45000 }, async t => {
  const encryptionKey = randomBytes(32), identity = createIdentityFixture();
  const deployment = JSON.parse(await readFile(path.join(root, 'wrangler.jsonc'), 'utf8'));
  const pricing = JSON.parse(deployment.env.staging.vars.RESOURCE_PRICING_JSON);
  for (const [unit, denominator] of [['embedding_input_bytes', 64], ['queried_dimensions', 100], ['written_dimensions', 100], ['allocated_vector_dimension_ms', 1000]]) {
    pricing.tariffs.push({ resource: 'worker', unit, microusd_numerator: 1, quantity_denominator: denominator, price_version: pricing.version });
  }
  const profile = { embedding: { model: '@cf/baai/bge-m3', revision: 'controlled-capacity-v1', dimensions: 2, input_byte_limit: 32768 },
    ai_binding: 'AI', vector_binding: 'VECTORIZE', namespace: 'capacity-fixture',
    embedding_allocation: { resource: 'worker', unit: 'embedding_input_bytes' }, query_allocation: { resource: 'worker', unit: 'queried_dimensions' }, upsert_allocation: { resource: 'worker', unit: 'written_dimensions' } };
  const vectors = new Map();
  let embeddings = 0, deletes = 0;
  await mkdir(path.join(root, '.temp'), { recursive: true });
  const directory = await mkdtemp(path.join(root, '.temp/vector-capacity-'));
  const mf = new Miniflare(convertV4MiniflareOptions({ modulesRoot: build,
    modules: [{ type: 'ESModule', path: path.join(build, 'worker/shim.mjs'), contents: await readFile(new URL('./file-search-shim.mjs', import.meta.url), 'utf8') },
      { type: 'ESModule', path: path.join(build, 'index.js') }, { type: 'CompiledWasm', path: path.join(build, 'index_bg.wasm') }],
    compatibilityDate: '2026-10-23', compatibilityFlags: ['enable_request_signal'], resourcePersistencePath: path.join(directory, 'storage'),
    d1Databases: ['DB'], r2Buckets: ['WORKSPACES', 'CONVERSATIONS', 'DIAGNOSTICS'],
    durableObjects: { SESSIONS: { className: 'Session', useSQLite: true }, AUTH_SESSIONS: { className: 'AuthSession', useSQLite: true },
      CONVERSATION_STATE: { className: 'ConversationState', useSQLite: true }, MODEL_OPERATIONS: { className: 'ModelOperation', useSQLite: true } },
    ratelimits: Object.fromEntries(['INGRESS', 'ACCOUNT', 'MUTATION'].map((name, i) => [`${name}_RATE_LIMITER`, { namespace_id: String(i + 1), simple: { limit: 1000, period: 60 } }])),
    bindings: { ENVIRONMENT: 'test', APP_ORIGIN: 'https://agents.test', ALLOWED_ORIGIN: 'https://agents.test', DATA_ENCRYPTION_KEY: encryptionKey.toString('base64url'),
      IDENTITY_ISSUER: 'https://identity.test', IDENTITY_WEB_CLIENT_ID: 'test-dashboard', IDENTITY_NATIVE_CLIENT_ID: 'test-native', IDENTITY_REDIRECT_URI: 'https://agents.test/auth/callback',
      IDENTITY_NATIVE_REDIRECT_URI: 'http://127.0.0.1/callback', IDENTITY_CLIENT_PRIVATE_JWK: JSON.stringify(identity.privateJwk), BILLING_PRODUCT_ID: 'agents', BILLING_MODE: 'test',
      MODEL_REGISTRY_JSON: '[]', MODEL_PRICING_JSON: '{}', RESOURCE_PRICING_JSON: JSON.stringify(pricing), FILE_SEARCH_PROFILE_JSON: JSON.stringify(profile),
      RUN_RESOURCE_RESERVATION_MICROUSD: '20000', FREE_GRANT_BUDGET_MICROUSD_PER_DAY: '100000000' },
    async outboundService(request) {
      const url = new URL(request.url);
      assert.equal(url.origin, 'https://retrieval.fixture.test', 'No model or external service is permitted');
      const args = await request.json();
      switch (url.pathname) {
        case '/embed': embeddings++; return Response.json({ shape: [args[1].text.length, 2], data: args[1].text.map(() => [1, 0]) });
        case '/upsert': for (const vector of args[0]) vectors.set(vector.id, vector); return Response.json({ mutationId: 'known-upsert' });
        case '/query': return Response.json({ matches: [...vectors.values()].filter(record =>
          Object.entries(args[1].filter).every(([key, filter]) => '$eq' in filter ? record.metadata[key] === filter.$eq : filter.$in.includes(record.metadata[key])))
          .slice(0, args[1].topK).map(record => ({ id: record.id, score: 1 })) });
        case '/getByIds': return Response.json(args[0].map(id => vectors.get(id)).filter(Boolean));
        case '/deleteByIds': for (const id of args[0]) if (vectors.delete(id)) deletes++; return Response.json({ mutationId: 'known-delete' });
        case '/describe': return Response.json({ dimensions: 2, vectorCount: vectors.size });
        default: throw Error('unexpected_retrieval_effect');
      }
    },
  }));
  t.after(() => mf.dispose());
  const db = await mf.getD1Database('DB');
  for (const name of (await readdir(path.join(root, 'migrations'))).filter(name => name.endsWith('.sql')).sort()) {
    const parsed = spawnSync(process.env.PYTHON ?? 'python', ['-c', 'import sys,sqlite3,json\ns: str=""; out: list[str]=[]\nfor line in sys.stdin:\n s+=line\n if sqlite3.complete_statement(s): out.append(s); s=""\nassert not s.strip()\nprint(json.dumps(out))'],
      { input: await readFile(path.join(root, 'migrations', name), 'utf8'), encoding: 'utf8' });
    assert.equal(parsed.status, 0);
    await db.batch(JSON.parse(parsed.stdout).map(sql => db.prepare(sql)));
  }
  const principal = await createBrowserPrincipal({ mf, db, encryptionKey, owner: 'capacity-owner' });
  const sdk = new OpenAI({ baseURL: 'https://agents.test/v1', apiKey: principal.apiKey, maxRetries: 0, fetch: (input, init) => dispatchSDKFetch(mf, input, init) });
  const file = await sdk.files.create({ purpose: 'assistants', file: await toFile(Buffer.from('A small owned capacity document.'), 'capacity.txt', { type: 'text/plain' }) });
  const store = await sdk.vectorStores.create({});
  await sdk.vectorStores.files.create(store.id, { file_id: file.id });
  let member;
  const deadline = Date.now() + 12000;
  do { await new Promise(resolve => setTimeout(resolve, 100)); member = await sdk.vectorStores.files.retrieve(file.id, { vector_store_id: store.id }); }
  while (member.status === 'in_progress' && Date.now() < deadline);
  assert.equal(member.status, 'completed');
  assert.equal(embeddings, 1);
  const before = await db.prepare('SELECT * FROM file_search_storage_clocks WHERE account_id=?').bind(principal.owner).first();
  assert.equal(before.dimensions, 2);
  await mf.dispatchFetch('https://agents.test/__test/retrieval-maintenance', { method: 'POST' });
  await mf.dispatchFetch('https://agents.test/__test/retrieval-maintenance', { method: 'POST' });
  await sdk.files.delete(file.id);
  await mf.dispatchFetch('https://agents.test/__test/retrieval-maintenance', { method: 'POST' });
  const after = await db.prepare('SELECT * FROM file_search_storage_clocks WHERE account_id=?').bind(principal.owner).first();
  assert.equal(vectors.size, 0);
  assert.equal(deletes, 1);
  assert.equal(after.finalized, 1);
  assert.equal(after.pending_json, null);
  assert.equal(after.carry_dimension_ms, '0');
  const usage = await db.prepare('SELECT COALESCE(SUM(CAST(quantity AS INTEGER)),0) AS quantity,COALESCE(SUM(CAST(amount_microusd AS INTEGER)),0) AS amount FROM usage_ledger l JOIN usage_reservations r ON r.id=l.reservation_id WHERE r.account_id=? AND l.unit=?').bind(principal.owner, unit).first();
  const expected = after.dimensions * (after.removed_at_ms - after.created_at_ms);
  assert.equal(usage.quantity, expected);
  assert.equal(usage.amount, Math.ceil(expected / 1000));
  assert.equal((await db.prepare("SELECT COUNT(*) AS n FROM usage_reservations WHERE account_id=? AND state='reserved'").bind(principal.owner).first()).n, 0);
  await sdk.vectorStores.delete(store.id);
  await writeFile(path.join(directory, 'receipt.json'), JSON.stringify({ source: process.env.MAGENTS_TEST_EXPECTED_BUILD ?? process.env.GITHUB_SHA ?? null,
    fixture: 'actual-rust-worker-controlled-vectorize', embeddings, native_deletes: deletes, model_calls: 0, vectors_after_delete: vectors.size,
    allocated_dimension_ms: usage.quantity, micro_usd: usage.amount, finalized: true, holds: 0 }, null, 2));
});
