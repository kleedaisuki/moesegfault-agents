import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { createHash, randomBytes } from 'node:crypto';
import { mkdir, mkdtemp, readFile, readdir, writeFile } from 'node:fs/promises';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import { Miniflare, convertV4MiniflareOptions } from 'miniflare';
import { createIdentityFixture } from './auth-journey.mjs';
import { createNativeSemanticsProvider } from './native-semantics-journey.mjs';
import { createMcpTransport, runMcpApprovalJourney, runMcpDeferredJourney, runMcpDeniedAndUnknown, runMcpApprovalReuse } from './mcp-journey.mjs';

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

test('compiled Rust Worker: owned MCP count, approval and exactly-once execution', { timeout: 120000 }, async t => {
  const encryptionKey = randomBytes(32);
  const identity = createIdentityFixture();
  const providers = createMcpTransport(createNativeSemanticsProvider());
  const deployment = JSON.parse(await readFile(path.join(root, 'wrangler.jsonc'), 'utf8'));
  await mkdir(path.join(root, '.temp'), { recursive: true });
  const directory = await mkdtemp(path.join(root, '.temp/mcp-worker-'));
  const mf = new Miniflare(convertV4MiniflareOptions({
    modulesRoot: build,
    modules: [
      { type: 'ESModule', path: path.join(build, 'worker/shim.mjs'),
        contents: await readFile(new URL('./mcp-shim.mjs', import.meta.url), 'utf8') },
      { type: 'ESModule', path: path.join(build, 'worker/chat-recovery-shim.mjs'),
        contents: await readFile(new URL('./chat-recovery-shim.mjs', import.meta.url), 'utf8') },
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
      BILLING_PRODUCT_ID: 'agents', BILLING_MODE: 'test', HOSTED_TOOL_RUNTIMES: 'mcp,tool_search',
      MODEL_REGISTRY_JSON: '[]', MODEL_PRICING_JSON: '{}',
      RESOURCE_PRICING_JSON: deployment.env.staging.vars.RESOURCE_PRICING_JSON,
      RUN_RESOURCE_RESERVATION_MICROUSD: '20000', FREE_GRANT_BUDGET_MICROUSD_PER_DAY: '100000000',
    },
    async outboundService(request) {
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
  function validateWire(response, events) {
    assert.ok(process.env.MAGENTS_CURRENT_SCHEMA_PYTHON, 'Use the current strict SDK interpreter');
    const argv = events.length ? ['tests/e2e/current_responses_sdk.py', 'response-wire', '--input', '-'] :
      ['-c', 'import json,sys;from openai.types.responses import Response;response: Response=Response.model_validate(json.load(sys.stdin),strict=True);print(json.dumps({"passed":True,"scope":"strict_current_sdk_response_snapshot","output_items":len(response.output)}))'];
    const checked = spawnSync(process.env.MAGENTS_CURRENT_SCHEMA_PYTHON, argv,
      { cwd: root, input: JSON.stringify(events.length ? { response, events } : response), encoding: 'utf8', maxBuffer: 1024 * 1024 });
    assert.equal(checked.status, 0, checked.stderr || checked.stdout);
    return JSON.parse(checked.stdout);
  }
  const context = { mf, db, encryptionKey, request, providers, validateWire };
  const reference = JSON.parse(await readFile(path.join(root, 'tests/fixtures/mcp-token-count.json'), 'utf8'));
  const receipt = {};
  const chosen = process.env.MAGENTS_MCP_CASE?.split(',');
  let selected = 0;
  for (const [id, run] of [['mcp_approval', () => runMcpApprovalJourney(context, reference)], ['mcp_deferred', () => runMcpDeferredJourney(context)], ['mcp_negative', () => runMcpDeniedAndUnknown(context)], ['mcp_claim', () => runMcpApprovalReuse(context)]]) {
    if (chosen && !chosen.includes(id)) continue;
    selected++;
    await t.test(id, async () => {
      receipt[id] = await run();
      await writeFile(path.join(directory, `receipt-${id}.json`), JSON.stringify({ source: process.env.MAGENTS_TEST_EXPECTED_BUILD ?? process.env.GITHUB_SHA ?? null,
        wasm_sha256: createHash('sha256').update(await readFile(path.join(build, 'index_bg.wasm'))).digest('hex'),
        case_id: id, status: 'passed_current', ...receipt[id] }, null, 2));
    });
  }
  assert.ok(selected, 'Select an actual MCP case');
  assert.equal(Object.keys(receipt).length, selected, 'Only actual successful cases enter the receipt');
  await writeFile(path.join(directory, 'receipt.json'), JSON.stringify({
    fixture: 'actual-compiled-rust-worker',
    source: process.env.MAGENTS_TEST_EXPECTED_BUILD ?? process.env.GITHUB_SHA ?? null,
    wasm_sha256: createHash('sha256').update(await readFile(path.join(build, 'index_bg.wasm'))).digest('hex'),
    executed_case_ids: Object.keys(receipt), external_network_calls: 0, ...receipt,
  }, null, 2));
});
