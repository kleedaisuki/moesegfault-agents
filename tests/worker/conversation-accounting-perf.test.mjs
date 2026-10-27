import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { mkdir, mkdtemp, readFile, readdir, writeFile } from 'node:fs/promises';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import OpenAI from 'openai';
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
  const directory = await mkdtemp(path.join(root, '.temp/conversation-accounting-perf-'));
  let externalCalls = 0;
  const mf = new Miniflare(convertV4MiniflareOptions({
    modulesRoot: build,
    modules: [
      { type: 'ESModule', path: path.join(build, 'worker/shim.mjs'),
        contents: await readFile(new URL('./conversation-accounting-perf-shim.mjs', import.meta.url), 'utf8') },
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
      RESOURCE_PRICING_JSON: deployment.env.staging.vars.RESOURCE_PRICING_JSON,
      RUN_RESOURCE_RESERVATION_MICROUSD: '20000', FREE_GRANT_BUDGET_MICROUSD_PER_DAY: '100000000',
    },
    async outboundService(request) {
      externalCalls++;
      throw new Error(`Conversation I/O journey must not call external service ${new URL(request.url).origin}`);
    },
  }));
  t.after(() => mf.dispose());
  const db = await mf.getD1Database('DB');
  await migrate(db);

  const owner = await createBrowserPrincipal({ mf, db, encryptionKey, owner: 'conversation-accounting-perf-owner' });
  const sdk = new OpenAI({ apiKey: owner.apiKey, baseURL: 'https://agents.test/v1', maxRetries: 0, timeout: 70000,
    fetch: (input, init) => mf.dispatchFetch(input, init) });
  const conversation = await sdk.conversations.create({items:[{type:'message',role:'user',content:[{type:'input_text',text:'Retained content must survive metadata deletion until explicit purge.'}]}]});
  const namespace = await mf.getDurableObjectNamespace('CONVERSATION_STATE');
  const actor = namespace.get(namespace.idFromName(owner.owner + ':' + conversation.id));
  await actor.fetch('https://fixture/__performance/arm', {method:'POST'});
  const before = await (await actor.fetch('https://fixture/__performance/status')).json();
  const started = performance.now();
  const changed = await sdk.conversations.update(conversation.id,{metadata:{purpose:'accounting-perf'}});
  assert.equal(changed.metadata.purpose,'accounting-perf');
  const updated = await (await actor.fetch('https://fixture/__performance/status')).json();
  const updateMs = performance.now()-started;
  const deleted = await sdk.conversations.delete(conversation.id);
  assert.equal(deleted.deleted,true);
  const retained = await sdk.conversations.items.list(conversation.id);
  assert.equal(retained.data.length,1,'Metadata deletion never purges retained user items');
  const purged = await mf.dispatchFetch('https://agents.test/v1/conversations/'+conversation.id+'/purge',{method:'POST',headers:{Authorization:'Bearer '+owner.apiKey}});
  assert.equal(purged.status,200);
  assert.equal((await purged.json()).purged,true);
  const status = await (await actor.fetch('https://fixture/__performance/status')).json();
  const pending=(await db.prepare("SELECT COUNT(*) AS n FROM resource_observations WHERE account_id=? AND status='pending'").bind(owner.owner).first()).n;
  assert.equal(pending,0);
  assert.equal((await db.prepare("SELECT COUNT(*) AS n FROM usage_reservations WHERE account_id=? AND state='reserved'").bind(owner.owner).first()).n,0);
  assert.equal((await db.prepare("SELECT COUNT(*) AS n FROM usage_ledger l JOIN usage_reservations r ON r.id=l.reservation_id WHERE r.account_id=? AND l.resource='model'").bind(owner.owner).first()).n,0);
  assert.equal(externalCalls,0);
  return {label,rtt_ms:before.rttMs,update_ms:updateMs,update_rpcs:updated.rpcs,update_business_statements:updated.businessStatements,update_injected_delay_ms:updated.delayMs,total_rpcs:status.rpcs,total_business_statements:status.businessStatements,batches:status.batches,pending,external_calls:externalCalls};
}

test('compiled Rust Worker: Conversation accounting batches remove measured D1 round trips without changing deletion or exact settlement', {timeout:85000}, async t => {
  const baseline = process.env.MAGENTS_BASELINE_WORKER_DIR ? await measure(t,path.resolve(root,process.env.MAGENTS_BASELINE_WORKER_DIR),'baseline-02d483b') : null;
  const optimized = await measure(t,current,'batched');
  assert.ok(optimized.batches > 0,'Actual Rust accounting must call native D1.batch');
  if (baseline) {
    assert.equal(optimized.update_business_statements,baseline.update_business_statements,'Same logical publication work');
    assert.equal(optimized.total_business_statements,baseline.total_business_statements,'Same management journey');
    assert.ok(optimized.update_rpcs < baseline.update_rpcs/2,'Eliminate accounting RTT amplification, not user-data work');
    assert.ok(optimized.update_ms < baseline.update_ms,'Equivalent fixed-RTT actual Worker metadata mutation is faster');
  }
  await mkdir(path.join(root,'.cache/perf'),{recursive:true});
  await writeFile(path.join(root,'.cache/perf/conversation-accounting.json'),JSON.stringify({baseline,optimized,scope:'controlled actual Rust Worker + D1, fixed20ms per actor DB RPC; not a staging SLO'},null,2)+'\n');
  console.log(JSON.stringify({baseline,optimized}));
});
