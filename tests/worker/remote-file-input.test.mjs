import OpenAI from 'openai';
import {createBrowserPrincipal} from './browser-principal.mjs';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { createHash, randomBytes } from 'node:crypto';
import { mkdir, mkdtemp, readFile, readdir, writeFile } from 'node:fs/promises';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import { Miniflare, convertV4MiniflareOptions } from 'miniflare';
import { createIdentityFixture } from './auth-journey.mjs';
import { createAuxiliaryProviderFixture } from './auxiliary-journey.mjs';

const root = fileURLToPath(new URL('../../', import.meta.url));
const build = path.resolve(root, process.env.WORKER_BUILD_DIR ?? 'crates/agents-worker/build');
async function migrate(db) {
  for (const name of (await readdir(path.join(root, 'migrations'))).filter(name => name.endsWith('.sql')).sort()) {
    const sql = await readFile(path.join(root, 'migrations', name), 'utf8');
    const parsed = spawnSync(process.env.PYTHON ?? 'python', ['-c',
      'import sys,sqlite3,json\ns: str=""; out: list[str]=[]\nfor line in sys.stdin:\n s+=line\n if sqlite3.complete_statement(s): out.append(s); s=""\nassert not s.strip(), s\nprint(json.dumps(out))'],
    { input: sql, encoding: 'utf8' });
    assert.equal(parsed.status, 0, `${name}: ${parsed.stderr}`);
    await db.batch(JSON.parse(parsed.stdout).map(statement => db.prepare(statement)));
  }
}

test('compiled Rust Worker: remote file freeze metadata and prepaid read-only canonical rendering', {timeout:60000}, async t => {
  const encryptionKey = randomBytes(32);
  const identity = createIdentityFixture();
  const providers = createAuxiliaryProviderFixture();
  const calls = [];
  let text = 'Frozen remote note 雪🦀.';
  const origin = 'https://public-file-fixture.example.com';
  const deployment = JSON.parse(await readFile(path.join(root, 'wrangler.jsonc'), 'utf8'));
  await mkdir(path.join(root, '.temp'), { recursive: true });
  const directory = await mkdtemp(path.join(root, '.temp/native-semantics-'));
  const mf = new Miniflare(convertV4MiniflareOptions({
    modulesRoot: build,
    modules: [
      { type: 'ESModule', path: path.join(build, 'worker/shim.mjs') },
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
      MODEL_OPERATIONS: { className: 'ModelOperation', useSQLite: true },
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
      const url = new URL(request.url);
      if (url.origin === origin) {
        calls.push(url.pathname);
        assert.equal(request.method, 'GET');
        for (const name of ['authorization','cookie','proxy-authorization','x-api-key','traceparent','tracestate','baggage']) {
          assert.equal(request.headers.get(name), null, 'No secret or trace is forwarded: '+name);
        }
        if (url.pathname === '/redirect') return new Response(null, {status:302,headers:{Location:origin+'/target'}});
        if (url.pathname === '/private-redirect') return new Response(null, {status:307,headers:{Location:'https://127.0.0.1/secret'}});
        if (url.pathname === '/large') return new Response('tiny', {headers:{'Content-Length':String(5*1024*1024)}});
        if (url.pathname === '/chunked-large') {
          let sent=0;
          return new Response(new ReadableStream({pull(controller){
            if(sent++===5){controller.close();return;}controller.enqueue(new Uint8Array(1024*1024));
          }}));
        }
        if (url.pathname === '/unavailable') return new Response('private vendor body', {status:503});
        assert.ok(['/note','/target'].includes(url.pathname), url.pathname);
        return new Response(text, {headers:{'Content-Type':'text/plain;charset=utf-8'}});
      }
      const response = await providers.handle(request);
      assert.notEqual(response, undefined, `Native fixtures must not access external services: ${new URL(request.url).origin}`);
      return response;
    },
  }));
  t.after(() => mf.dispose());
  const db = await mf.getD1Database('DB');
  await migrate(db);
  async function request(method, endpoint, body, expected = 200, bearer) {
    const response = await mf.dispatchFetch('https://agents.test' + endpoint, {
      method, headers: { Authorization: `Bearer ${bearer}`, 'Content-Type': 'application/json', 'CF-Connecting-IP': '192.0.2.73' },
      ...(body === undefined ? {} : { body: JSON.stringify(body) }),
    });
    const text = await response.text();
    assert.equal(response.status, expected, `${method} ${endpoint}: ${text}`);
    return JSON.parse(text);
  }
  const principal = await createBrowserPrincipal({mf,db,encryptionKey,owner:'remote-file-owner'});
  await principal.browser('GET','/v1/account');
  const credential = await principal.browser('POST','/v1/provider-keys',{protocol:'openai',base_url:'https://auxiliary-provider.example.com/v1',api_key:'synthetic-auxiliary-owned-key'},201);
  const agent = await request('POST','/v1/agents',{name:'Remote file input',instructions:'Pinned remote input policy',model:{provider:'byok',credential_id:credential.id,protocol:'responses',model:'fixture-native-model',context_window:8192,max_output_tokens:64},workspace:false,tools:[]},201,principal.apiKey);
  const sdk = new OpenAI({baseURL:'https://agents.test/v1',apiKey:principal.apiKey,maxRetries:0,fetch:(input,init)=>mf.dispatchFetch(input,init)});
  const body = path => ({model:agent.id,input:[{role:'user',content:[{type:'input_file',file_url:origin+path,filename:'note.txt',detail:'auto',prompt_cache_breakpoint:{mode:'explicit'}}]}]});
  if (process.env.MAGENTS_REMOTE_FILE_CASE === 'metadata-budget') {
    for (const source of [{file_id:'file_opaque_other_owner'},{filename:'note.txt',file_data:'aGk='}]) {
      const request = {model:agent.id,input:[{role:'user',content:[{type:'input_file',...source,detail:'high',prompt_cache_breakpoint:{mode:'explicit'}}]}]};
      await assert.rejects(sdk.responses.inputTokens.count(request),error=>error.status===422&&error.code==='file_detail_unavailable');
    }
    assert.equal(calls.length,0,'Legal but unsupported visual detail is a capability error before owned lookup or remote fetch');
    text='x'.repeat(256*1024);
    const oversized={model:agent.id,input:[{role:'user',content:Array.from({length:17},()=>({type:'input_file',file_url:origin+'/note'}))}]};
    await assert.rejects(sdk.responses.inputTokens.count(oversized),error=>error.status===413);
    assert.equal(calls.length,16,'The exhausted aggregate budget prevents the seventeenth download');
    assert.equal(providers.state.counts.length,0);
    assert.equal(providers.state.calls.length,0);
    assert.equal(await db.prepare('SELECT COUNT(*) AS n FROM sessions WHERE account_id=?').bind(principal.owner).first('n'),0);
    assert.equal(await db.prepare('SELECT COUNT(*) AS n FROM space_files WHERE account_id=?').bind(principal.owner).first('n'),0);
    return;
  }
  const counted = await sdk.responses.inputTokens.count(body('/note?signature=not-a-provider-key'));
  assert.equal(counted.input_tokens,42);
  assert.equal(providers.state.calls.length,0);
  assert.equal(await db.prepare('SELECT COUNT(*) AS n FROM sessions WHERE account_id=?').bind(principal.owner).first('n'),0,'Count never creates a Session');
  assert.equal(await db.prepare('SELECT COUNT(*) AS n FROM space_files WHERE account_id=?').bind(principal.owner).first('n'),0,'Count never creates a permanent inline File');
  const original = body('/note?signature=not-a-provider-key');
  const made = await sdk.responses.create(original,{headers:{'Idempotency-Key':'remote-file-frozen'}});
  assert.equal(made.status,'completed');
  assert.deepEqual(providers.state.counts[0].input,providers.state.calls[0].input,'Count/create use exactly the same frozen file renderer');
  const payload = JSON.stringify(providers.state.calls[0].input);
  assert.ok(payload.includes(text));
  assert.ok(payload.includes('prompt_cache_breakpoint'));
  assert.ok(!payload.includes('signature')&&!payload.includes(origin),'Remote address is not model content');
  const before = calls.length;
  text='The public file changed after admission';
  assert.equal((await sdk.responses.create(original,{headers:{'Idempotency-Key':'remote-file-frozen'}})).id,made.id);
  assert.equal(calls.length,before,'Repeated response intent cannot download a changed source');
  for (const path of ['/redirect','/private-redirect','/large','/chunked-large','/unavailable']) {
    const beforeModels=providers.state.calls.length;
    await assert.rejects(sdk.responses.inputTokens.count(body(path)),error=>error.status>=400&&error.status<500);
    assert.equal(providers.state.calls.length,beforeModels);
  }
  assert.equal(calls.filter(path=>path==='/target').length,0,'No redirects, including public-to-public');
  const unknown = body('/unavailable');
  await assert.rejects(sdk.responses.create(unknown,{headers:{'Idempotency-Key':'remote-known-rejection'}}),error=>error.status===422);
  const failures=calls.filter(path=>path==='/unavailable').length;
  await assert.rejects(sdk.responses.create(unknown,{headers:{'Idempotency-Key':'remote-known-rejection'}}),error=>error.status===422);
  assert.equal(calls.filter(path=>path==='/unavailable').length,failures,'A fixed rejected preparation never redownloads');
  for (const url of ['https://127.0.0.1/a','https://[::1]/a','https://localhost/a','https://host.internal/a','https://user:secret@public-file-fixture.example.com/a']) {
    const invalid=body('/note');invalid.input[0].content[0].file_url=url;
    const before=calls.length;
    await assert.rejects(sdk.responses.inputTokens.count(invalid),error=>error.status===400);
    assert.equal(calls.length,before);
  }
  const detail=body('/note');detail.input[0].content[0].detail='high';
  const detailBefore=calls.length;
  await assert.rejects(sdk.responses.inputTokens.count(detail),error=>error.status===422&&error.code==='file_detail_unavailable');
  assert.equal(calls.length,detailBefore);
  const foreign={model:agent.id,input:[{role:'user',content:[{type:'input_file',file_id:'file_opaque_other_owner',detail:'auto',prompt_cache_breakpoint:{mode:'explicit'}}]}]};
  await assert.rejects(sdk.responses.inputTokens.count(foreign),error=>error.status===404);
  await db.prepare('UPDATE usage_accounts SET budget_microusd=spent_microusd+reserved_microusd WHERE account_id=?').bind(principal.owner).run();
  const noCredit=calls.length;
  await assert.rejects(sdk.responses.inputTokens.count(body('/note')),error=>error.status===402);
  assert.equal(calls.length,noCredit,'No external file read without prepaid I/O');
  assert.equal(await db.prepare('SELECT COUNT(*) AS n FROM space_files WHERE account_id=?').bind(principal.owner).first('n'),0);
  await writeFile(path.join(directory,'receipt.json'),JSON.stringify({source:process.env.GITHUB_SHA??null,remote_fetches:calls.length,model_calls:providers.state.calls.length,count_create_same_renderer:true,no_secret_forwarding:true,redirects_followed:0,permanent_files:0},null,2));
});
