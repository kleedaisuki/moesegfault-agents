import assert from 'node:assert/strict';
import { appendFile, readFile, readdir } from 'node:fs/promises';
import { spawnSync } from 'node:child_process';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { Miniflare, convertV4MiniflareOptions } from 'miniflare';
import { createBrowserPrincipal } from './browser-principal.mjs';
import { createIdentityFixture } from './auth-journey.mjs';

const root = fileURLToPath(new URL('../../', import.meta.url));
const [phase, directory] = process.argv.slice(2);
assert.ok(['start', 'recover'].includes(phase));
assert.ok(path.resolve(directory).startsWith(path.join(root, '.temp') + path.sep));
const config = JSON.parse(await readFile(path.join(directory, 'config.json'), 'utf8'));
const build = path.resolve(root, process.env.WORKER_BUILD_DIR ?? 'crates/agents-worker/build');
const deployment = JSON.parse(await readFile(path.join(root, 'wrangler.jsonc'), 'utf8'));
const identity = createIdentityFixture();
const options = convertV4MiniflareOptions({
  modulesRoot: build,
  modules: [
    { type: 'ESModule', path: path.join(build, 'worker/shim.mjs') },
    { type: 'ESModule', path: path.join(build, 'index.js') },
    { type: 'CompiledWasm', path: path.join(build, 'index_bg.wasm') },
  ],
  compatibilityDate: '2026-10-23',
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
    DATA_ENCRYPTION_KEY: config.encryptionKey,
    IDENTITY_ISSUER: 'https://identity.test', IDENTITY_WEB_CLIENT_ID: 'test-dashboard',
    IDENTITY_NATIVE_CLIENT_ID: 'test-native', IDENTITY_REDIRECT_URI: 'https://agents.test/auth/callback',
    IDENTITY_NATIVE_REDIRECT_URI: 'http://127.0.0.1/callback',
    IDENTITY_CLIENT_PRIVATE_JWK: JSON.stringify(identity.privateJwk),
    BILLING_PRODUCT_ID: 'agents', BILLING_MODE: 'test',
    MODEL_REGISTRY_JSON: '[]', MODEL_PRICING_JSON: '{}',
    RESOURCE_PRICING_JSON: deployment.env.staging.vars.RESOURCE_PRICING_JSON,
    RUN_RESOURCE_RESERVATION_MICROUSD: '20000', FREE_GRANT_BUDGET_MICROUSD_PER_DAY: '2000000',
  },
  async outboundService(request) {
    assert.equal(request.url, 'https://crash-provider.example.com/v1/chat/completions', 'Unexpected fixture egress');
    assert.equal(request.headers.get('authorization'), 'Bearer synthetic-crash-provider-key');
    const body = await request.json();
    assert.equal(body.model, 'crash-fixture');
    assert.equal(body.stream, true);
    await appendFile(path.join(directory, 'provider-calls.jsonl'), JSON.stringify({ phase }) + '\n');
    if (phase === 'recover') return Response.json({ error: { message: 'Inference replay forbidden after hard crash' } }, { status: 500 });
    const event = { id: 'synthetic-crash-upstream', model: 'crash-fixture', choices: [{
      index: 0, delta: { role: 'assistant', content: 'A disclosed delta before process death.' }, finish_reason: null,
    }] };
    return new Response(new ReadableStream({
      start(controller) { controller.enqueue(new TextEncoder().encode(`data: ${JSON.stringify(event)}\n\n`)); },
    }), { headers: { 'Content-Type': 'text/event-stream' } });
  },
});
const mf = new Miniflare(options);
const db = await mf.getD1Database('DB');
const delay = milliseconds => new Promise(resolve => setTimeout(resolve, milliseconds));

async function request(key, method, endpoint, body, expected = 200) {
  const response = await mf.dispatchFetch('https://agents.test' + endpoint, {
    method, headers: { Authorization: `Bearer ${key}`, 'Content-Type': 'application/json', 'CF-Connecting-IP': '192.0.2.67' },
    ...(body === undefined ? {} : { body: JSON.stringify(body) }),
  });
  assert.equal(response.status, expected, `${method} ${endpoint}: HTTP ${response.status}`);
  return response.json();
}

async function migrate() {
  for (const name of (await readdir(path.join(root, 'migrations'))).filter(name => name.endsWith('.sql')).sort()) {
    const sql = await readFile(path.join(root, 'migrations', name), 'utf8');
    const parsed = spawnSync(process.env.PYTHON ?? 'python', ['-c', 'import sys,sqlite3,json\ns=""; out=[]\nfor line in sys.stdin:\n s+=line\n if sqlite3.complete_statement(s): out.append(s); s=""\nassert not s.strip(), s\nprint(json.dumps(out))'], { input: sql, encoding: 'utf8' });
    assert.equal(parsed.status, 0, name);
    await db.batch(JSON.parse(parsed.stdout).map(statement => db.prepare(statement)));
  }
}

async function start() {
  await migrate();
  const principal = await createBrowserPrincipal({ mf, db, encryptionKey: Buffer.from(config.encryptionKey, 'base64url'), owner: 'hard-crash-owner' });
  const credential = await principal.browser('POST', '/v1/provider-keys', {
    protocol: 'openai', base_url: 'https://crash-provider.example.com/v1', api_key: 'synthetic-crash-provider-key',
  }, 201);
  const agent = await request(principal.apiKey, 'POST', '/v1/agents', {
    name: 'Hard crash receipt', workspace: false, tools: ['time'],
    model: { provider: 'byok', credential_id: credential.id, model: 'crash-fixture', context_window: 8192, max_output_tokens: 512 },
  }, 201);
  const response = await mf.dispatchFetch('https://agents.test/v1/responses', {
    method: 'POST', headers: { Authorization: `Bearer ${principal.apiKey}`, 'Content-Type': 'application/json' },
    body: JSON.stringify({ model: agent.id, input: 'Start a stream and remain in flight.', stream: true }),
  });
  assert.equal(response.status, 200);
  const sessionId = response.headers.get('x-agents-session');
  assert.ok(sessionId);
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let buffer = '';
  let responseId;
  while (true) {
    const chunk = await reader.read();
    assert.equal(chunk.done, false, 'Provider must remain in flight until the process is killed');
    buffer += decoder.decode(chunk.value, { stream: true });
    let boundary;
    while ((boundary = buffer.indexOf('\n\n')) >= 0) {
      const block = buffer.slice(0, boundary);
      buffer = buffer.slice(boundary + 2);
      const data = block.split('\n').find(line => line.startsWith('data: '));
      if (!data) continue;
      const event = JSON.parse(data.slice(6));
      if (event.type === 'response.created') responseId = event.response.id;
      if (event.type !== 'response.output_text.delta') continue;
      assert.ok(responseId);
      const held = await db.prepare("SELECT COUNT(*) AS count FROM usage_reservations WHERE account_id=? AND state='reserved' AND idempotency_key LIKE 'completion:%'")
        .bind(principal.owner).first('count');
      assert.equal(held, 1, 'The crash must happen during a dispatched, unsettled provider operation');
      process.send({ type: 'disclosed', receipt: { key: principal.apiKey, owner: principal.owner,
        responseId, sessionId, sequence: event.sequence_number } });
      await new Promise(() => {});
    }
  }
}

async function recover() {
  const receipt = JSON.parse(await readFile(path.join(directory, 'receipt.json'), 'utf8'));
  const deadline = Date.now() + 8000;
  let final;
  while (Date.now() < deadline) {
    final = await request(receipt.key, 'GET', `/v1/responses/${receipt.responseId}`);
    if (final.status === 'failed') break;
    await delay(20);
  }
  assert.equal(final?.status, 'failed', 'A cold in-flight response must fail explicitly rather than replay inference');
  assert.equal(final.error?.code, 'server_error');
  assert.equal(final.agents_error_code, 'provider_outcome_unknown');
  const response = await mf.dispatchFetch(`https://agents.test/v1/responses/${receipt.responseId}?stream=true&starting_after=${receipt.sequence}`, {
    headers: { Authorization: `Bearer ${receipt.key}` },
  });
  assert.equal(response.status, 200);
  const events = (await response.text()).split('\n\n')
    .map(block => block.split('\n').find(line => line.startsWith('data: '))).filter(Boolean)
    .map(line => JSON.parse(line.slice(6)));
  assert.ok(events.length > 0);
  assert.ok(events.every(event => event.sequence_number > receipt.sequence), 'Crash recovery cannot reuse a sequence disclosed by the dead process');
  assert.equal(events.at(-1).type, 'response.failed');
  assert.equal(events.at(-1).response.id, receipt.responseId);
  const calls = (await readFile(path.join(directory, 'provider-calls.jsonl'), 'utf8')).trim().split('\n').map(line => JSON.parse(line));
  assert.deepEqual(calls, [{ phase: 'start' }], 'Recovery cannot dispatch another provider request');
  const held = await db.prepare("SELECT COUNT(*) AS count FROM usage_reservations WHERE account_id=? AND state='reserved' AND idempotency_key LIKE 'completion:%'")
    .bind(receipt.owner).first('count');
  assert.equal(held, 1, 'Unknown provider usage must retain the explicit unresolved completion receipt');
  const settled = await db.prepare("SELECT COUNT(*) AS count FROM usage_ledger l JOIN usage_reservations r ON r.id=l.reservation_id WHERE r.account_id=? AND l.resource='model'")
    .bind(receipt.owner).first('count');
  assert.equal(settled, 0, 'A crash cannot manufacture a zero-usage provider settlement');
  assert.ok(process.env.MAGENTS_SCHEMA_PYTHON);
  const strict = spawnSync(process.env.MAGENTS_SCHEMA_PYTHON, [path.join(root, 'tests/e2e/validate_responses_wire.py')], {
    input: JSON.stringify({ responses: [final], events }), encoding: 'utf8', timeout: 15000,
  });
  assert.equal(strict.status, 0, strict.stdout);
  await mf.dispose();
  process.send({ type: 'recovered', evidence: { disclosed_sequence: receipt.sequence,
    terminal_sequence: events.at(-1).sequence_number, provider_calls: calls.length, unresolved_completion_receipts: held } });
}

try {
  await (phase === 'start' ? start() : recover());
} catch (error) {
  // Only this process's synthetic fixture data may appear in assertion details.
  process.send?.({ type: 'failed', error: { name: error.name, message: error.message, stack: error.stack?.split('\n').slice(0, 7).join('\n') } });
  await mf.dispose().catch(() => {});
  process.exitCode = 1;
} finally {
  if (phase === 'recover') process.disconnect?.();
}
