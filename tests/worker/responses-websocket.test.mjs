import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { createHash, randomBytes } from 'node:crypto';
import { mkdir, mkdtemp, readFile, readdir, writeFile } from 'node:fs/promises';
import path from 'node:path';
import test from 'node:test';
import OpenAI from 'openai';
import { ResponsesWS } from 'openai/resources/responses/ws';
import { fileURLToPath } from 'node:url';
import { Miniflare, convertV4MiniflareOptions } from 'miniflare';
import { createIdentityFixture } from './auth-journey.mjs';
import { createResponseJourneyFixture, createResponsesProviderFixture } from './responses-journey.mjs';

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


test('compiled Rust Worker: authenticated Responses WebSocket journeys', { timeout: 120000 }, async t => {
  const encryptionKey = randomBytes(32);
  const identity = createIdentityFixture();
  const providers = createResponsesProviderFixture();
  const deployment = JSON.parse(await readFile(path.join(root, 'wrangler.jsonc'), 'utf8'));
  await mkdir(path.join(root, '.temp'), { recursive: true });
  const directory = await mkdtemp(path.join(root, '.temp/responses-websocket-'));
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
      const response = await providers.handle(request);
      assert.notEqual(response, undefined, 'Branch fixtures must never access external services');
      return response;
    },
  }));
  t.after(() => mf.dispose());
  const db = await mf.getD1Database('DB');
  await migrate(db);
  async function request(method, endpoint, body, expected = 200, bearer) {
    const response = await mf.dispatchFetch('https://agents.test' + endpoint, {
      method, headers: { Authorization: `Bearer ${bearer}`, 'Content-Type': 'application/json', 'CF-Connecting-IP': '192.0.2.69' },
      ...(body === undefined ? {} : { body: JSON.stringify(body) }),
    });
    const text = await response.text();
    assert.equal(response.status, expected, `${method} ${endpoint}: ${text}`);
    return JSON.parse(text);
  }
  const context = { mf, db, encryptionKey, request, providers, diagnostic: message => t.diagnostic(message) };
  const cases = await selectedCases(t);
  await runSocketJourneys(context, cases);
  await cases.test('current official Node SDK uses the real compiled Worker network upgrade', () => runOfficialSdk(context));
  await cases.test('accepted steering survives disconnect without source or financial replay', () => runAcceptedSteeringDisconnect(context));
  await cases.test('steering crosses a safe server-tool boundary into an automatic successor', () => runSteeringSuccessor(context));
  await cases.test('steering waits for explicit client results without repeating accepted input', () => runSteeringPendingInput(context));
  await cases.test('sixteen parallel actors, queued seventeenth lane and same-lane FIFO', () => runLaneConcurrency(context));
  await cases.test('account concurrency stays enforced and disconnect does not cancel work', () => runPlanConcurrency(context));
});


async function connect(mf, headers) {
  const response = await mf.dispatchFetch('https://agents.test/v1/responses', {
    headers: { Upgrade: 'websocket', Connection: 'Upgrade', ...headers },
  });
  assert.equal(response.status, 101, 'Actual Worker must perform the public authenticated upgrade');
  const socket = response.webSocket;
  assert.ok(socket);
  const buffered = [];
  const waiters = [];
  socket.addEventListener('message', message => {
    const value = JSON.parse(message.data);
    const waiter = waiters.shift();
    if (waiter) waiter(value); else buffered.push(value);
  });
  socket.accept();
  async function next() {
    if (buffered.length) return buffered.shift();
    let timer;
    let resolve;
    const message = new Promise(done => { resolve = done; waiters.push(done); });
    try {
      return await Promise.race([message, new Promise((_, reject) => {
        timer = setTimeout(() => reject(new Error('Compiled WebSocket observation exceeded 8s')), 8000);
      })]);
    } finally {
      clearTimeout(timer);
      const index = waiters.indexOf(resolve);
      if (index >= 0) waiters.splice(index, 1);
    }
  }
  async function until(predicate) {
    const observed = [];
    while (true) {
      const event = await next();
      observed.push(event);
      if (predicate(event)) return observed;
    }
  }
  return { socket, next, until, close: () => { if (socket.readyState === 1) socket.close(1000); }, send: value => socket.send(JSON.stringify(value)) };
}

async function runSocketJourneys(context, t) {
  await t.test('cookie Origin is mandatory and native keys need no browser Origin', async () => {
    const scenario = await createResponseJourneyFixture(context, 'ws-origin', [], { validate_response_schema: false });
    try {
      const denied = await context.mf.dispatchFetch('https://agents.test/v1/responses', {
        headers: { Upgrade: 'websocket', Connection: 'Upgrade', Cookie: scenario.principal.cookie },
      });
      assert.equal(denied.status, 403);
      const browser = await connect(context.mf, { Cookie: scenario.principal.cookie, Origin: 'https://agents.test' });
      browser.socket.close(1000);
      const native = await connect(context.mf, { Authorization: `Bearer ${scenario.principal.apiKey}` });
      native.send({ type: 'response.create', model: 'nonexistent-agent', input: 'No dispatch' });
      const error = await native.next();
      assert.equal(error.type, 'error');
      assert.equal(error.status, 404);
      assert.equal(scenario.state.calls, 0);
      native.socket.close(1000);
    } finally { await scenario.finish(); }
  });

  await t.test('warmup, store=false connection cache and same-lane FIFO use one harness', async () => {
    const scenario = await createResponseJourneyFixture(context, 'ws-cache', [{ text: 'Continuation result' }], { validate_response_schema: false });
    const client = await connect(context.mf, { Authorization: `Bearer ${scenario.principal.apiKey}` });
    try {
      client.send({ type: 'response.create', stream_id: 'primary', model: scenario.agent.id,
        input: 'Warmup retained input', generate: false, store: false });
      const warmup = await client.until(event => event.type === 'response.completed' || event.type === 'error');
      assert.equal(warmup.at(-1).type, 'response.completed', JSON.stringify(warmup));
      const parent = warmup.at(-1).response;
      assert.equal(scenario.state.calls, 0, 'Warmup cannot dispatch inference');
      assert.deepEqual(parent.output, []);
      await scenario.read(parent.id, '', 404);
      const otherConnection = await connect(context.mf, { Authorization: `Bearer ${scenario.principal.apiKey}` });
      try {
        otherConnection.send({ type: 'response.create', previous_response_id: parent.id, input: 'Cannot borrow another socket cache' });
        const unavailable = await otherConnection.next();
        assert.equal(unavailable.type, 'error');
        assert.equal(unavailable.status, 404, 'A response ID alone cannot transfer another connection no-store lease');
        assert.equal(scenario.state.calls, 0);
      } finally { otherConnection.socket.close(1000); }
      client.send({ type: 'response.create', stream_id: 'primary', previous_response_id: parent.id,
        input: 'Continue warmed context', store: false });
      const result = await client.until(event => event.type === 'response.completed' || event.type === 'error');
      assert.equal(result.at(-1).type, 'response.completed', JSON.stringify(result));
      assert.equal(result.at(-1).response.previous_response_id, parent.id);
      assert.ok(result.every(event => event.stream_id === 'primary'));
      assert.equal(scenario.state.calls, 1);
      assert.ok(JSON.stringify(scenario.state.requests[0].messages).includes('Warmup retained input'));
    } finally { client.socket.close(1000); await scenario.finish(); }
  });

  await t.test('named lane lifetime cap and API-key revocation are enforced before execution', async () => {
    const scenario = await createResponseJourneyFixture(context, 'ws-authority', [], { validate_response_schema: false });
    const client = await connect(context.mf, { Authorization: `Bearer ${scenario.principal.apiKey}` });
    try {
      for (let index = 0; index < 32; index++) {
        const lane = `lane-${index}`;
        client.send({ type: 'response.create', stream_id: lane, model: 'nonexistent-agent', input: 'Reject' });
        const error = await client.next();
        assert.equal(error.type, 'error');
        assert.equal(error.stream_id, lane);
        assert.equal(error.status, 404);
      }
      client.send({ type: 'response.create', stream_id: 'lane-33', model: scenario.agent.id, input: 'Reject limit' });
      assert.equal((await client.next()).error.code, 'websocket_stream_limit_reached');
      await context.db.prepare('UPDATE api_keys SET revoked_at=? WHERE account_id=?').bind(Math.floor(Date.now() / 1000), scenario.principal.owner).run();
      client.send({ type: 'response.create', stream_id: 'lane-0', model: scenario.agent.id, input: 'Reject revoked' });
      const rejected = await client.next();
      assert.equal(rejected.status, 401);
      assert.equal(scenario.state.calls, 0);
    } finally { client.socket.close(1000); }
    // Revocation intentionally invalidates normal fixture cleanup credentials;
    // the entire isolated Miniflare instance is disposed by the parent test.
    assert.equal(scenario.state.failure, null);
  });
}

// Protocol lane capacity does not grant additional account execution capacity.
async function runPlanConcurrency(context) {
  let release;
  const wait = new Promise(resolve => { release = resolve; });
  const scenario = await createResponseJourneyFixture(context, 'ws-plan', [{ text: 'Survives socket close', wait }], { validate_response_schema: false });
  const client = await connect(context.mf, { Authorization: `Bearer ${scenario.principal.apiKey}` });
  let primary;
  try {
    client.send({ type: 'response.create', stream_id: 'running', model: scenario.agent.id, input: 'Long task' });
    const started = await client.until(event => event.type === 'response.created');
    const id = started.at(-1).response.id;
    // Created is an identity publication, not proof of account run admission.
    const admittedBy = Date.now() + 8000;
    while (scenario.state.calls !== 1) {
      assert.ok(Date.now() < admittedBy, 'First run must be admitted before exercising the competing plan gate');
      await new Promise(resolve => setTimeout(resolve, 10));
    }
    client.send({ type: 'response.create', stream_id: 'other', model: scenario.agent.id, input: 'Plan cap' });
    const rejected = await client.until(event => event.stream_id === 'other' && ['error', 'response.failed'].includes(event.type));
    const denial = rejected.at(-1);
    if (denial.type === 'error') assert.equal(denial.status, 429);
    else {
      assert.equal(denial.response.status, 'failed');
      assert.equal(denial.response.error.code, 'server_error');
      assert.equal(denial.response.agents_error_code, 'run_limit_exceeded', 'The sanitized canonical failure retains its precise application diagnostic');
    }
    client.close();
    release();
    const deadline = Date.now() + 8000;
    while (true) {
      const response = await scenario.read(id);
      if (response.status === 'completed') break;
      assert.ok(Date.now() < deadline, 'Disconnect must detach observation without cancelling admitted work');
      await new Promise(resolve => setTimeout(resolve, 20));
    }
    assert.equal(scenario.state.calls, 1, 'Rejected lane and disconnect must not dispatch or replay inference');
  } catch (error) { primary = error; }
  finally { release(); await finishDisconnectedScenario(client, scenario, primary); }
}


async function runLaneConcurrency(context) {
  let release;
  const wait = new Promise(resolve => { release = resolve; });
  const scenario = await createResponseJourneyFixture(context, 'ws-sixteen',
    Array.from({ length: 18 }, (_, index) => ({ text: `Lane result ${index}`, wait })),
    { validate_response_schema: false });
  const now = Math.floor(Date.now() / 1000);
  const subscription = [{ id: 'local-ws-pro', product_id: 'agents', plan_id: 'agents-pro', status: 'active',
    current_period_start: now - 10, current_period_end: now + 3600, entitlements: ['agents.access', 'agents.tier.pro'] }];
  await context.db.prepare("INSERT INTO subscription_snapshots(account_id,product_id,subscription_json,fetched_at,valid_until) VALUES(?,'agents',?,?,?)")
    .bind(scenario.principal.owner, JSON.stringify(subscription), now, now + 300).run();
  const client = await connect(context.mf, { Authorization: `Bearer ${scenario.principal.apiKey}` });
  try {
    for (let index = 0; index < 17; index++) client.send({ type: 'response.create',
      stream_id: `parallel-${index}`, model: scenario.agent.id, input: `Input ${index}` });
    client.send({ type: 'response.create', stream_id: 'parallel-0', model: scenario.agent.id, input: 'FIFO second input' });
    const created = new Set();
    while (created.size < 16) {
      const event = await client.next();
      assert.notEqual(event.type, 'error', JSON.stringify(event));
      if (event.type === 'response.created') created.add(event.stream_id);
    }
    const deadline = Date.now() + 8000;
    while (scenario.state.calls < 16) {
      assert.ok(Date.now() < deadline, 'Sixteen independent actor runs must dispatch');
      await new Promise(resolve => setTimeout(resolve, 10));
    }
    assert.equal(scenario.state.calls, 16, 'Seventeenth lane and second same-lane request remain queued');
    assert.equal(created.has('parallel-16'), false);
    release();
    let completed = 0;
    let firstLaneTerminated = false;
    let firstLaneCreates = 1;
    while (completed < 18) {
      const event = await client.next();
      assert.notEqual(event.type, 'error', JSON.stringify(event));
      if (event.stream_id === 'parallel-0' && event.type === 'response.created') {
        firstLaneCreates++;
        assert.ok(firstLaneTerminated, 'Same-lane successor must not start before original terminal');
      }
      if (event.type === 'response.completed') {
        completed++;
        if (event.stream_id === 'parallel-0') firstLaneTerminated = true;
      }
    }
    assert.equal(firstLaneCreates, 2);
    assert.equal(scenario.state.calls, 18);
  } finally { release(); client.socket.close(1000); await scenario.finish(); }
}


async function runSteeringSuccessor(context) {
  let release;
  const wait = new Promise(resolve => { release = resolve; });
  const marker = 'Apply accepted steering exactly once-星';
  const scenario = await createResponseJourneyFixture(context, 'ws-steering', [
    { calls: [{ id: 'ws-clock', name: 'time', arguments: {} }], wait },
    { text: 'Steered successor finished', check(input) {
      const messages = JSON.stringify(input.messages);
      assert.equal(messages.split(marker).length - 1, 1);
      assert.ok(input.messages.some(message => message.role === 'tool' && message.tool_call_id === 'ws-clock'));
    } },
  ], { validate_response_schema: false });
  const client = await connect(context.mf, { Authorization: `Bearer ${scenario.principal.apiKey}` });
  try {
    client.send({ type: 'response.create', stream_id: 'steering', model: scenario.agent.id, input: 'Use the clock' });
    const start = await client.until(event => event.type === 'response.created');
    const parent = start.at(-1).response.id;
    client.send({ type: 'response.steer', previous_response_id: parent, input: marker });
    const accepted = await client.until(event => event.type === 'response.steer.accepted' || event.type === 'error');
    assert.equal(accepted.at(-1).type, 'response.steer.accepted', JSON.stringify(accepted));
    release();
    const result = await client.until(event => event.type === 'error' ||
      (event.type === 'response.completed' && event.response.id !== parent));
    assert.notEqual(result.at(-1).type, 'error', JSON.stringify(result));
    const original = result.find(event => event.type === 'response.incomplete' && event.response.id === parent);
    assert.ok(original, 'Steering is applied after the running server tool group reaches its safe boundary');
    assert.equal(original.response.incomplete_details.reason, 'steered');
    assert.ok(result.some(event => event.type === 'response.created' && event.response.id !== parent));
    assert.ok([...start, ...accepted, ...result].every(event => event.stream_id === 'steering'));
    assert.equal([...accepted, ...result].filter(event => event.type === 'response.steer.accepted').length, 1);
    assert.equal(scenario.state.calls, 2);
  } finally { release(); client.socket.close(1000); await scenario.finish(); }
}

async function runSteeringPendingInput(context) {
  let release;
  const wait = new Promise(resolve => { release = resolve; });
  const marker = 'Queued user change without replay-月';
  const tool = { type: 'function', name: 'ws_client_status', strict: true,
    parameters: { type: 'object', properties: {}, required: [], additionalProperties: false } };
  const scenario = await createResponseJourneyFixture(context, 'ws-pending', [
    { calls: [{ id: 'ws-client-call', name: tool.name, arguments: {} }], wait },
    { text: 'Client result plus steering applied', check(input) {
      const messages = JSON.stringify(input.messages);
      assert.equal(messages.split(marker).length - 1, 1);
      assert.ok(input.messages.some(message => message.role === 'tool' && message.tool_call_id === 'ws-client-call'));
    } },
  ], { validate_response_schema: false });
  const client = await connect(context.mf, { Authorization: `Bearer ${scenario.principal.apiKey}` });
  try {
    client.send({ type: 'response.create', stream_id: 'pending', model: scenario.agent.id, tools: [tool], input: 'Call client status' });
    const start = await client.until(event => event.type === 'response.created');
    const parent = start.at(-1).response.id;
    client.send({ type: 'response.steer', previous_response_id: parent, input: marker });
    const accepted = await client.until(event => event.type === 'response.steer.accepted' || event.type === 'error');
    assert.equal(accepted.at(-1).type, 'response.steer.accepted', JSON.stringify(accepted));
    release();
    const pending = await client.until(event => event.type === 'response.steer.pending' || event.type === 'error');
    assert.equal(pending.at(-1).type, 'response.steer.pending', JSON.stringify(pending));
    assert.ok(pending.some(event => event.type === 'response.completed' && event.response.id === parent));
    assert.equal(pending.at(-1).reason, 'waiting_for_required_input');
    assert.ok(pending.at(-1).required_input.some(item => item.type === 'function_call_output' && item.call_id === 'ws-client-call'));
    assert.equal(scenario.state.calls, 1, 'Queued steering cannot fabricate client tool results');
    client.send({ type: 'response.create', stream_id: 'pending', previous_response_id: parent,
      tools: [tool], input: [{ type: 'function_call_output', call_id: 'ws-client-call', output: 'Current status' }] });
    const result = await client.until(event => event.type === 'error' ||
      (event.type === 'response.completed' && event.response.id !== parent));
    assert.notEqual(result.at(-1).type, 'error', JSON.stringify(result));
    assert.equal(scenario.state.calls, 2);
  } finally { release(); client.socket.close(1000); await scenario.finish(); }
}


async function runAcceptedSteeringDisconnect(context) {
  let release;
  const wait = new Promise(resolve => { release = resolve; });
  const marker = 'Durably accepted intent after observer disconnect-雪';
  const scenario = await createResponseJourneyFixture(context, 'ws-steer-disconnect', [
    { calls: [{ id: 'disconnect-clock', name: 'time', arguments: {} }], wait },
    { text: 'Accepted work survived disconnect', check(input) {
      assert.equal(JSON.stringify(input.messages).split(marker).length - 1, 1);
    } },
  ], { validate_response_schema: false });
  const client = await connect(context.mf, { Authorization: `Bearer ${scenario.principal.apiKey}` });
  let primary;
  try {
    client.send({ type: 'response.create', model: scenario.agent.id, input: 'Call time before continuing' });
    const start = await client.until(event => event.type === 'response.created');
    const parent = start.at(-1).response.id;
    client.send({ type: 'response.steer', previous_response_id: parent, input: marker });
    const ack = await client.until(event => event.type === 'response.steer.accepted' || event.type === 'error');
    assert.equal(ack.at(-1).type, 'response.steer.accepted', JSON.stringify(ack));
    client.close();
    release();
    const deadline = Date.now() + 8000;
    let child;
    let session;
    while (true) {
      const routes = await context.db.prepare('SELECT id,session_id FROM response_routes WHERE account_id=? AND agent_id=? AND id<>?')
        .bind(scenario.principal.owner, scenario.agent.id, parent).all();
      if (routes.results.length) {
        assert.equal(routes.results.length, 1, 'Disconnect must not duplicate the automatic successor');
        session = routes.results[0].session_id;
        child = await scenario.read(routes.results[0].id);
        if (child.status === 'completed') break;
      }
      assert.ok(Date.now() < deadline, 'Accepted steering must continue durably after observation detaches');
      await new Promise(resolve => setTimeout(resolve, 20));
    }
    assert.equal(child.previous_response_id, parent);
    const history = await context.request('GET', `/v1/sessions/${session}/messages`, undefined, 200, scenario.principal.apiKey);
    assert.equal(JSON.stringify(history).split(marker).length - 1, 1);
    const ledger = await context.db.prepare("SELECT COUNT(*) AS count FROM usage_ledger l JOIN usage_reservations r ON r.id=l.reservation_id WHERE r.account_id=? AND l.resource='model'")
      .bind(scenario.principal.owner).first();
    assert.equal(ledger.count, 2, 'Each actual model request settles once even after disconnect');
    assert.equal(scenario.state.calls, 2);
  } catch (error) { primary = error; }
  finally { release(); await finishDisconnectedScenario(client, scenario, primary); }
}



async function runOfficialSdk(context) {
  const scenario = await createResponseJourneyFixture(context, 'ws-sdk', [
    { text: 'Official SDK initial answer' }, { text: 'Official SDK continuation answer' },
  ], { validate_response_schema: false });
  const address = await context.mf.ready;
  const client = new OpenAI({ apiKey: scenario.principal.apiKey,
    baseURL: new URL('/v1', address).href, maxRetries: 0 });
  const socket = new ResponsesWS(client, { reconnect: null, maxPayload: 32 * 1024 * 1024 });
  const events = [];
  const waiters = [];
  socket.on('event', event => {
    events.push(event);
    if (event.type === 'response.completed') waiters.shift()?.resolve(event.response);
  });
  socket.on('error', () => { for (const waiter of waiters.splice(0)) waiter.reject(new Error('Official SDK WebSocket failed; do not replay creation')); });
  async function completed() {
    let timer;
    let entry;
    try {
      return await new Promise((resolve, reject) => {
        entry = { resolve, reject }; waiters.push(entry);
        timer = setTimeout(() => reject(new Error('Official SDK canonical completion deadline exceeded')), 8000);
      });
    } finally {
      clearTimeout(timer);
      const index = waiters.indexOf(entry);
      if (index >= 0) waiters.splice(index, 1);
    }
  }
  try {
    await new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error('Official SDK network upgrade deadline exceeded')), 8000);
      socket.socket.on('open', () => { clearTimeout(timer); resolve(); });
      socket.on('error', () => { clearTimeout(timer); reject(new Error('Official SDK network upgrade rejected')); });
    });
    const first = completed();
    socket.send({ type: 'response.create', stream_id: 'sdk', model: scenario.agent.id, input: 'First request' });
    const parent = await first;
    const second = completed();
    socket.send({ type: 'response.create', stream_id: 'sdk', previous_response_id: parent.id, input: 'Continue' });
    const result = await second;
    assert.equal(result.previous_response_id, parent.id);
    assert.equal(events.filter(event => event.type === 'response.created').length, 2);
    assert.ok(events.every(event => event.stream_id === 'sdk'));
    assert.equal(scenario.state.calls, 2);
  } finally { socket.close(); await scenario.finish(); }
}


async function finishDisconnectedScenario(client, scenario, primary) {
  const errors = primary ? [primary] : [];
  try { client.close(); } catch (error) { errors.push(error); }
  try { await scenario.finish(); } catch (error) { errors.push(error); }
  if (errors.length === 1) throw errors[0];
  if (errors.length > 1) throw new AggregateError(errors, 'Primary and disconnect cleanup failures retained');
}


const caseIds = new Map([
  ['cookie Origin is mandatory and native keys need no browser Origin', 'ws-origin'],
  ['warmup, store=false connection cache and same-lane FIFO use one harness', 'ws-warmup-cache'],
  ['named lane lifetime cap and API-key revocation are enforced before execution', 'ws-lane-revocation'],
  ['current official Node SDK uses the real compiled Worker network upgrade', 'ws-sdk'],
  ['accepted steering survives disconnect without source or financial replay', 'ws-steer-disconnect'],
  ['steering crosses a safe server-tool boundary into an automatic successor', 'ws-steer-successor'],
  ['steering waits for explicit client results without repeating accepted input', 'ws-steer-pending'],
  ['sixteen parallel actors, queued seventeenth lane and same-lane FIFO', 'ws-lane-concurrency'],
  ['account concurrency stays enforced and disconnect does not cancel work', 'ws-plan-disconnect'],
]);

async function selectedCases(parent) {
  const requested = process.env.MAGENTS_WS_CASE;
  const selected = requested === undefined ? [...caseIds.values()] : requested.split(',').filter(Boolean).map(value => caseIds.get(value) ?? value);
  assert.ok(selected.length, 'WS selector must match at least one case');
  for (const id of selected) assert.ok([...caseIds.values()].includes(id), `Unknown WS case ID: ${id}`);
  const sha = bytes => createHash('sha256').update(bytes).digest('hex');
  let provenance;
  try { provenance = JSON.parse(await readFile(path.join(build, 'provenance.json'), 'utf8')); }
  catch (error) { if (error.code !== 'ENOENT') throw error; }
  const receipt = {
    schema_version: 1,
    artifact_source_sha: provenance?.source_sha ?? process.env.MAGENTS_WORKER_SOURCE_SHA ?? null,
    source_run: provenance?.source_run ?? process.env.MAGENTS_WORKER_SOURCE_RUN ?? null,
    artifact_id: provenance?.artifact_id ?? null,
    fixture_head_sha: spawnSync('git', ['rev-parse', 'HEAD'], { cwd: root, encoding: 'utf8' }).stdout.trim(),
    fixture_sha256: sha(await readFile(fileURLToPath(import.meta.url))),
    wasm_sha256: sha(await readFile(path.join(build, 'index_bg.wasm'))),
    selected_case_ids: selected,
    cases: Object.fromEntries([...caseIds.values()].map(id => [id, { executed: false, status: 'not_executed' }])),
  };
  const directory = path.join(root, '.cache/verification');
  await mkdir(directory, { recursive: true });
  const save = () => writeFile(path.join(directory, 'ws-cases.json'), JSON.stringify(receipt, null, 2) + '\n');
  await save();
  return { test: async (name, run) => {
    const id = caseIds.get(name);
    assert.ok(id, `Missing stable WS case ID: ${name}`);
    return parent.test(`${id}: ${name}`, { skip: !selected.includes(id) }, async context => {
      receipt.cases[id] = { executed: true, status: 'running' };
      await save();
      try {
        await run(context);
        receipt.cases[id].status = 'passed';
      } catch (error) {
        receipt.cases[id].status = 'failed';
        throw error;
      } finally { await save(); }
    });
  } };
}
