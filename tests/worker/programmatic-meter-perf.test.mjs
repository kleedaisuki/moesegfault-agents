import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { mkdir, mkdtemp, readFile, readdir, writeFile } from 'node:fs/promises';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import { Miniflare, convertV4MiniflareOptions } from 'miniflare';
import { createIdentityFixture } from './auth-journey.mjs';
import { createNativeSemanticsProvider } from './native-semantics-journey.mjs';
import OpenAI from 'openai';
import {createBrowserPrincipal} from './browser-principal.mjs';
import {collectNativeEvents} from './native-semantics-journey.mjs';

const root = fileURLToPath(new URL('../../', import.meta.url));
const current = path.resolve(root, process.env.WORKER_BUILD_DIR ?? 'crates/agents-worker/build');

async function measure(t,build,label){
  const identity = createIdentityFixture();
  const encryptionKey = randomBytes(32);
  const providers = createNativeSemanticsProvider();
  const deployment = JSON.parse(await readFile(path.join(root, 'wrangler.jsonc'), 'utf8'));
  const pricing = JSON.parse(deployment.env.staging.vars.RESOURCE_PRICING_JSON);
  for (const [unit, numerator, denominator] of [
    ['dynamic_worker_creation_day', 2000, 1], ['dynamic_worker_request', 3, 10], ['dynamic_worker_cpu_slot_100ms', 2, 1],
  ]) pricing.tariffs.push({ resource: 'worker', unit, microusd_numerator: numerator,
    quantity_denominator: denominator, price_version: pricing.version });
  await mkdir(path.join(root, '.temp'), { recursive: true });
  const directory = await mkdtemp(path.join(root, '.temp/programmatic-actor-'));
  const options = convertV4MiniflareOptions({
    modulesRoot: build, modules: [
      { type: 'ESModule', path: path.join(build, 'worker/shim.mjs'),contents:await readFile(new URL('./programmatic-meter-perf-shim.mjs',import.meta.url),'utf8') },
      { type: 'ESModule', path: path.join(build, 'index.js') },
      { type: 'CompiledWasm', path: path.join(build, 'index_bg.wasm') },
    ], compatibilityDate: '2026-10-23', compatibilityFlags: ['enable_request_signal'],
    workerLoaders: { PROGRAM_LOADER: {} },
    resourcePersistencePath: path.join(directory, 'storage'), d1Databases: ['DB'],
    r2Buckets: ['WORKSPACES', 'CONVERSATIONS', 'DIAGNOSTICS'],
    durableObjects: {
      SESSIONS: { className: 'Session', useSQLite: true }, SESSION_4G: { className: 'Session4G', useSQLite: true },
      AUTH_SESSIONS: { className: 'AuthSession', useSQLite: true },
      CONVERSATION_STATE: { className: 'ConversationState', useSQLite: true }, MODEL_OPERATIONS: { className: 'ModelOperation', useSQLite: true },
    }, ratelimits: Object.fromEntries(['INGRESS', 'ACCOUNT', 'MUTATION'].map((name, index) => [
      `${name}_RATE_LIMITER`, { namespace_id: String(index + 1), simple: { limit: 1000, period: 60 } },
    ])), bindings: {
      ENVIRONMENT: 'test', APP_ORIGIN: 'https://agents.test', ALLOWED_ORIGIN: 'https://agents.test',
      DATA_ENCRYPTION_KEY: encryptionKey.toString('base64url'),
      IDENTITY_ISSUER: 'https://identity.test', IDENTITY_WEB_CLIENT_ID: 'test-dashboard', IDENTITY_NATIVE_CLIENT_ID: 'test-native',
      IDENTITY_REDIRECT_URI: 'https://agents.test/auth/callback', IDENTITY_NATIVE_REDIRECT_URI: 'http://127.0.0.1/callback',
      IDENTITY_CLIENT_PRIVATE_JWK: JSON.stringify(identity.privateJwk), BILLING_PRODUCT_ID: 'agents', BILLING_MODE: 'test',
      MODEL_REGISTRY_JSON: '[]', MODEL_PRICING_JSON: '{}', RESOURCE_PRICING_JSON: JSON.stringify(pricing),
      HOSTED_TOOL_RUNTIMES: 'programmatic_tool_calling,mcp',
      RUN_RESOURCE_RESERVATION_MICROUSD: '20000', FREE_GRANT_BUDGET_MICROUSD_PER_DAY: '100000000',
    }, async outboundService(request) {
      const response = await providers.handle(request);
      assert.notEqual(response, undefined, `No unplanned external access: ${new URL(request.url).origin}`);
      return response;
    },
  });
  const mf = new Miniflare(options);
  t.after(() => mf.dispose());
  const db = await mf.getD1Database('DB');
  for (const name of (await readdir(path.join(root, 'migrations'))).filter(name => name.endsWith('.sql')).sort()) {
    const parsed = spawnSync(process.env.PYTHON ?? 'python', ['-c',
      'import sys,sqlite3,json\ns: str=""; out: list[str]=[]\nfor line in sys.stdin:\n s+=line\n if sqlite3.complete_statement(s): out.append(s); s=""\nassert not s.strip(), s\nprint(json.dumps(out))'],
    { input: await readFile(path.join(root, 'migrations', name), 'utf8'), encoding: 'utf8' });
    assert.equal(parsed.status, 0, `${name}: ${parsed.stderr}`);
    await db.batch(JSON.parse(parsed.stdout).map(statement => db.prepare(statement)));
  }
  async function request(method, endpoint, body, expected = 200, bearer) {
    const response = await mf.dispatchFetch(`https://agents.test${endpoint}`, { method,
      headers: { Authorization: `Bearer ${bearer}`, 'Content-Type': 'application/json', 'CF-Connecting-IP': '192.0.2.74' },
      ...(body === undefined ? {} : { body: JSON.stringify(body) }) });
    const text = await response.text();
    assert.equal(response.status, expected, `${method} ${endpoint}: ${text}`);
    return JSON.parse(text);
  }
  const principal=await createBrowserPrincipal({mf,db,encryptionKey,owner:'programmatic-meter-perf'});
  const code='const first=await tools.time({}); const second=await tools.time({}); text(JSON.stringify({gap_ms:second.unix_ms-first.unix_ms,stamps:[first.unix_ms,second.unix_ms]}));';
  const state=providers.register('meter-performance',[
    {output:[{type:'function_call',id:'meter-native-program',call_id:'meter-native-call',name:'__agents_program',arguments:JSON.stringify({code}),status:'completed'}]},
    {output:[{type:'message',id:'meter-final',role:'assistant',status:'completed',content:[{type:'output_text',text:'Metering benchmark completed.',annotations:[],logprobs:[]}]}]},
  ]);
  const credential=await principal.browser('POST','/v1/provider-keys',{protocol:'openai',base_url:state.baseUrl,api_key:state.key},201);
  const agent=await request('POST','/v1/agents',{name:'Metering performance',instructions:'Use the program interface.',workspace:false,tools:['time'],compaction_percent:80,
    model:{provider:'byok',credential_id:credential.id,protocol:'responses',model:state.model,context_window:32768,max_output_tokens:512}},201,principal.apiKey);
  const sdk=new OpenAI({apiKey:principal.apiKey,baseURL:'https://agents.test/v1',maxRetries:0,timeout:100000,fetch:(input,init)=>mf.dispatchFetch(input,init)});
  await mf.dispatchFetch('https://agents.test/__meter/arm',{method:'POST'});
  const started=performance.now();
  const wire=await sdk.responses.create({model:agent.id,input:'Measure two sequential harness times in one native program.',tools:[{type:'programmatic_tool_calling'}],max_tool_calls:2,stream:true},{headers:{'Idempotency-Key':'meter-performance-once'}}).asResponse();
  assert.equal(wire.status,200,wire.status===200?'':await wire.text());
  const events=await collectNativeEvents(wire);
  if(state.failure)throw state.failure;
  const value=events.findLast(item=>item.type==='response.completed')?.response;
  assert.ok(value,JSON.stringify(events.findLast(item=>item.type==='response.failed')?.response?.error));
  const elapsed=performance.now()-started;
  const output=value.output.find(item=>item.type==='program_output');
  assert.equal(output?.status,'completed',output?.result);
  const clocks=JSON.parse(output.result);
  assert.equal(clocks.stamps.length,2);assert.equal(clocks.gap_ms,clocks.stamps[1]-clocks.stamps[0]);
  assert.equal(value.output.filter(item=>item.type==='function_call_output').length,2);
  assert.deepEqual(await(await sdk.responses.retrieve(value.id).asResponse()).json(),value);
  assert.equal(state.calls,2,'Exactly two controlled model turns; no external inference or replay');
  const limit=Date.now()+15000;
  let pending,held;
  do{
    pending=(await db.prepare("SELECT COUNT(*) AS n FROM resource_observations WHERE account_id=? AND status='pending'").bind(principal.owner).first()).n;
    held=(await db.prepare("SELECT COUNT(*) AS n FROM usage_reservations WHERE account_id=? AND state='reserved'").bind(principal.owner).first()).n;
    if(pending||held)await new Promise(resolve=>setTimeout(resolve,25));
  }while((pending||held)&&Date.now()<limit);
  assert.equal(pending,0);assert.equal(held,0);
  const dynamic=(await db.prepare("SELECT l.unit,SUM(l.quantity) quantity,SUM(l.amount_microusd) amount FROM usage_ledger l JOIN usage_reservations r ON r.id=l.reservation_id WHERE r.account_id=? AND l.unit LIKE 'dynamic_worker_%' GROUP BY l.unit ORDER BY l.unit").bind(principal.owner).all()).results;
  assert.deepEqual(dynamic,[{unit:'dynamic_worker_cpu_slot_100ms',quantity:1,amount:2},{unit:'dynamic_worker_creation_day',quantity:1,amount:2000},{unit:'dynamic_worker_request',quantity:1,amount:1}]);
  const d1=(await db.prepare("SELECT l.unit,SUM(l.quantity) quantity,SUM(l.amount_microusd) amount FROM usage_ledger l JOIN usage_reservations r ON r.id=l.reservation_id WHERE r.account_id=? AND l.resource='d1' GROUP BY l.unit ORDER BY l.unit").bind(principal.owner).all()).results;
  return{label,elapsed_ms:elapsed,tool_gap_ms:clocks.gap_ms,...await(await mf.dispatchFetch('https://agents.test/__meter/status')).json(),dynamic,d1,model_turns:state.calls,external_model_calls:0,pending,held};
}

test('compiled Rust Worker: central D1 batching shortens native program tool gaps with unchanged actual quantities and fees',{timeout:150000},async t=>{
  const baseline=await measure(t,path.resolve(root,process.env.MAGENTS_BASELINE_WORKER_DIR),'accepted-files-batched');
  const optimized=await measure(t,current,'central-default');
  assert.deepEqual(optimized.dynamic,baseline.dynamic);
  assert.deepEqual(optimized.d1,baseline.d1,'Exact same actual D1 row quantities and charges');
  assert.equal(optimized.businessStatements,baseline.businessStatements,'Same actual Session projections and business SQL');
  assert.ok(optimized.rpcs<baseline.rpcs,'Eliminate repeated accounting round trips, not durable publication');
  assert.ok(optimized.tool_gap_ms<baseline.tool_gap_ms,'Sequential time calls become closer at equivalent fixed RTT');
  await mkdir(path.join(root,'.cache/verification'),{recursive:true});
  await writeFile(path.join(root,'.cache/verification/programmatic-meter-perf.json'),JSON.stringify({baseline,optimized,scope:'actual Rust Session and native DynamicWorker, fixed20msDBRTT, no external model; not a staging SLO'},null,2)+'\n');
  console.log(JSON.stringify({baseline,optimized}));
});
