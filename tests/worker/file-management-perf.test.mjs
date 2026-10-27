import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { mkdir, mkdtemp, readFile, readdir, writeFile } from 'node:fs/promises';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import OpenAI, {toFile} from 'openai';
import {dispatchSDKFetch} from './sdk-miniflare-fetch.mjs';
import { Miniflare, convertV4MiniflareOptions } from 'miniflare';
import { createIdentityFixture } from './auth-journey.mjs';
import { createBrowserPrincipal } from './browser-principal.mjs';

const root = fileURLToPath(new URL('../../', import.meta.url));
const current = path.resolve(root, process.env.WORKER_BUILD_DIR ?? 'crates/agents-worker/build');

async function migrate(db) {
  for (const name of (await readdir(path.join(root, 'migrations'))).filter(name => name.endsWith('.sql')).sort()) {
    const parsed = spawnSync(process.env.MAGENTS_SCHEMA_PYTHON ?? process.env.PYTHON ?? 'python', ['-c',
      'import sys,sqlite3,json\ns: str=""; out: list[str]=[]\nfor line in sys.stdin:\n s+=line\n if sqlite3.complete_statement(s): out.append(s); s=""\nassert not s.strip(), s\nprint(json.dumps(out))'],
    { input: await readFile(path.join(root, 'migrations', name), 'utf8'), encoding: 'utf8' });
    assert.equal(parsed.status, 0, `${name}: ${parsed.stderr}`);
    await db.batch(JSON.parse(parsed.stdout).map(statement => db.prepare(statement)));
  }
}


async function measure(t, build, label) {
  const encryptionKey = randomBytes(32);
  const identity = createIdentityFixture();
  const deployment = JSON.parse(await readFile(path.join(root, 'wrangler.jsonc'), 'utf8'));
  await mkdir(path.join(root, '.temp'), { recursive: true });
  const directory = await mkdtemp(path.join(root, '.temp/file-management-perf-'));
  let externalCalls = 0;
  const mf = new Miniflare(convertV4MiniflareOptions({
    modulesRoot: build,
    modules: [
      { type: 'ESModule', path: path.join(build, 'worker/shim.mjs'),
        contents: await readFile(new URL('./file-management-perf-shim.mjs', import.meta.url), 'utf8') },
      { type: 'ESModule', path: path.join(build, 'index.js') },
      { type: 'CompiledWasm', path: path.join(build, 'index_bg.wasm') },
    ],
    compatibilityDate: '2026-10-23', compatibilityFlags: ['enable_request_signal'],
    resourcePersistencePath: path.join(directory, 'storage'),
    d1Databases: ['DB'], r2Buckets: ['WORKSPACES', 'CONVERSATIONS', 'DIAGNOSTICS'],
    durableObjects: {
      SESSIONS: { className: 'Session', useSQLite: true },
      AUTH_SESSIONS: { className: 'AuthSession', useSQLite: true },
      CONVERSATION_STATE: { className: 'ConversationState', useSQLite: true },
    },
    ratelimits: Object.fromEntries(['INGRESS', 'ACCOUNT', 'MUTATION'].map((name, index) => [
      `${name}_RATE_LIMITER`, { namespace_id: String(index + 1), simple: { limit: 1000, period: 60 } },
    ])),
    bindings: {
      ENVIRONMENT: 'test', APP_ORIGIN: 'https://agents.test', ALLOWED_ORIGIN: 'https://agents.test',
      DATA_ENCRYPTION_KEY: encryptionKey.toString('base64url'),
      IDENTITY_ISSUER: 'https://identity.test', IDENTITY_WEB_CLIENT_ID: 'test-dashboard',
      IDENTITY_NATIVE_CLIENT_ID: 'test-native', IDENTITY_REDIRECT_URI: 'https://agents.test/auth/callback',
      IDENTITY_NATIVE_REDIRECT_URI: 'http://127.0.0.1/callback',
      IDENTITY_CLIENT_PRIVATE_JWK: JSON.stringify(identity.privateJwk),
      BILLING_PRODUCT_ID: 'agents', BILLING_MODE: 'test',
      MODEL_REGISTRY_JSON: '[]', MODEL_PRICING_JSON: '{}',
      FILE_SEARCH_PROFILE_JSON: deployment.env.staging.vars.FILE_SEARCH_PROFILE_JSON,
      RESOURCE_PRICING_JSON: deployment.env.staging.vars.RESOURCE_PRICING_JSON,
      RUN_RESOURCE_RESERVATION_MICROUSD: '20000', FREE_GRANT_BUDGET_MICROUSD_PER_DAY: '100000000',
    },
    async outboundService(request) {
      externalCalls++;
      throw new Error(`Queued management benchmark must not call external service ${new URL(request.url).origin}`);
    },
  }));
  t.after(() => mf.dispose());
  const db = await mf.getD1Database('DB');
  await migrate(db);

  const owner = await createBrowserPrincipal({ mf, db, encryptionKey, owner: 'file-management-perf-owner' });
  const sdk = new OpenAI({ apiKey: owner.apiKey, baseURL: 'https://agents.test/v1', maxRetries: 0, timeout: 70000,
    fetch: (input, init) => dispatchSDKFetch(mf,input,init) });
  const snapshots=[];
  const state=async()=> (await mf.dispatchFetch('https://agents.test/__management/status')).json();
  const run=async(name,work)=>{
    await mf.dispatchFetch('https://agents.test/__management/arm',{method:'POST'});
    const start=performance.now();const result=await work();
    snapshots.push({name,duration_ms:performance.now()-start,...await state()});
    return result;
  };
  const uploadFile=await toFile(Buffer.from('Queued file management metadata must not run ingestion.'),'queued.txt',{type:'text/plain'});
  const source=await run('upload',()=>sdk.files.create({purpose:'assistants',file:uploadFile}));
  const store=await run('store_create',()=>sdk.vectorStores.create({name:'Management performance'}));
  const attached=await run('attach',()=>sdk.vectorStores.files.create(store.id,{file_id:source.id,attributes:{kind:'queued'}}));
  assert.equal(attached.status,'in_progress');
  assert.equal(attached.id,source.id);
  const polled=await run('poll',()=>sdk.vectorStores.files.retrieve(source.id,{vector_store_id:store.id}));
  assert.equal(polled.status,'in_progress');assert.deepEqual(polled.attributes,{kind:'queued'});
  const listed=await run('list',()=>sdk.vectorStores.files.list(store.id));
  assert.equal(listed.data.length,1);assert.equal(listed.data[0].id,source.id);
  await run('detach',()=>sdk.vectorStores.files.delete(source.id,{vector_store_id:store.id}));
  await assert.rejects(sdk.vectorStores.files.retrieve(source.id,{vector_store_id:store.id}),error=>error.status===404);
  await run('store_delete',()=>sdk.vectorStores.delete(store.id));
  await sdk.files.delete(source.id);
  const observations=(await db.prepare("SELECT resource,unit,SUM(CAST(quantity AS INTEGER)) AS quantity FROM resource_observations WHERE account_id=? AND resource='d1' AND status='observed' GROUP BY resource,unit ORDER BY resource,unit").bind(owner.owner).all()).results;
  const ledger=(await db.prepare("SELECT l.resource,l.unit,SUM(l.quantity) AS quantity,SUM(l.amount_microusd) AS amount FROM usage_ledger l JOIN usage_reservations r ON r.id=l.reservation_id WHERE r.account_id=? AND l.resource='d1' GROUP BY l.resource,l.unit ORDER BY l.resource,l.unit").bind(owner.owner).all()).results;
  assert.deepEqual(ledger.map(({resource,unit,quantity})=>({resource,unit,quantity})),observations,'Exact actual D1 metadata matches immutable charged quantities');
  assert.equal((await db.prepare("SELECT COUNT(*) AS n FROM resource_observations WHERE account_id=? AND status='pending'").bind(owner.owner).first()).n,0);
  assert.equal((await db.prepare("SELECT COUNT(*) AS n FROM usage_reservations WHERE account_id=? AND state='reserved'").bind(owner.owner).first()).n,0);
  assert.equal((await db.prepare("SELECT COUNT(*) AS n FROM usage_ledger l JOIN usage_reservations r ON r.id=l.reservation_id WHERE r.account_id=? AND l.resource='model'").bind(owner.owner).first()).n,0);
  assert.equal((await db.prepare("SELECT COUNT(*) AS n FROM file_search_chunks WHERE account_id=?").bind(owner.owner).first()).n,0);
  assert.equal(externalCalls,0);
  return {label,snapshots,observations,ledger,pending:0,reserved:0,external_calls:externalCalls};
}

test('compiled Rust Worker: queued file management reuses exact batched accounting without ingestion or changed business SQL', {timeout:110000},async t=>{
  const baseline=await measure(t,path.resolve(root,process.env.MAGENTS_BASELINE_WORKER_DIR),'accepted-7ed1653');
  const optimized=await measure(t,current,'file-management-batched');
  assert.deepEqual(optimized.observations,baseline.observations,'Same actual application D1 row quantities');
  assert.deepEqual(optimized.ledger,baseline.ledger,'Same pinned exact D1 charges and quantities');
  for(const name of ['store_create','attach','poll','list','detach','store_delete']){
    const before=baseline.snapshots.find(item=>item.name===name);const after=optimized.snapshots.find(item=>item.name===name);
    assert.equal(after.businessStatements,before.businessStatements,name+': same logical management SQL');
    assert.ok(after.rpcs<before.rpcs,name+': fewer accounting round trips');
    assert.ok(after.duration_ms<before.duration_ms,name+': faster equivalent fixed-RTT journey');
  }
  await mkdir(path.join(root,'.cache/verification'),{recursive:true});
  await writeFile(path.join(root,'.cache/verification/file-management-perf.json'),JSON.stringify({baseline,optimized,scope:'actual Rust Worker and native D1/R2; fixed20ms request DB RTT; queued ingestion explicitly deferred; not a staging SLO'},null,2)+'\n');
  console.log(JSON.stringify({baseline,optimized}));
});
