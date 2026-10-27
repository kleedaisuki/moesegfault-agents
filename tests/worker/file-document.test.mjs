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


test('compiled Rust Worker: document conversion is bounded recoverable and not billed as model tokens', { timeout: 45000 }, async t => {
  const encryptionKey = randomBytes(32), identity = createIdentityFixture();
  const deployment = JSON.parse(await readFile(path.join(root, 'wrangler.jsonc'), 'utf8'));
  const pricing = JSON.parse(deployment.env.staging.vars.RESOURCE_PRICING_JSON);
  const controlledUnits=new Set(['embedding_input_bytes','queried_dimensions','written_dimensions','allocated_vector_dimension_ms','conversion_input_bytes']);
  pricing.tariffs=pricing.tariffs.filter(tariff=>tariff.resource!=='worker'||!controlledUnits.has(tariff.unit));
  for (const [unit, denominator] of [['embedding_input_bytes', 64], ['queried_dimensions', 100], ['written_dimensions', 100], ['allocated_vector_dimension_ms', 1000], ['conversion_input_bytes', 100]]) {
    pricing.tariffs.push({ resource: 'worker', unit, microusd_numerator: 1, quantity_denominator: denominator, price_version: pricing.version });
  }
  const profile = { embedding: { model: '@cf/baai/bge-m3', revision: 'controlled-document-v1', dimensions: 2, input_byte_limit: 32768 },
    ai_binding: 'AI', vector_binding: 'VECTORIZE', namespace: 'document-fixture',
    embedding_allocation: { resource: 'worker', unit: 'embedding_input_bytes' }, query_allocation: { resource: 'worker', unit: 'queried_dimensions' }, upsert_allocation: { resource: 'worker', unit: 'written_dimensions' }, conversion_allocation: { resource: 'worker', unit: 'conversion_input_bytes' } };
  const vectors = new Map();
  let embeddings = 0, conversions = 0;
  const convertedInputs = [];
  await mkdir(path.join(root, '.temp'), { recursive: true });
  const directory = await mkdtemp(path.join(root, '.temp/vector-capacity-'));
  const mf = new Miniflare(convertV4MiniflareOptions({ modulesRoot: build,
    modules: [{ type: 'ESModule', path: path.join(build, 'worker/shim.mjs'), contents: await readFile(new URL('./file-document-shim.mjs', import.meta.url), 'utf8') },
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
      const native = url.pathname === '/native-convert';
      const submitted = await request.json();
      const args = native ? [submitted.files.map(file => ({name:file.name,bytes:Array.from(Buffer.from(file.data,'base64'))})),submitted.options] : submitted;
      if (native) {
        assert.equal(submitted.files[0].mimeType,submitted.files[0].name.endsWith('.pdf')?'application/pdf':'application/vnd.openxmlformats-officedocument.wordprocessingml.document');
        assert.equal(request.headers.get('authorization'),null);
      }
      switch (url.pathname) {
        case '/native-convert':
        case '/convert': {
          conversions++;
          assert.deepEqual(args[1], { conversionOptions: { output: { format: 'text' }, pdf: { metadata: false, images:{convert:false} }, docx:{images:{convert:false}} } });
          assert.equal(args[0].length, 1);
          const input = args[0][0]; convertedInputs.push(Buffer.from(input.bytes));
          assert.ok(['source.pdf', 'source.docx'].includes(input.name));
          if (process.env.MAGENTS_DOCUMENT_CASE === 'native-contract') return Response.json({errors:[{code:10042,message:'private provider body containing synthetic secret and source text'}]}, {status:422,headers:{'x-request-id':'native-document-rejected-1','cf-ray':'abc123-SIN'}});
          const result = [{ name: input.name, format: 'text', mimeType: input.name.endsWith('.pdf') ? 'application/pdf' : 'application/vnd.openxmlformats-officedocument.wordprocessingml.document',
            data: Buffer.from(input.bytes).includes(Buffer.from('empty-text-layer')) ? '' : 'Text-layer extraction: silver-lantern-513 雪', tokens: 999999999 }];
          return Response.json(native ? {result} : result);
        }
        case '/embed': embeddings++; return Response.json({ shape: [args[1].text.length, 2], data: args[1].text.map(() => [1, 0]) });
        case '/upsert': for (const vector of args[0]) vectors.set(vector.id, vector); return Response.json({ mutationId: 'known-upsert' });
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
  const db = await mf.getD1Database('DB');
  for (const name of (await readdir(path.join(root, 'migrations'))).filter(name => name.endsWith('.sql')).sort()) {
    const parsed = spawnSync(process.env.PYTHON ?? 'python', ['-c', 'import sys,sqlite3,json\ns: str=""; out: list[str]=[]\nfor line in sys.stdin:\n s+=line\n if sqlite3.complete_statement(s): out.append(s); s=""\nassert not s.strip()\nprint(json.dumps(out))'],
      { input: await readFile(path.join(root, 'migrations', name), 'utf8'), encoding: 'utf8' });
    assert.equal(parsed.status, 0);
    await db.batch(JSON.parse(parsed.stdout).map(sql => db.prepare(sql)));
  }
  const principal = await createBrowserPrincipal({ mf, db, encryptionKey, owner: 'capacity-owner' });
  const sdk = new OpenAI({ baseURL: 'https://agents.test/v1', apiKey: principal.apiKey, maxRetries: 0, fetch: (input, init) => dispatchSDKFetch(mf, input, init) });
  if (process.env.MAGENTS_DOCUMENT_CASE === 'native-contract') {
    const bytes=await readFile(path.join(root,'tests/fixtures/retrieval/text-layer.pdf'));
    const file=await sdk.files.create({purpose:'assistants',file:await toFile(bytes,'new-native-contract.pdf',{type:'application/pdf'})});
    const store=await sdk.vectorStores.create({});
    await sdk.vectorStores.files.create(store.id,{file_id:file.id});
    let member;const deadline=Date.now()+12000;
    do {await new Promise(resolve=>setTimeout(resolve,100));member=await sdk.vectorStores.files.retrieve(file.id,{vector_store_id:store.id});}
    while(member.status==='in_progress'&&Date.now()<deadline);
    assert.equal(member.status,'failed');
    assert.equal(member.last_error.message,'file_search_document_upstream_rejected');
    const cached=await db.prepare('SELECT status,error_code,diagnostics_json,provider_receipt_id FROM file_search_extractions WHERE account_id=? AND file_id=?').bind(principal.owner,file.id).first();
    assert.equal(cached.status,'failed');
    const diagnostics=JSON.parse(cached.diagnostics_json);
    assert.equal(diagnostics.status,422);assert.equal(diagnostics.request_id,'native-document-rejected-1');
    assert.equal(diagnostics.ray_id,'abc123-SIN');assert.equal(diagnostics.phase,'http_error');
    assert.equal(diagnostics.codec,'application/pdf');assert.equal(diagnostics.input_bytes,593);
    assert.equal(diagnostics.body_complete,true);assert.equal(diagnostics.upstream_error_code,10042);
    assert.ok(diagnostics.request_bytes>593&&diagnostics.response_bytes>0);
    assert.ok(!cached.diagnostics_json.includes('private provider body')&&!cached.diagnostics_json.includes('synthetic secret'));
    assert.deepEqual(convertedInputs,[bytes]);assert.equal(conversions,1);assert.equal(embeddings,0);
    await sdk.vectorStores.files.retrieve(file.id,{vector_store_id:store.id});
    await mf.dispatchFetch('https://agents.test/__test/retrieval-maintenance',{method:'POST'});
    assert.equal(conversions,1,'Known rejected native request is never resubmitted');
    await sdk.files.delete(file.id);await sdk.vectorStores.delete(store.id);
    await writeFile(path.join(directory,'native-contract-receipt.json'),JSON.stringify({source:process.env.GITHUB_SHA??null,conversions,embeddings,model_calls:0,status:diagnostics.status,request_id:diagnostics.request_id,raw_body_in_diagnostics:false},null,2));
    return;
  }
  const makeOffice = spawnSync(process.env.PYTHON ?? 'python', ['-c',
    'import io,zipfile,sys\nb: io.BytesIO=io.BytesIO()\nwith zipfile.ZipFile(b,"w",compression=zipfile.ZIP_STORED) as z:\n z.writestr("[Content_Types].xml", \'<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>\')\n z.writestr("_rels/.rels", \'<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>\')\n z.writestr("word/document.xml", \'<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>silver-lantern-513</w:t></w:r></w:p></w:body></w:document>\')\nsys.stdout.buffer.write(b.getvalue())']);
  assert.equal(makeOffice.status, 0, makeOffice.stderr.toString());
  const inputs = [await readFile(path.join(root,'tests/fixtures/retrieval/text-layer.pdf')), makeOffice.stdout];
  const store = await sdk.vectorStores.create({});
  const files = [];
  for (const [index, input] of inputs.entries()) {
    // Deliberately unrelated names demonstrate detection from actual bytes.
    const file = await sdk.files.create({ purpose: 'assistants', file: await toFile(input, `unrelated-${index}.bin`, { type: 'application/octet-stream' }) });
    files.push(file);
    await sdk.vectorStores.files.create(store.id, { file_id: file.id });
    let member;
    const deadline = Date.now() + 12000;
    do { await new Promise(resolve => setTimeout(resolve, 100)); member = await sdk.vectorStores.files.retrieve(file.id, { vector_store_id: store.id }); }
    while (member.status === 'in_progress' && Date.now() < deadline);
    assert.equal(member.status, 'completed', JSON.stringify(member.last_error));
    const cached = await db.prepare("SELECT status,text FROM file_search_extractions WHERE account_id=? AND file_id=?").bind(principal.owner, file.id).first();
    assert.equal(cached.status, 'completed');
    assert.ok(cached.text.includes('silver-lantern-513'));
    await sdk.vectorStores.files.retrieve(file.id, { vector_store_id: store.id });
  }
  assert.equal(conversions, 2, 'Completed extraction must not dispatch twice');
  assert.equal(embeddings, 2);
  assert.deepEqual(convertedInputs, inputs, 'The native service receives the original bounded source bytes');
  const result = await sdk.vectorStores.search(store.id, { query: 'silver-lantern-513' });
  assert.equal(result.data.length, 2);
  assert.ok(result.data.every(hit => hit.content.some(part => part.text.includes('silver-lantern-513'))));
  const usage = await db.prepare("SELECT COALESCE(SUM(CAST(quantity AS INTEGER)),0) AS quantity FROM usage_ledger l JOIN usage_reservations r ON r.id=l.reservation_id WHERE r.account_id=? AND l.unit='conversion_input_bytes'").bind(principal.owner).first();
  assert.equal(usage.quantity, inputs.reduce((n,b) => n+b.length,0), 'Estimated converter tokens are not vendor usage');
  const invalid = await sdk.files.create({ purpose: 'assistants', file: await toFile(Buffer.from('PK\x03\x04broken'), 'broken.docx') });
  await sdk.vectorStores.files.create(store.id, { file_id: invalid.id });
  let member;
  const deadline = Date.now()+12000;
  do { await new Promise(resolve => setTimeout(resolve,100)); member=await sdk.vectorStores.files.retrieve(invalid.id,{vector_store_id:store.id}); }
  while (member.status==='in_progress' && Date.now()<deadline);
  assert.equal(member.status,'failed');
  assert.equal(member.last_error.code,'invalid_file');
  assert.equal(conversions,2,'Malformed archives are rejected before a service call');
  const empty = await sdk.files.create({ purpose: 'assistants', file: await toFile(Buffer.from('%PDF-1.4 empty-text-layer'), 'scan.pdf') });
  await sdk.vectorStores.files.create(store.id, { file_id: empty.id });
  const emptyDeadline=Date.now()+12000;
  do { await new Promise(resolve=>setTimeout(resolve,100));member=await sdk.vectorStores.files.retrieve(empty.id,{vector_store_id:store.id}); }
  while (member.status==='in_progress' && Date.now()<emptyDeadline);
  assert.equal(member.status,'failed');
  assert.equal(member.last_error.code,'invalid_file');
  assert.equal(conversions,3);
  assert.equal(embeddings,3,'Only two documents and the one user search may be embedded; empty extraction has no OCR or generative fallback');
  for (const file of [...files,invalid,empty]) await sdk.files.delete(file.id);
  await mf.dispatchFetch('https://agents.test/__test/retrieval-maintenance', { method: 'POST' });
  assert.equal((await db.prepare('SELECT COUNT(*) AS n FROM file_search_extractions WHERE account_id=?').bind(principal.owner).first()).n,0);
  await sdk.vectorStores.delete(store.id);
  await writeFile(path.join(directory,'receipt.json'),JSON.stringify({source:process.env.MAGENTS_TEST_EXPECTED_BUILD ?? process.env.GITHUB_SHA ?? null,
    fixture:'actual-rust-worker-controlled-document-conversion', conversions,embeddings,model_calls:0,formats:['pdf','docx'],extraction_cache_after_delete:0},null,2));
});
