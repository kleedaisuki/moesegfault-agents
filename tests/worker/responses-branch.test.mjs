import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { mkdir, mkdtemp, readFile, readdir } from 'node:fs/promises';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import { Miniflare, convertV4MiniflareOptions } from 'miniflare';
import { createIdentityFixture } from './auth-journey.mjs';
import { createResponseJourneyFixture, createResponsesProviderFixture } from './responses-journey.mjs';

const root = fileURLToPath(new URL('../../', import.meta.url));
const build = path.resolve(root, process.env.WORKER_BUILD_DIR ?? 'crates/agents-worker/build');
const fixed = 'Fixed agent prefix must remain in every model context.';
const localTool = {
  type: 'function', name: 'branch_local_echo', strict: true,
  parameters: { type: 'object', properties: { value: { type: 'string' } },
    required: ['value'], additionalProperties: false },
};

function textOf(messages) {
  return JSON.stringify(messages);
}

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

async function runHistoryFork(context) {
  const markerA = 'branch-source-A-雪';
  const markerB = 'branch-sibling-B-星';
  const markerC = 'branch-child-C-月';
  const changed = 'New agent configuration must not replace the parent pin.';
  const scenario = await createResponseJourneyFixture(context, 'branch-history', [
    { text: 'Parent A completed.' },
    { text: 'Continuation B completed.' },
    { text: 'Independent child C completed.', check(input) {
      const prompt = textOf(input.messages);
      assert.ok(prompt.includes(markerA));
      assert.ok(prompt.includes('Parent A completed.'));
      assert.ok(prompt.includes(markerC));
      assert.ok(!prompt.includes(markerB), 'A historical fork cannot inherit its sibling input');
      assert.ok(!prompt.includes('Continuation B completed.'), 'A historical fork cannot inherit its sibling answer');
      assert.ok(prompt.includes(fixed), 'A same-agent branch retains the original immutable pin');
      assert.ok(!prompt.includes(changed));
      assert.ok(!prompt.includes('RESPONSE-ONLY-A'));
      assert.ok(!prompt.includes('RESPONSE-ONLY-B'), 'Top-level response instructions are not inherited');
    } },
  ]);
  try {
    const a = await scenario.invoke({ input: markerA, instructions: 'RESPONSE-ONLY-A' });
    const b = await scenario.invoke({ previous_response_id: a.value.id, input: markerB, instructions: 'RESPONSE-ONLY-B' });
    assert.equal(a.headers.get('x-agents-session'), b.headers.get('x-agents-session'), 'Latest-head continuation keeps the warm session');
    await context.request('PATCH', `/v1/agents/${scenario.agent.id}`, { instructions: changed }, 200, scenario.principal.apiKey);
    const body = { previous_response_id: a.value.id, input: markerC };
    const c = await scenario.invoke(body, 200, { 'Idempotency-Key': 'history-fork-C' });
    assert.equal(c.value.status, 'completed');
    assert.equal(c.value.previous_response_id, a.value.id);
    assert.notEqual(c.headers.get('x-agents-session'), a.headers.get('x-agents-session'));
    const retry = await scenario.invoke(body, 200, { 'Idempotency-Key': 'history-fork-C' });
    assert.deepEqual(retry.value, c.value);
    assert.equal(retry.headers.get('x-agents-session'), c.headers.get('x-agents-session'));
    assert.equal(scenario.state.calls, 3, 'An idempotent fork retry cannot regenerate either branch');
    const rows = await context.db.prepare('SELECT id,session_id FROM response_routes WHERE account_id=? ORDER BY id')
      .bind(scenario.principal.owner).all();
    assert.equal(rows.results.length, 3, 'Fork routing preserves one durable identity per response');
    const sessions = new Map(rows.results.map(row => [row.id, row.session_id]));
    assert.equal(sessions.get(a.value.id), sessions.get(b.value.id));
    assert.equal(sessions.get(c.value.id), c.headers.get('x-agents-session'));
    const source = await context.request('GET', `/v1/sessions/${sessions.get(a.value.id)}`, undefined, 200, scenario.principal.apiKey);
    const child = await context.request('GET', `/v1/sessions/${sessions.get(c.value.id)}`, undefined, 200, scenario.principal.apiKey);
    assert.ok(textOf(source.messages).includes(markerB));
    assert.ok(!textOf(source.messages).includes(markerC), 'A child cannot mutate its parent source journal');
    assert.ok(!textOf(child.messages).includes(markerB));
    for (const sessionId of new Set(sessions.values())) {
      await context.mf.unsafeEvictDurableObject('', 'Session', { name: `${scenario.principal.owner}:${sessionId}` });
    }
    assert.deepEqual(await scenario.read(a.value.id), a.value);
    assert.deepEqual(await scenario.read(b.value.id), b.value);
    assert.deepEqual(await scenario.read(c.value.id), c.value, 'Cold reconstruction retains each immutable branch response');
    assert.equal(scenario.state.calls, 3);
  } catch (error) { scenario.recordFailure(error); throw error; }
  finally { await scenario.finish(); }
}

async function runParallelToolForks(context) {
  let release;
  const bothDispatched = new Promise(resolve => { release = resolve; });
  const results = new Set();
  function check(input) {
    const tools = input.messages.filter(message => message.role === 'tool' && message.tool_call_id === 'shared-parent-call');
    assert.equal(tools.length, 1, 'Each child consumes the parent pending call exactly once');
    assert.ok(['left-result-雪', 'right-result-星'].includes(tools[0].content));
    assert.ok(!results.has(tools[0].content), 'No result can be replayed in another branch');
    results.add(tools[0].content);
    const sibling = tools[0].content === 'left-result-雪' ? 'right-result-星' : 'left-result-雪';
    assert.ok(!textOf(input.messages).includes(sibling), 'Parallel children cannot observe each other’s tool result');
    if (results.size === 2) release();
  }
  const scenario = await createResponseJourneyFixture(context, 'branch-parallel-tools', [
    { calls: [{ id: 'shared-parent-call', name: localTool.name, arguments: { value: 'parent' } }] },
    { text: 'First independent tool continuation.', check, wait: bothDispatched },
    { text: 'Second independent tool continuation.', check, wait: bothDispatched },
  ]);
  try {
    // A synthetic local Go snapshot exercises two admitted runs. Free deliberately
    // permits only one, so expecting parallel generation on Free would test a bug.
    const now = Math.floor(Date.now() / 1000);
    const subscription = [{ id: 'local-branch-go', product_id: 'agents', plan_id: 'agents-go', status: 'active',
      current_period_start: now - 10, current_period_end: now + 3600, entitlements: ['agents.access', 'agents.tier.go'] }];
    await context.db.prepare("INSERT INTO subscription_snapshots(account_id,product_id,subscription_json,fetched_at,valid_until) VALUES(?,'agents',?,?,?)")
      .bind(scenario.principal.owner, JSON.stringify(subscription), now, now + 300).run();
    const parent = await scenario.invoke({ input: 'Call the local function once.', tools: [localTool] });
    assert.equal(parent.value.status, 'completed');
    assert.equal(parent.value.output.filter(item => item.type === 'function_call').length, 1);
    const bodies = ['left-result-雪', 'right-result-星'].map(output => ({
      previous_response_id: parent.value.id, tools: [localTool],
      input: [{ type: 'function_call_output', call_id: 'shared-parent-call', output }],
    }));
    const [left, right] = await Promise.all(bodies.map((body, index) => scenario.invoke(body, 200, {
      'Idempotency-Key': `parallel-child-${index}`,
    })));
    assert.equal(results.size, 2, 'Both branches must actually dispatch before either fixture upstream is released');
    assert.equal(left.value.status, 'completed');
    assert.equal(right.value.status, 'completed');
    assert.notEqual(left.value.id, right.value.id);
    assert.notEqual(left.headers.get('x-agents-session'), right.headers.get('x-agents-session'));
    for (const child of [left, right]) assert.equal(child.value.previous_response_id, parent.value.id);
    const replay = await scenario.invoke(bodies[1], 200, { 'Idempotency-Key': 'parallel-child-1' });
    assert.deepEqual(replay.value, right.value);
    assert.equal(scenario.state.calls, 3, 'Two independent child results do not consume or replay each other’s inference');
    assert.deepEqual(await scenario.read(parent.value.id), parent.value, 'Pending-call metadata belongs to the immutable parent');
  } catch (error) { scenario.recordFailure(error); throw error; }
  finally { release(); await scenario.finish(); }
}

async function runCrossAgentFork(context) {
  const targetInstructions = 'TARGET-AGENT-PIN-月';
  const scenario = await createResponseJourneyFixture(context, 'branch-cross-agent', [
    { text: 'Original agent answer.' },
    { text: 'Target agent branch answer.', check(input) {
      const prompt = textOf(input.messages);
      assert.ok(prompt.includes(targetInstructions));
      assert.ok(!prompt.includes(fixed), 'A different agent cannot inherit the parent agent’s permission/instruction prefix');
      assert.ok(prompt.includes('original-cross-agent-user-雪'));
      assert.ok(prompt.includes('Original agent answer.'));
    } },
  ]);
  let target;
  try {
    const parent = await scenario.invoke({ input: 'original-cross-agent-user-雪' });
    target = await context.request('POST', '/v1/agents', {
      name: 'Owned branch target', instructions: targetInstructions, model: scenario.agent.model,
      tools: ['time'], workspace: false, compaction_percent: 80,
    }, 201, scenario.principal.apiKey);
    const child = await scenario.invoke({ model: target.id, previous_response_id: parent.value.id, input: 'Continue using the target agent.' });
    assert.equal(child.value.status, 'completed');
    assert.equal(child.value.model, target.id);
    assert.equal(child.value.previous_response_id, parent.value.id);
    assert.notEqual(child.headers.get('x-agents-session'), parent.headers.get('x-agents-session'));
    assert.deepEqual(await scenario.read(parent.value.id), parent.value);
    assert.equal(scenario.state.calls, 2);
  } catch (error) { scenario.recordFailure(error); throw error; }
  finally {
    const errors = [];
    if (target) {
      try {
        const sessions = await context.request('GET', `/v1/agents/${target.id}/sessions`, undefined, 200, scenario.principal.apiKey);
        for (const session of sessions.data) await context.request('DELETE', `/v1/sessions/${session.id}`, undefined, 200, scenario.principal.apiKey);
        await context.request('DELETE', `/v1/agents/${target.id}`, undefined, 200, scenario.principal.apiKey);
      } catch (error) { errors.push(error); }
    }
    try { await scenario.finish(); } catch (error) { errors.push(error); }
    if (errors.length === 1) throw errors[0];
    if (errors.length) throw new AggregateError(errors, 'Cross-agent primary and cleanup failures retained');
  }
}

test('compiled Rust Worker: immutable historical and concurrent response branches', { timeout: 60000 }, async t => {
  const encryptionKey = randomBytes(32);
  const identity = createIdentityFixture();
  const providers = createResponsesProviderFixture();
  const deployment = JSON.parse(await readFile(path.join(root, 'wrangler.jsonc'), 'utf8'));
  await mkdir(path.join(root, '.temp'), { recursive: true });
  const directory = await mkdtemp(path.join(root, '.temp/responses-branches-'));
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
  await t.test('historical fork excludes siblings and retains pin across cold reconstruction', () => runHistoryFork(context));
  await t.test('parallel children independently resolve the same immutable pending function call', () => runParallelToolForks(context));
  await t.test('an owned different agent receives its own pin and inherited user history', () => runCrossAgentFork(context));
});
