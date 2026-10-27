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
import { runProgrammaticTime, runProgrammaticMcp, runProgrammaticAdmissionFailures, runProgrammaticClient, runProgrammaticApproval } from './programmatic-journey.mjs';
import { createMcpTransport } from './mcp-journey.mjs';

const root = fileURLToPath(new URL('../../', import.meta.url));
const build = path.resolve(root, process.env.WORKER_BUILD_DIR ?? 'crates/agents-worker/build');

test('compiled Rust Worker: selected programmatic Session capability journeys', { timeout: 75000 }, async t => {
  const identity = createIdentityFixture();
  const encryptionKey = randomBytes(32);
  const providers = createMcpTransport(createNativeSemanticsProvider());
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
      { type: 'ESModule', path: path.join(build, 'worker/shim.mjs') },
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
  function validateWire(response, events) {
    assert.ok(process.env.MAGENTS_CURRENT_SCHEMA_PYTHON, 'Use the current strict SDK interpreter');
    const checked = spawnSync(process.env.MAGENTS_CURRENT_SCHEMA_PYTHON,
      ['tests/e2e/current_responses_sdk.py', 'response-wire', '--input', '-'],
      { cwd: root, input: JSON.stringify({ response, events }), encoding: 'utf8', maxBuffer: 1024 * 1024 });
    assert.equal(checked.status, 0, checked.stderr || checked.stdout);
    return JSON.parse(checked.stdout);
  }
  async function configureProgramCapability(missing) {
    const tariffs = pricing.tariffs.filter(tariff => !tariff.unit.startsWith('dynamic_worker_'));
    const worker = options.workers[0];
    const env = { ...worker.config.env };
    if (missing === 'loader') delete env.PROGRAM_LOADER;
    env.RESOURCE_PRICING_JSON = { type: 'text', value: missing === 'price' ? JSON.stringify({ ...pricing, tariffs }) : JSON.stringify(pricing) };
    await mf.setOptions({ ...options, workers: [{ ...worker, config: { ...worker.config, env } }, ...options.workers.slice(1)] });
    context.db = await mf.getD1Database('DB');
  }
  const context = { mf, db, encryptionKey, providers, request, validateWire, configureProgramCapability };
  const cases = { time: runProgrammaticTime, mcp: runProgrammaticMcp, admission: runProgrammaticAdmissionFailures, client: runProgrammaticClient, approval: runProgrammaticApproval };
  const selected = process.env.MAGENTS_PROGRAMMATIC_CASE?.split(",") ?? Object.keys(cases);
  const receipt = {};
  for (const name of selected) {
    assert.ok(Object.hasOwn(cases, name), `Unknown programmatic journey: ${name}`);
    receipt[name] = await cases[name](context);
  }
  await writeFile(path.join(directory, 'receipt.json'), JSON.stringify({
    source: process.env.MAGENTS_TEST_EXPECTED_BUILD ?? process.env.GITHUB_SHA ?? null,
    fixture: 'actual-compiled-rust-worker-and-native-loader', external_network_calls: 0, ...receipt,
  }, null, 2));
});
