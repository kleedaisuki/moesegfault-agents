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


test('compiled Rust Worker: bounded file batches fund separately, resume only unsubmitted effects and cancel without credit', { timeout: 90000 }, async t => {
  const encryptionKey = randomBytes(32), identity = createIdentityFixture();
  const deployment = JSON.parse(await readFile(path.join(root, 'wrangler.jsonc'), 'utf8'));
  const pricing = JSON.parse(deployment.env.staging.vars.RESOURCE_PRICING_JSON);
  const controlledUnits=new Set(['embedding_input_bytes','queried_dimensions','written_dimensions','allocated_vector_dimension_ms','conversion_input_bytes']);
  pricing.tariffs=pricing.tariffs.filter(tariff=>tariff.resource!=='worker'||!controlledUnits.has(tariff.unit));
  for (const [unit, denominator] of [['embedding_input_bytes', 1], ['queried_dimensions', 100], ['written_dimensions', 100], ['allocated_vector_dimension_ms', 1000]]) {
    pricing.tariffs.push({ resource: 'worker', unit, microusd_numerator: 1, quantity_denominator: denominator, price_version: pricing.version });
  }
  const profile = { embedding: { model: '@cf/baai/bge-m3', revision: 'controlled-batch-v1', dimensions: 2, input_byte_limit: 32768 },
    ai_binding: 'AI', vector_binding: 'VECTORIZE', namespace: 'batch-fixture',
    embedding_allocation: { resource: 'worker', unit: 'embedding_input_bytes' }, query_allocation: { resource: 'worker', unit: 'queried_dimensions' }, upsert_allocation: { resource: 'worker', unit: 'written_dimensions' } };
  const vectors = new Map();
  let embeddings = 0, upserts = 0;
  let db;
  const embeddingSizes=[];
  await mkdir(path.join(root, '.temp'), { recursive: true });
  const directory = await mkdtemp(path.join(root, '.temp/file-batch-funding-'));
  const mf = new Miniflare(convertV4MiniflareOptions({ modulesRoot: build,
    modules: [{ type: 'ESModule', path: path.join(build, 'worker/shim.mjs'), contents: await readFile(new URL('./file-search-shim.mjs', import.meta.url), 'utf8') },
      { type: 'ESModule', path: path.join(build, 'index.js') }, { type: 'CompiledWasm', path: path.join(build, 'index_bg.wasm') }],
    compatibilityDate: '2026-10-23', compatibilityFlags: ['enable_request_signal'], resourcePersistencePath: path.join(directory, 'storage'),
    d1Databases: ['DB'], r2Buckets: ['WORKSPACES', 'CONVERSATIONS', 'DIAGNOSTICS'],
    durableObjects: { SESSIONS: { className: 'Session', useSQLite: true }, AUTH_SESSIONS: { className: 'AuthSession', useSQLite: true },
      CONVERSATION_STATE: { className: 'ConversationState', useSQLite: true }, MODEL_OPERATIONS: { className: 'ModelOperation', useSQLite: true }, FILE_INGEST: { className: 'FileIngest', useSQLite: true } },
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
        case '/embed': embeddings++; embeddingSizes.push(args[1].text.reduce((n,text)=>n+Buffer.byteLength(text),0)); return Response.json({ shape: [args[1].text.length, 2], data: args[1].text.map(() => [1, 0]) });
        case '/upsert': {
          upserts++;
          for (const vector of args[0]) {assert.deepEqual(vector.values,[1,0]);vectors.set(vector.id,vector);}
          if(upserts===1){
            const next=await db.prepare('SELECT SUM(length(CAST(text AS BLOB))) AS bytes FROM (SELECT text FROM file_search_chunks WHERE ordinal>=32 ORDER BY ordinal LIMIT 32)').first();
            assert.ok(next.bytes>1000,'The discriminator must contain more than one full source batch');
            // An operator credit cap changes concurrently with known usage. The
            // immutable holds remain protected; no synthetic usage is appended.
            await db.prepare('UPDATE usage_accounts SET budget_microusd=spent_microusd+reserved_microusd+? WHERE account_id=?').bind(Math.floor(next.bytes/2),'capacity-owner').run();
            return Response.json({error:'synthetic_ambiguous_write'},{status:503});
          }
          return Response.json({mutationId:'known-upsert-'+upserts});
        }
        case '/query': return Response.json({ matches: [...vectors.values()].filter(record =>
          Object.entries(args[1].filter).every(([key, filter]) => '$eq' in filter ? record.metadata[key] === filter.$eq : filter.$in.includes(record.metadata[key])))
          .slice(0, args[1].topK).map(record => ({ id: record.id, score: 1 })) });
        case '/getByIds': return Response.json(args[0].map(id => vectors.get(id)).filter(Boolean));
        case '/deleteByIds': for (const id of args[0]) vectors.delete(id); return Response.json({ mutationId: 'known-delete' });
        case '/describe': return Response.json({ dimensions: 2, vectorCount: vectors.size });
        default: throw Error('unexpected_retrieval_effect');
      }
    },
  }));
  t.after(() => mf.dispose());
  db = await mf.getD1Database('DB');
  for (const name of (await readdir(path.join(root, 'migrations'))).filter(name => name.endsWith('.sql')).sort()) {
    const parsed = spawnSync(process.env.PYTHON ?? 'python', ['-c', 'import sys,sqlite3,json\ns: str=""; out: list[str]=[]\nfor line in sys.stdin:\n s+=line\n if sqlite3.complete_statement(s): out.append(s); s=""\nassert not s.strip()\nprint(json.dumps(out))'],
      { input: await readFile(path.join(root, 'migrations', name), 'utf8'), encoding: 'utf8' });
    assert.equal(parsed.status, 0);
    await db.batch(JSON.parse(parsed.stdout).map(sql => db.prepare(sql)));
  }
  const principal = await createBrowserPrincipal({ mf, db, encryptionKey, owner: 'capacity-owner' });
  const sdk = new OpenAI({ baseURL: 'https://agents.test/v1', apiKey: principal.apiKey, maxRetries: 0, fetch: (input, init) => dispatchSDKFetch(mf, input, init) });

  const fragment=process.env.MAGENTS_FILE_FUNDING_CASE??'all';
  let store,held;let summary={};
  if(fragment!=='cancel'){
  const text='batch recovery tokens 雪 crabs\n'.repeat(1600);
  const file=await sdk.files.create({purpose:'assistants',file:await toFile(Buffer.from(text),'batches.txt',{type:'text/plain'})});
  store=await sdk.vectorStores.create({});
  await sdk.vectorStores.files.create(store.id,{file_id:file.id,chunking_strategy:{type:'static',static:{max_chunk_size_tokens:100,chunk_overlap_tokens:0}}});
  const pausedDeadline=Date.now()+30000;let member;
  do{await new Promise(r=>setTimeout(r,150));member=await sdk.vectorStores.files.retrieve(file.id,{vector_store_id:store.id});}
  while(!member.last_error?.message.includes('file_search_budget_required')&&Date.now()<pausedDeadline);
  assert.equal(member.status,'failed');assert.ok(member.last_error?.message.includes('file_search_budget_required'),JSON.stringify(member));
  const row=await db.prepare('SELECT generation,funding_blocked,staged_chunks,chunk_count FROM vector_store_files WHERE vector_store_id=? AND file_id=?').bind(store.id,file.id).first();
  assert.equal(row.funding_blocked,1);assert.ok(row.chunk_count>32);assert.equal(row.staged_chunks,row.chunk_count);
  assert.equal(embeddings,1);assert.equal(upserts,1);
  const fundingBefore=(await db.prepare('SELECT COUNT(*) AS n FROM file_search_funding').first()).n;
  for(let n=0;n<3;n++)await sdk.vectorStores.files.retrieve(file.id,{vector_store_id:store.id});
  assert.equal((await db.prepare('SELECT COUNT(*) AS n FROM file_search_funding').first()).n,fundingBefore,'Paused GET polling cannot purchase another turn or effect');
  await assert.rejects(sdk.post('/vector_stores/'+store.id+'/files/'+file.id+'/retry',{body:{}}),error=>error.status===409);
  await db.prepare('UPDATE usage_accounts SET budget_microusd=budget_microusd+1000000 WHERE account_id=?').bind(principal.owner).run();
  await sdk.post('/vector_stores/'+store.id+'/files/'+file.id+'/retry',{body:{}});
  const completedDeadline=Date.now()+30000;
  do{await new Promise(r=>setTimeout(r,150));member=await sdk.vectorStores.files.retrieve(file.id,{vector_store_id:store.id});}
  while(member.status==='in_progress'&&Date.now()<completedDeadline);
  assert.equal(member.status,'completed',JSON.stringify(member.last_error));
  assert.equal(embeddings,Math.ceil(row.chunk_count/32));assert.equal(upserts,embeddings,'The ambiguous first upsert is observed, never submitted again');
  assert.equal((await db.prepare('SELECT generation FROM vector_store_files WHERE vector_store_id=? AND file_id=?').bind(store.id,file.id).first()).generation,row.generation);
  const measured=await db.prepare("SELECT SUM(l.quantity) AS quantity,SUM(l.amount_microusd) AS amount FROM usage_ledger l JOIN usage_reservations r ON r.id=l.reservation_id WHERE r.account_id=? AND l.unit='embedding_input_bytes'").bind(principal.owner).first();
  const staged=await db.prepare('SELECT SUM(length(CAST(text AS BLOB))) AS bytes FROM file_search_chunks WHERE vector_store_id=? AND file_id=?').bind(store.id,file.id).first();
  assert.equal(measured.quantity,staged.bytes);assert.equal(measured.amount,staged.bytes);assert.ok(measured.amount>20000,'This real ledger exceeds the former whole-generation reservation');
  assert.equal(embeddingSizes.reduce((a,b)=>a+b,0),staged.bytes);
  const settledDeadline=Date.now()+4000;
  do{await new Promise(r=>setTimeout(r,50));held=(await db.prepare("SELECT COUNT(*) AS n FROM usage_reservations WHERE account_id=? AND state='reserved' AND id LIKE 'file-ingest:%'").bind(principal.owner).first()).n;}while(held&&Date.now()<settledDeadline);
  assert.equal(held,0);assert.equal((await db.prepare("SELECT COUNT(*) AS n FROM resource_observations WHERE account_id=? AND status='pending'").bind(principal.owner).first()).n,0);
  summary={chunks:row.chunk_count,batches:embeddings,upserts,embedding_allocation_bytes:measured.quantity,embedding_allocation_microusd:measured.amount,paused_polling_no_new_funding:true,same_generation_resume:true,unknown_upsert_not_replayed:true};
  }
  if(fragment==='batches'){
    await writeFile(path.join(directory,'receipt.json'),JSON.stringify({source:process.env.MAGENTS_TEST_EXPECTED_BUILD??process.env.GITHUB_SHA??null,fragment,...summary,model_calls:0,terminal_holds:0},null,2));return;
  }
  if(!store)store=await sdk.vectorStores.create({});
  const next=await sdk.files.create({purpose:'assistants',file:await toFile(Buffer.from('Cancel this before any paid effect.'),'cancel.txt')});
  await db.prepare('UPDATE usage_accounts SET budget_microusd=spent_microusd+21000 WHERE account_id=?').bind(principal.owner).run();
  await sdk.vectorStores.files.create(store.id,{file_id:next.id});
  await mf.dispatchFetch('https://agents.test/__test/retrieval-maintenance',{method:'POST'});
  const cancelDeadline=Date.now()+6000;let cancelled;
  do{await new Promise(r=>setTimeout(r,100));cancelled=await db.prepare('SELECT funding_blocked FROM vector_store_files WHERE vector_store_id=? AND file_id=?').bind(store.id,next.id).first();}while(!cancelled?.funding_blocked&&Date.now()<cancelDeadline);
  assert.equal(cancelled.funding_blocked,1,JSON.stringify(await db.prepare('SELECT status,last_error_json,lease_id,retry_at FROM vector_store_files WHERE file_id=?').bind(next.id).first()));
  if(fragment==='cancel')assert.deepEqual([embeddings,upserts],[0,0]);
  const cancelFundingDeadline=Date.now()+4000;
  do{await new Promise(r=>setTimeout(r,50));held=(await db.prepare("SELECT COUNT(*) AS n FROM usage_reservations WHERE account_id=? AND state='reserved' AND id LIKE 'file-ingest:%'").bind(principal.owner).first()).n;}while(held&&Date.now()<cancelFundingDeadline);
  assert.equal(held,0);
  await db.prepare('UPDATE usage_accounts SET budget_microusd=spent_microusd+reserved_microusd WHERE account_id=?').bind(principal.owner).run();
  assert.equal((await db.prepare('SELECT budget_microusd-spent_microusd-reserved_microusd AS available FROM usage_accounts WHERE account_id=?').bind(principal.owner).first()).available,0);
  const stopped=await sdk.vectorStores.files.poll(store.id,next.id,{pollIntervalMs:0,signal:AbortSignal.timeout(2500)});
  assert.equal(stopped.status,'failed');assert.equal(stopped.last_error.code,'server_error');assert.ok(stopped.last_error.message.includes('file_search_budget_required'));
  const callsBefore=[embeddings,upserts];assert.equal((await sdk.vectorStores.files.delete(next.id,{vector_store_id:store.id})).deleted,true);
  assert.deepEqual([embeddings,upserts],callsBefore);assert.equal((await db.prepare('SELECT status FROM space_files WHERE id=?').bind(next.id).first()).status,'processed');
  assert.equal((await db.prepare('SELECT status FROM vector_store_files WHERE vector_store_id=? AND file_id=?').bind(store.id,next.id).first()).status,'cancelled');
  await writeFile(path.join(directory,'receipt.json'),JSON.stringify({source:process.env.MAGENTS_TEST_EXPECTED_BUILD??process.env.GITHUB_SHA??null,fixture:'actual-rust-worker-bounded-funding',...summary,fragment,zero_credit_cancel:true,standard_sdk_poll_terminal:true,native_embeddings:embeddings,native_upserts:upserts,model_calls:0,terminal_holds:0},null,2));
});
