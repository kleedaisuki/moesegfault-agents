import assert from 'node:assert/strict';
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { createBrowserPrincipal } from './browser-principal.mjs';

const origin = 'https://agents.test';
const instructions = 'Counter fixture fixed instructions.';

// Only the compiled Rust Worker is called. Its existing outbound fixture rejects
// any attempted DeepSeek inference, tokenizer download, or media fetch.
export async function runDeepSeekTokenizerJourney({ mf, db, encryptionKey, request, diagnostic }) {
  const principal = await createBrowserPrincipal({ mf, db, encryptionKey, owner: 'deepseek-exact-counter-owner' });
  // Exercise the real account entitlement sync before reserving count resources.
  await principal.browser('GET', '/v1/account');
  const credential = await principal.browser('POST', '/v1/provider-keys', {
    protocol: 'openai', base_url: 'https://api.deepseek.com',
    api_key: 'synthetic-count-only-never-leaves-worker',
  }, 201);
  const model = name => ({ provider: 'byok', credential_id: credential.id, protocol: 'responses',
    model: name, context_window: 1048576, max_output_tokens: 512 });
  const agent = await request('POST', '/v1/agents', {
    name: 'Official DeepSeek counter', instructions, model: model('deepseek-flash'),
    tools: [], skills: [], workspace: false, compaction_percent: 80,
  }, 201, principal.apiKey);
  const corpus = JSON.parse(await readFile(new URL('../fixtures/deepseek-token-count.json', import.meta.url), 'utf8'));
  const samples = [];
  const state = async () => ({
    sessions: (await db.prepare('SELECT COUNT(*) AS count FROM sessions WHERE account_id=?').bind(principal.owner).first()).count,
    model_entries: (await db.prepare("SELECT COUNT(*) AS count FROM usage_ledger l JOIN usage_reservations r ON r.id=l.reservation_id WHERE r.account_id=? AND l.resource='model'").bind(principal.owner).first()).count,
  });
  const before = await state();
  async function count(body, label, expected) {
    const started = performance.now();
    const response = await mf.dispatchFetch(origin + '/v1/responses/input_tokens', {
      method: 'POST', headers: { Authorization: `Bearer ${principal.apiKey}`, 'Content-Type': 'application/json', 'CF-Connecting-IP': '192.0.2.81' },
      body: JSON.stringify({ ...body, model: agent.id }),
    });
    const text = await response.text();
    assert.equal(response.status, 200, `${label}: ${text}`);
    const value = JSON.parse(text);
    assert.equal(value.object, 'response.input_tokens');
    assert.ok(Number.isSafeInteger(value.input_tokens) && value.input_tokens > 0);
    if (expected !== undefined) assert.equal(value.input_tokens, expected, label);
    samples.push({ label, input_tokens: value.input_tokens, wall_ms: performance.now() - started });
    return value.input_tokens;
  }
  try {
    for (const fixture of corpus.fixtures) {
      await count(fixture.public_body ?? fixture.body, `flash-${fixture.name}`, fixture.public_input_tokens);
    }
    await count(corpus.fixtures[0].body, 'flash-warm-repeat', corpus.fixtures[0].public_input_tokens);
    await request('PATCH', `/v1/agents/${agent.id}`, { model: model('deepseek-v4-pro') }, 200, principal.apiKey);
    const pro = await count(corpus.fixtures[0].body, 'pro-cold-profile-replacement');
    await count(corpus.fixtures[0].body, 'pro-warm-repeat', pro);
    await request('PATCH', `/v1/agents/${agent.id}`, { model: model('deepseek-flash') }, 200, principal.apiKey);
    await count(corpus.fixtures[0].body, 'flash-profile-replacement', corpus.fixtures[0].public_input_tokens);
    assert.deepEqual(await state(), before, 'Counting must not allocate a Session or charge model inference');
    const reservations = (await db.prepare("SELECT state,session_id FROM usage_reservations WHERE account_id=? AND idempotency_key LIKE '%token_count%'").bind(principal.owner).all()).results;
    assert.equal(reservations.length, samples.length, 'Every count has its own accounted resource operation');
    assert.ok(reservations.every(row => row.session_id === null && row.state !== 'reserved'));
    const receipt = { schema: 1, source: process.env.GITHUB_SHA ?? 'local-uncommitted', api_calls: 0,
      actual_compiled_worker: true, cold_warm_and_profile_switch: true,
      independent_reference: corpus.reference, reference_sha256: corpus.reference_sha256,
      public_prepared_prefix: instructions, samples,
      memory_evidence: 'provider.count.local events contain wasm_linear_memory_bytes; linear memory is not total isolate heap' };
    await mkdir('.cache/perf/v3', { recursive: true });
    await writeFile('.cache/perf/v3/deepseek-token-count-workerd.json', JSON.stringify(receipt, null, 2) + '\n');
    diagnostic?.(`DeepSeek count-only Worker receipt: ${JSON.stringify(receipt)}`);
  } finally {
    await request('DELETE', `/v1/agents/${agent.id}`, undefined, 200, principal.apiKey);
  }
}
