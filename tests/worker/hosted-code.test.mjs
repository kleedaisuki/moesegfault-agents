import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { createHash, randomBytes } from 'node:crypto';
import { mkdir, mkdtemp, readFile, readdir, writeFile } from 'node:fs/promises';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import OpenAI from 'openai';
import { Miniflare, convertV4MiniflareOptions } from 'miniflare';
import { createIdentityFixture } from './auth-journey.mjs';
import { createBrowserPrincipal } from './browser-principal.mjs';
import { createNativeSemanticsProvider } from './native-semantics-journey.mjs';

const root = fileURLToPath(new URL('../../', import.meta.url));
const build = path.resolve(root, process.env.WORKER_BUILD_DIR ?? 'crates/agents-worker/build');
const hash = bytes => createHash('sha256').update(bytes).digest('hex');
const defaultContextId = `cntr_${'a'.repeat(32)}`;
const artifactId = `pyart_${'b'.repeat(64)}`;
const source = "retained_value = 42\nprint(retained_value)\n";
const artifact = Buffer.from('value\n42\n');
const guestSource = "print(retained_value + 1)\n";

async function migrate(db) {
  for (const name of (await readdir(path.join(root, 'migrations'))).filter(name => name.endsWith('.sql')).sort()) {
    const parsed = spawnSync(process.env.PYTHON ?? 'python', ['-c',
      'import sys,sqlite3,json\ns: str=""; out: list[str]=[]\nfor line in sys.stdin:\n s+=line\n if sqlite3.complete_statement(s): out.append(s); s=""\nassert not s.strip(), s\nprint(json.dumps(out))'],
    { input: await readFile(path.join(root, 'migrations', name), 'utf8'), encoding: 'utf8' });
    assert.equal(parsed.status, 0, name);
    await db.batch(JSON.parse(parsed.stdout).map(statement => db.prepare(statement)));
  }
}

test('compiled Rust Worker: hosted Code Interpreter auto context publication and exact retry', { timeout: 90000 }, t => codeJourney(t, false));
test('compiled Rust Worker: Code container outlives archived harness and explicit release preserves Files', { timeout: 90000 }, t => codeJourney(t, true));
test('compiled Rust Worker: lost Code VM releases capacity while preserving unknown finance and published Files', { timeout: 90000 }, t => codeJourney(t, 'lost'));
test('compiled Rust Worker: owned CID reuses the archived source kernel with fresh guest context and no second VM', { timeout: 90000 }, t => codeJourney(t, 'borrowed'));
test('compiled Rust Worker: late owned-CID use funds its full new twenty-minute idle window', { timeout: 90000 }, t => codeJourney(t, 'late'));
test('compiled Rust Worker: BYOK Code executor uses its own key and real memory without a local VM', { timeout: 90000 }, t => codeJourney(t, 'executor'));
test('compiled Rust Worker: real BYOK executor Python and artifact cross the owned service boundary', { timeout: 90000 }, t => codeJourney(t, 'executor_real'));

async function codeJourney(t, lifecycle) {
  const borrowed=lifecycle==='borrowed'||lifecycle==='late';
  const realExecutor=lifecycle==='executor_real';
  const external=lifecycle==='executor'||realExecutor;
  let contextId=defaultContextId;
  const cellSource=realExecutor?"from pathlib import Path\nretained_value = 42\nPath('result.csv').write_bytes(b'value\\n42\\n')\nprint(retained_value)\n":source;
  if (realExecutor) assert.ok(process.env.MAGENTS_CODE_EXECUTOR_ORIGIN&&process.env.MAGENTS_CODE_EXECUTOR_TOKEN);
  const executorOrigin='https://owned-executor.example.com';
  const executorKey=realExecutor?process.env.MAGENTS_CODE_EXECUTOR_TOKEN:'synthetic-executor-only-key';
  const encryptionKey = randomBytes(32), identity = createIdentityFixture(), providers = createNativeSemanticsProvider();
  const deployment = JSON.parse(await readFile(path.join(root, 'wrangler.jsonc'), 'utf8'));
  await mkdir(path.join(root, '.temp'), { recursive: true });
  const directory = await mkdtemp(path.join(root, '.temp/hosted-code-'));
  process.env.TMP = process.env.TEMP = process.env.TMPDIR = directory;
  const results = new Map();
  const order = [];
  let posts = 0, preparations = 0, acknowledgements = 0, releases = 0;
  let contextLive = false,capabilitiesCalls=0;
  const mf = new Miniflare(convertV4MiniflareOptions({ modulesRoot: build,
    modules: [{ type: 'ESModule', path: path.join(build, 'worker/shim.mjs'),
      contents: await readFile(new URL('./hosted-code-shim.mjs', import.meta.url), 'utf8') },
    { type: 'ESModule', path: path.join(build, 'index.js') }, { type: 'CompiledWasm', path: path.join(build, 'index_bg.wasm') }],
    compatibilityDate: '2026-10-23', compatibilityFlags: ['enable_request_signal'],
    resourcePersistencePath: path.join(directory, 'storage'), d1Databases: ['DB'], r2Buckets: ['WORKSPACES', 'CONVERSATIONS', 'DIAGNOSTICS'],
    durableObjects: { SESSIONS: { className: 'Session', useSQLite: true }, AUTH_SESSIONS: { className: 'AuthSession', useSQLite: true },
      CONVERSATION_STATE: { className: 'ConversationState', useSQLite: true }, MODEL_OPERATIONS: { className: 'ModelOperation', useSQLite: true } },
    ratelimits: Object.fromEntries(['INGRESS', 'ACCOUNT', 'MUTATION'].map((name, index) => [`${name}_RATE_LIMITER`, { namespace_id: String(index + 1), simple: { limit: 1000, period: 60 } }])),
    bindings: { ENVIRONMENT: 'test', APP_ORIGIN: 'https://agents.test', ALLOWED_ORIGIN: 'https://agents.test',
      DATA_ENCRYPTION_KEY: encryptionKey.toString('base64url'), IDENTITY_ISSUER: 'https://identity.test',
      IDENTITY_WEB_CLIENT_ID: 'test-dashboard', IDENTITY_NATIVE_CLIENT_ID: 'test-native', IDENTITY_REDIRECT_URI: 'https://agents.test/auth/callback',
      IDENTITY_NATIVE_REDIRECT_URI: 'http://127.0.0.1/callback', IDENTITY_CLIENT_PRIVATE_JWK: JSON.stringify(identity.privateJwk),
      BILLING_PRODUCT_ID: 'agents', BILLING_MODE: 'test', MODEL_REGISTRY_JSON: '[]', MODEL_PRICING_JSON: '{}',
      HOSTED_TOOL_RUNTIMES: 'code_interpreter', HOSTED_TOOL_CAPABILITIES_JSON: JSON.stringify({ version: 1,
        platform_images: false, platform_search: false, file_search: false, container_memory: null,
        available_container_profiles: ['1g'], code_network_disabled: true, code_network_allowlist: false }),
      RESOURCE_PRICING_JSON: deployment.env.staging.vars.RESOURCE_PRICING_JSON,
      RUN_RESOURCE_RESERVATION_MICROUSD: '20000', FREE_GRANT_BUDGET_MICROUSD_PER_DAY: '100000000' },
    async outboundService(request) {
      const url = new URL(request.url);
      if (url.hostname !== 'code-supervisor.fixture' && url.origin!==executorOrigin) {
        const response = await providers.handle(request);
        assert.notEqual(response, undefined, `Unexpected fixture origin ${url.origin}`);
        return response;
      }
      if (realExecutor) {
        assert.equal(url.origin,executorOrigin,'a real executor path must never allocate or call the local supervisor');
        assert.equal(request.headers.get('authorization'),`Bearer ${executorKey}`);
        order.push(`${request.method} ${url.pathname}`);
        if (url.pathname==='/v1/python/capabilities') capabilitiesCalls++;
        if (request.method==='POST'&&url.pathname==='/v1/python') posts++;
        if (request.method==='DELETE'&&url.pathname.startsWith('/v1/python/artifacts/')) acknowledgements++;
        if (request.method==='DELETE'&&url.pathname.startsWith('/v1/python/contexts/')) releases++;
        const body=['GET','HEAD'].includes(request.method)?undefined:new Uint8Array(await request.arrayBuffer());
        const forwarded=await fetch(`${process.env.MAGENTS_CODE_EXECUTOR_ORIGIN}${url.pathname}${url.search}`,{method:request.method,headers:request.headers,body,redirect:'manual'});
        if (request.method==='POST'&&url.pathname==='/v1/python/contexts'&&forwarded.status===200) {
          preparations++;contextId=(await forwarded.clone().json()).container_id;
        }
        return forwarded;
      }
      if (external) assert.equal(request.headers.get('authorization'),`Bearer ${executorKey}`);
      else assert.ok(request.headers.get('authorization')?.startsWith('Bearer '));
      order.push(`${request.method} ${url.pathname}`);
      if (url.pathname === '/v1/python/capabilities') {
        capabilitiesCalls++;
        return Response.json({protocol:'workspace-daemon',context_scope:'instance',version:1,memory_limits:['4g'],network_disabled:true,network_allowlist:false,max_execution_ms:300000,max_artifact_bytes:16*1024*1024,atomic_boot_header:true});
      }
      if (url.pathname === '/health') return Response.json({ boot_id: 'supervisor-known-boot' });
      if (url.pathname === '/v1/status') return Response.json({ running: false, dirty_bytes: 0 });
      if (url.pathname === `/v1/python/contexts/${contextId}`) {
        if (request.method === 'DELETE') { contextLive = false; releases++; return new Response(null, { status: 204 }); }
        return contextLive ? Response.json({ container_id: contextId, status: 'running' }) : Response.json({ error: 'not found' }, { status: 404 });
      }
      if (url.pathname === '/v1/python/contexts') {
        if (request.method === 'GET') return contextLive ? Response.json({ container_id: contextId }) : new Response(null, { status: 404 });
        const body = await request.json();
        assert.equal(body.memory_limit, external?'4g':'1g');
        assert.deepEqual(body.network_policy, { type: 'disabled' });
        assert.equal(body.container_id, null);
        preparations++; contextLive = true;
        return Response.json({ container_id: contextId },{headers:{'X-Code-Boot-Id':'supervisor-known-boot'}});
      }
      if (url.pathname === '/v1/python' && request.method === 'POST') {
        const body = await request.json();
        const borrowedCell = borrowed && body.code === guestSource;
        assert.ok(body.code === cellSource || borrowedCell);
        assert.equal(body.container_id, contextId);
        assert.equal(body.memory_limit, external?'4g':'1g');
        assert.ok(!results.has(body.id), 'No accepted cell may be POSTed twice');
        posts++;
        const result = { id: body.id, container_id: contextId, outputs: [{ type: 'logs', logs: borrowedCell ? '43\n' : '42\n' }],
          artifacts: borrowedCell ? [] : [{ id: artifactId, filename: 'result.csv', path: 'result.csv', bytes: artifact.length,
            sha256: hash(artifact), mime_type: 'text/csv' }], artifact_error: null, error: null,
          python_error: false, timed_out: false, cancelled: false, truncated: false, duration_ms: 5 };
        results.set(body.id, result);
        return Response.json(result);
      }
      if (url.pathname === `/v1/python/artifacts/${artifactId}`) {
        if (request.method === 'DELETE') {
          acknowledgements++;
          return new Response(null, { status: 204 });
        }
        return new Response(artifact, { headers: { 'Content-Type': 'text/csv',
          'Content-Length': String(artifact.length), 'X-Content-Sha256': hash(artifact) } });
      }
      if (url.pathname.startsWith('/v1/python/')) {
        const result = results.get(url.pathname.split('/').at(-1));
        return result ? Response.json(result) : Response.json({ error: 'not found' }, { status: 404 });
      }
      throw Error(`Unexpected supervisor request ${request.method} ${url.pathname}`);
    } }));
  t.after(() => mf.dispose());
  const db = await mf.getD1Database('DB'); await migrate(db);
  const principal = await createBrowserPrincipal({ mf, db, encryptionKey, owner: 'hosted-code-auto' });
  const model = providers.register('hosted-code-auto', [
    { output: [{ id: 'native-code', type: 'function_call', name: '__agents_code_interpreter', call_id: 'code-call',
      arguments: JSON.stringify({ code: cellSource }), status: 'completed' }] },
    { output: [{ id: 'native-answer', type: 'message', role: 'assistant', status: 'completed',
      content: [{ type: 'output_text', text: '42', annotations: [], logprobs: [] }] }] },
  ]);
  const key = await principal.browser('POST', '/v1/provider-keys', { protocol: 'openai', base_url: model.baseUrl, api_key: model.key }, 201);
  const executorCredential=external?await principal.browser('POST','/v1/provider-keys',{protocol:'openai',base_url:executorOrigin,api_key:executorKey},201):null;
  const client = new OpenAI({ apiKey: principal.apiKey, baseURL: 'https://agents.test/v1', maxRetries: 0,
    fetch: (input, init) => mf.dispatchFetch(input, init) });
  const agentResponse = await mf.dispatchFetch('https://agents.test/v1/agents', { method: 'POST',
    headers: { Authorization: `Bearer ${principal.apiKey}`, 'Content-Type': 'application/json' },
    body: JSON.stringify({ name: 'Code auto publication', instructions: 'SOURCE_PIN_MARKER', workspace: false, tools: [],
      ...(external?{tool_services:{code_interpreter:{provider:'workspace_daemon',credential_id:executorCredential.id}}}:{}),
      model: { provider: 'byok', credential_id: key.id, protocol: 'responses', model: model.model, context_window: 32768, max_output_tokens: 512 } }) });
  assert.equal(agentResponse.status, 201);
  const agent = await agentResponse.json();
  const input = { model: agent.id, input: 'Compute and publish CSV.', include: ['code_interpreter_call.outputs'],
    tools: [{ type: 'code_interpreter', container: { type: 'auto',...(external?{memory_limit:'4g'}:{}) } }] };
  const diagnostics = setTimeout(async () => {
    try {
      const route = await db.prepare('SELECT session_id FROM response_routes WHERE account_id=? ORDER BY created_at DESC LIMIT 1').bind(principal.owner).first();
      const namespace = await mf.getDurableObjectNamespace('SESSIONS');
      const actor = namespace.get(namespace.idFromName(`${principal.owner}:${route.session_id}`));
      const state = await (await actor.fetch('https://session.internal/__test/code-inspect')).json();
      console.error(JSON.stringify({ code_fixture_diagnostic: { posts, preparations, acknowledgements, order, state } }));
    } catch (error) { console.error(JSON.stringify({ code_fixture_diagnostic: { failure: error.name, posts, preparations, order } })); }
  }, 4000);
  t.after(() => clearTimeout(diagnostics));
  const events = [];
  for await (const event of await client.responses.create({ ...input, stream: true }, { headers: { 'Idempotency-Key': 'known-code-auto' } })) events.push(event);
  const response = events.find(event => event.type === 'response.completed')?.response;
  clearTimeout(diagnostics);
  assert.ok(response, 'Code must reach a genuine completed response');
  const item = response.output.find(item => item.type === 'code_interpreter_call');
  assert.ok(item, 'A completed model reply is not evidence that hosted Python executed');
  assert.equal(item.container_id, contextId);
  assert.equal(item.code, cellSource);
  assert.deepEqual(item.outputs, [{ type: 'logs', logs: '42\n' }]);
  const phases = events.filter(event => event.item_id === item.id).map(event => event.type);
  assert.deepEqual(phases, ['response.code_interpreter_call.in_progress', 'response.code_interpreter_call_code.delta',
    'response.code_interpreter_call_code.done', 'response.code_interpreter_call.interpreting', 'response.code_interpreter_call.completed']);
  assert.equal(events.find(event => event.type === 'response.code_interpreter_call_code.done').code, cellSource);
  assert.equal(model.calls, 2); assert.equal(posts, 1); assert.equal(preparations, 1); assert.equal(acknowledgements, 1);
  const files = await client.files.list();
  const file = files.data.find(file => file.filename === 'result.csv');
  assert.ok(file);
  assert.equal(hash(Buffer.from(await (await client.files.content(file.id)).arrayBuffer())), hash(artifact));
  if (lifecycle) {
    const route = await db.prepare('SELECT session_id FROM response_routes WHERE account_id=? AND id=?').bind(principal.owner, response.id).first();
    assert.ok(route?.session_id);
    let finished = false;
    for (let attempt = 0; attempt < 200; attempt++) {
      const observed = await mf.dispatchFetch(`https://agents.test/v1/sessions/${route.session_id}`, { headers: { Authorization: `Bearer ${principal.apiKey}` } });
      assert.equal(observed.status,200);
      const session = await observed.json();
      if (session.status === 'idle' && !session.run_id) { finished = true; break; }
      await new Promise(resolve => setTimeout(resolve,25));
    }
    assert.ok(finished,'canonical completion must finish financial/harness bookkeeping before archival');
    const namespace = await mf.getDurableObjectNamespace('SESSIONS');
    const actor = namespace.get(namespace.idFromName(`${principal.owner}:${route.session_id}`));
    let lostLease = null;
    if (external) {
      const execution=await(await actor.fetch('https://session.internal/__test/code-inspect')).json();
      assert.equal(execution.running,false);assert.equal(execution.lease_present,false);
      const allocated=await db.prepare('SELECT executor_kind,memory_limit,source_profile,cleanup_run_id FROM code_contexts WHERE account_id=? AND container_id=?').bind(principal.owner,contextId).first();
      assert.equal(allocated.executor_kind,'workspace_daemon');assert.equal(allocated.memory_limit,'4g');assert.equal(allocated.source_profile,'basic1g');assert.ok(allocated.cleanup_run_id);
    }

    if (lifecycle === 'lost') {
      const dropped = await (await actor.fetch('https://session.internal/__test/code-drop')).json();
      assert.ok(dropped.lease_id);
      lostLease = dropped.lease_id;
      assert.equal((await client.containers.retrieve(contextId)).status, 'expired');
    } else {
      const archive = await mf.dispatchFetch(`https://agents.test/v1/sessions/${route.session_id}`, { method: 'DELETE', headers: { Authorization: `Bearer ${principal.apiKey}` } });
      assert.equal(archive.status, 200, await archive.clone().text());
      assert.deepEqual((await archive.json()).retained_code_containers, [contextId]);
      const retained = await client.containers.retrieve(contextId);
      assert.equal(retained.status, 'running');
      if (external) assert.equal(retained.memory_limit,'4g');
    }
    if (borrowed) {
      if (lifecycle==='late') await actor.fetch('https://session.internal/__test/code-funding-window');
      const originalLease=await(await actor.fetch('https://session.internal/__test/code-inspect')).json();
      const before = await (await mf.dispatchFetch(`https://agents.test/v1/sessions/${route.session_id}`, { headers: { Authorization: `Bearer ${principal.apiKey}` } })).json();
      assert.equal(before.status,'closed');
      const guest = providers.register('hosted-code-guest', [
        { output: [{ id:'guest-code',type:'function_call',name:'__agents_code_interpreter',call_id:'guest-call',arguments:JSON.stringify({code:guestSource}),status:'completed' }] },
        { output: [{ id:'guest-answer',type:'message',role:'assistant',status:'completed',content:[{type:'output_text',text:'43',annotations:[],logprobs:[]}] }] },
      ]);
      const guestKey = await principal.browser('POST','/v1/provider-keys',{protocol:'openai',base_url:guest.baseUrl,api_key:guest.key},201);
      const created = await mf.dispatchFetch('https://agents.test/v1/agents',{method:'POST',headers:{Authorization:`Bearer ${principal.apiKey}`,'Content-Type':'application/json'},body:JSON.stringify({
        name:'Fresh guest policy',instructions:'GUEST_PIN_MARKER',workspace:false,tools:[],model:{provider:'byok',credential_id:guestKey.id,protocol:'responses',model:guest.model,context_window:32768,max_output_tokens:512},
      })});
      assert.equal(created.status,201,await created.clone().text());
      const guestAgent=await created.json();
      const guestResponse=await client.responses.create({model:guestAgent.id,input:'Use the existing kernel without importing another conversation.',include:['code_interpreter_call.outputs'],tools:[{type:'code_interpreter',container:contextId}]});
      const guestItem=guestResponse.output.find(item=>item.type==='code_interpreter_call');
      assert.equal(guestResponse.status,'completed');
      assert.equal(guestItem?.container_id,contextId);
      assert.deepEqual(guestItem.outputs,[{type:'logs',logs:'43\n'}]);
      assert.equal(guest.calls,2);
      assert.equal(model.calls,2,'the source harness must never resume its model');
      assert.ok(JSON.stringify(guest.requests).includes('GUEST_PIN_MARKER'));
      assert.ok(!JSON.stringify(guest.requests).includes('SOURCE_PIN_MARKER'));
      const guestRoute=await db.prepare('SELECT session_id FROM response_routes WHERE account_id=? AND id=?').bind(principal.owner,guestResponse.id).first();
      assert.notEqual(guestRoute.session_id,route.session_id);
      const guestActor=namespace.get(namespace.idFromName(`${principal.owner}:${guestRoute.session_id}`));
      const guestState=await (await guestActor.fetch('https://session.internal/__test/code-inspect')).json();
      assert.equal(guestState.running,false);
      assert.equal(guestState.lease_present,false);
      const sourceState=await (await actor.fetch('https://session.internal/__test/code-inspect')).json();
      assert.equal(sourceState.borrowed_receipts,1);
      assert.equal(sourceState.published_borrowed_receipts,1);
      assert.equal(sourceState.borrowed_outcome_bytes,0,'the guest owns the published result; the source retains only its replay fence');
      if (lifecycle==='late') {
        const current=await db.prepare('SELECT expires_at_ms FROM code_contexts WHERE account_id=? AND container_id=?').bind(principal.owner,contextId).first();
        assert.ok(sourceState.lease_expires_ms>=current.expires_at_ms,'Code cannot promise20minutes while its physical funding expires earlier');
        assert.notEqual(sourceState.lease_id,originalLease.lease_id);
        const prior=await db.prepare('SELECT state FROM usage_reservations WHERE account_id=? AND id=?').bind(principal.owner,originalLease.lease_id).first();
        assert.equal(prior.state,'settled','lease transfer must close the old physical interval, not bill the VM twice');
      }

      assert.equal((await db.prepare('SELECT COUNT(*) AS n FROM code_contexts WHERE account_id=?').bind(principal.owner).first()).n,1);
      const after=await (await mf.dispatchFetch(`https://agents.test/v1/sessions/${route.session_id}`,{headers:{Authorization:`Bearer ${principal.apiKey}`}})).json();
      assert.equal(after.status,'closed');
      assert.equal(after.last_input_at_ms,before.last_input_at_ms);
      assert.equal(posts,2);assert.equal(preparations,1);
      if (lifecycle==='borrowed') {
      const outsider=await createBrowserPrincipal({mf,db,encryptionKey,owner:'hosted-code-outsider'});
      const forbidden=providers.register('hosted-code-forbidden',[]);
      const forbiddenKey=await outsider.browser('POST','/v1/provider-keys',{protocol:'openai',base_url:forbidden.baseUrl,api_key:forbidden.key},201);
      const other=await outsider.browser('POST','/v1/agents',{name:'Other owner',workspace:false,tools:[],model:{provider:'byok',credential_id:forbiddenKey.id,protocol:'responses',model:forbidden.model,context_window:32768,max_output_tokens:512}},201);
      const otherClient=new OpenAI({apiKey:outsider.apiKey,baseURL:'https://agents.test/v1',maxRetries:0,fetch:(input,init)=>mf.dispatchFetch(input,init)});
      await assert.rejects(otherClient.responses.create({model:other.id,input:'Use this foreign code container.',tools:[{type:'code_interpreter',container:contextId}]}),error=>error.status===404&&error.code==='code_container_not_found');
      assert.equal(forbidden.calls,0,'foreign CID ownership must fail before any outer inference');
      assert.equal(posts,2);assert.equal(preparations,1);
      }
    }
    assert.equal(releases, 0);
    await client.containers.delete(contextId);
    assert.equal(releases, lifecycle === 'lost' ? 0 : 1);
    await assert.rejects(client.containers.retrieve(contextId), error => error.status === 404);
    assert.equal(hash(Buffer.from(await (await client.files.content(file.id)).arrayBuffer())), hash(artifact));
    const allocation = await db.prepare('SELECT state, allocation_released_at_ms FROM code_contexts WHERE account_id=? AND container_id=?').bind(principal.owner, contextId).first();
    assert.equal(allocation.state, 'deleted');
    assert.ok(allocation.allocation_released_at_ms > 0);
    const state = await (await actor.fetch('https://session.internal/__test/code-inspect')).json();
    assert.equal(state.running, false, 'the last resource interest must actually stop the physical VM');
    assert.equal(state.lease_present, false, 'physical release must detach its funded lease');
    if (external) {
      const cleanup=await db.prepare('SELECT r.state FROM usage_reservations r JOIN code_contexts c ON c.cleanup_run_id=r.id WHERE c.account_id=? AND c.container_id=?').bind(principal.owner,contextId).first();
      assert.equal(cleanup.state,'settled');
      const before=capabilitiesCalls;
      await assert.rejects(client.responses.create({model:agent.id,input:'Use larger memory.',tools:[{type:'code_interpreter',container:{type:'auto',memory_limit:'16g'}}]}),error=>error.status===400);
      assert.ok(capabilitiesCalls>before,'executor capabilities, not the CF1g SKU gate, must reject unsupported16g');
      assert.equal(model.calls,2);assert.equal(posts,1);
    }

    if (lostLease) {
      const reservation = await db.prepare('SELECT state FROM usage_reservations WHERE account_id=? AND id=?').bind(principal.owner, lostLease).first();
      assert.equal(reservation.state, 'reserved', 'an unknown exit time is not a bill or a refund');
      const unknown = await db.prepare("SELECT status,quantity FROM resource_observations WHERE account_id=? AND run_id=? AND event_key='container:unexpected_exit' AND resource='container' AND unit='wall_ms'").bind(principal.owner,lostLease).first();
      assert.equal(unknown.status, 'pending');
      assert.equal(unknown.quantity, null);
      const archived = await mf.dispatchFetch(`https://agents.test/v1/sessions/${route.session_id}`, { method: 'DELETE', headers: { Authorization: `Bearer ${principal.apiKey}` } });
      assert.equal(archived.status,200,await archived.clone().text());
      assert.equal((await archived.json()).retained_code_containers,undefined);
    }

  }
  const replayEvents = [];
  for await (const event of await client.responses.create({ ...input, stream: true }, { headers: { 'Idempotency-Key': 'known-code-auto' } })) replayEvents.push(event);
  assert.equal(replayEvents.find(event => event.type === 'response.completed')?.response.id, response.id);
  assert.deepEqual(replayEvents, events);
  assert.equal(posts, borrowed ? 2 : 1); assert.equal(preparations, 1); assert.equal(acknowledgements, 1); assert.equal(model.calls, 2);
  const getAt = order.findIndex(value=>value.startsWith('GET /v1/python/artifacts/')), ackAt = order.findIndex(value=>value.startsWith('DELETE /v1/python/artifacts/'));
  assert.ok(getAt >= 0 && ackAt > getAt);
  if (process.env.MAGENTS_CURRENT_SCHEMA_PYTHON) {
    const checked = spawnSync(process.env.MAGENTS_CURRENT_SCHEMA_PYTHON, ['tests/e2e/current_responses_sdk.py', 'response-wire', '--input', '-'],
      { cwd: root, input: JSON.stringify({ response, events }), encoding: 'utf8', maxBuffer: 1024 * 1024 });
    assert.equal(checked.status, 0, checked.stdout || checked.stderr);
  }
  await writeFile(path.join(directory, 'receipt.json'), JSON.stringify({ source: process.env.MAGENTS_TEST_EXPECTED_BUILD ?? null,
    wasm_sha256: hash(await readFile(path.join(build, 'index_bg.wasm'))), fixture: realExecutor?'real-rust-worker-real-docker-executor-controlled-origin-bridge':'real-rust-worker-controlled-supervisor',
    real_python: realExecutor, external_network_calls: 0, model_calls: model.calls, code_posts: posts,
    preparations, acknowledgements, releases, lifecycle, artifact_bytes: artifact.length, phases }, null, 2));
}
