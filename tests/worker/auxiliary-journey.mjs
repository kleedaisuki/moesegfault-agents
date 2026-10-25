import assert from 'node:assert/strict';
import OpenAI from 'openai';
import { createBrowserPrincipal } from './browser-principal.mjs';

export function createAuxiliaryProviderFixture() {
  const state = { calls: [], counts: [], missingDetail: false };
  return { state, async handle(request) {
    const url = new URL(request.url);
    if (url.hostname !== 'auxiliary-provider.test') return undefined;
    assert.equal(request.headers.get('authorization'), 'Bearer synthetic-auxiliary-owned-key');
    const input = await request.json();
    assert.equal(input.model, 'fixture-native-model');
    if (url.pathname === '/v1/responses/input_tokens') {
      state.counts.push(input);
      return Response.json({ object: 'response.input_tokens', input_tokens: 42 });
    }
    assert.equal(url.pathname, '/v1/responses');
    assert.equal(input.stream, true);
    assert.equal(input.store, false);
    state.calls.push(input);
    const usage = { input_tokens: 12, output_tokens: 3, total_tokens: 15,
      input_tokens_details: { cached_tokens: 2, ...(state.missingDetail ? {} : { cache_write_tokens: 0 }) },
      output_tokens_details: { reasoning_tokens: 1 } };
    const response = { id: `upstream-aux-${state.calls.length}`, object: 'response', model: input.model,
      status: 'completed', output: [{ id: 'upstream-message', type: 'message', role: 'assistant', status: 'completed',
        content: [{ type: 'output_text', text: 'Preserved goal and evidence 雪.', annotations: [] }] }], usage };
    return new Response(`event: response.completed\ndata: ${JSON.stringify({ type: 'response.completed', response })}\n\n`,
      { headers: { 'Content-Type': 'text/event-stream' } });
  } };
}

export async function runAuxiliaryJourney({ mf, db, encryptionKey, request, provider }) {
  const principal = await createBrowserPrincipal({ mf, db, encryptionKey, owner: 'auxiliary-api-owner' });
  const credential = await principal.browser('POST', '/v1/provider-keys', {
    protocol: 'openai', base_url: 'https://auxiliary-provider.test/v1', api_key: 'synthetic-auxiliary-owned-key' }, 201);
  const agent = await request('POST', '/v1/agents', { name: 'Auxiliary native agent',
    model: { provider: 'byok', credential_id: credential.id, protocol: 'responses', model: 'fixture-native-model',
      context_window: 8192, max_output_tokens: 64 }, instructions: 'Pinned policy 雪.', tools: ['time'], workspace: false }, 201, principal.apiKey);
  const sdk = new OpenAI({ apiKey: principal.apiKey, baseURL: 'https://agents.test/v1', timeout: 15000, maxRetries: 0,
    fetch: (input, init) => mf.dispatchFetch(input, init) });
  const tools = [{ type: 'function', name: 'fixture_calculate', parameters: { type: 'object', properties: {} } }];
  const input = [{ role: 'user', content: [{ type: 'input_file', filename: 'notes.txt', file_data: Buffer.from('Count and preserve 雪.').toString('base64') }] }];
  const body = { model: agent.id, input, instructions: 'One-response scope.', tools };
  assert.deepEqual(await sdk.responses.inputTokens.count(body), { object: 'response.input_tokens', input_tokens: 42 });
  assert.equal(provider.state.calls.length, 0, 'Counting never buys inference');
  assert.equal(await db.prepare('SELECT COUNT(*) AS n FROM sessions WHERE account_id=?').bind(principal.owner).first('n'), 0);
  assert.equal(await db.prepare('SELECT COUNT(*) AS n FROM space_files WHERE account_id=?').bind(principal.owner).first('n'), 0);
  const made = await sdk.responses.create({ ...body, store: true }).withResponse();
  assert.equal(made.data.status, 'completed');
  assert.deepEqual(provider.state.counts[0].input, provider.state.calls[0].input, 'Count and generation share resolved input');
  assert.deepEqual(provider.state.counts[0].tools, provider.state.calls[0].tools);
  const sessionId = made.response.headers.get('X-Agents-Session');
  assert.ok(sessionId);
  const before = await db.prepare('SELECT config_snapshot_json FROM sessions WHERE id=?').bind(sessionId).first('config_snapshot_json');
  await sdk.responses.inputTokens.count({ previous_response_id: made.data.id, input: 'Continue.' });
  assert.ok(!JSON.stringify(provider.state.counts.at(-1).input).includes('One-response scope.'), 'Previous instructions do not leak into count continuation');
  const compactBody = { previous_response_id: made.data.id, input: 'Preserve the goal.', instructions: 'Compact in concise bullets.' };
  const window = await sdk.responses.compact(compactBody, { idempotencyKey: 'auxiliary-compact-key' });
  assert.equal(window.object, 'response.compaction');
  assert.equal(window.usage.input_tokens_details.cache_write_tokens, 0);
  assert.match(window.output.at(-1).encrypted_content, /^mcmp1\./);
  assert.ok(window.output.slice(0, -1).every(item => item.role === 'user'));
  assert.deepEqual(await sdk.responses.compact(compactBody, { idempotencyKey: 'auxiliary-compact-key' }), window);
  assert.equal(provider.state.calls.length, 2, 'Same-key compact does not repeat inference');
  const independentlyCompacted = await sdk.responses.compact(compactBody, { idempotencyKey: 'auxiliary-second-operation' });
  assert.notEqual(independentlyCompacted.output.at(-1).id, window.output.at(-1).id,
    'Different compact operations cannot publish different ciphertexts under the same item ID');
  const conversation = await sdk.conversations.create({ items: [window.output.at(-1)] });
  await sdk.conversations.items.create(conversation.id, { items: [independentlyCompacted.output.at(-1)] });
  assert.equal((await sdk.conversations.items.list(conversation.id)).data.length, 2,
    'Distinct operation items coexist under immutable Conversation identities');
  await assert.rejects(sdk.responses.compact({ ...compactBody, input: 'Changed' }, { idempotencyKey: 'auxiliary-compact-key' }), error => error.status === 409);
  await sdk.responses.inputTokens.count({ model: agent.id, input: [...window.output, { role: 'user', content: 'Continue.' }] });
  const restored = JSON.stringify(provider.state.counts.at(-1).input);
  assert.ok(restored.includes('One-response scope.'), 'Compaction retains protected ongoing instructions');
  assert.ok(!restored.includes('Compact in concise bullets.'), 'Compact guidance never becomes ongoing instructions');
  const resumedBody = { model: agent.id, input: [...window.output, { role: 'user', content: 'Continue.' }] };
  const resumed = await sdk.responses.create(resumedBody, { idempotencyKey: 'auxiliary-resume-key' });
  assert.equal(resumed.instructions, null, 'Private effective instructions do not rewrite the public request');
  assert.ok(JSON.stringify(provider.state.calls.at(-1).input).includes('One-response scope.'));
  assert.equal((await sdk.responses.create(resumedBody, { idempotencyKey: 'auxiliary-resume-key' })).id, resumed.id);
  assert.equal(provider.state.calls.length, 4, 'Raw-key replay remains exact after sealed instruction expansion');
  await sdk.responses.inputTokens.count({ previous_response_id: resumed.id, input: 'Next response.' });
  assert.ok(!JSON.stringify(provider.state.counts.at(-1).input).includes('One-response scope.'),
    'A later previous_response_id never implicitly inherits the private current-response override');
  const override = await sdk.responses.create({ ...resumedBody, previous_response_id: resumed.id, instructions: 'Explicit new override.' });
  const overridden = JSON.stringify(provider.state.calls.at(-1).input);
  assert.ok(overridden.includes('Explicit new override.'));
  assert.ok(!overridden.includes('One-response scope.'));
  const corrupted = structuredClone(window.output);
  corrupted.at(-1).encrypted_content += 'corrupt';
  const beforeRejected = provider.state.calls.length;
  await assert.rejects(sdk.responses.create({ model: agent.id, previous_response_id: override.id, input: corrupted }), error => error.status === 400);
  assert.equal(provider.state.calls.length, beforeRejected, 'Unverified compaction cannot reach a model call');
  assert.equal(await db.prepare('SELECT config_snapshot_json FROM sessions WHERE id=?').bind(sessionId).first('config_snapshot_json'), before);
  assert.equal(await db.prepare('SELECT COUNT(*) AS n FROM sessions WHERE account_id=?').bind(principal.owner).first('n'), 2,
    'Only explicit generation creates Sessions; auxiliary calls do not');
  provider.state.missingDetail = true;
  await assert.rejects(sdk.responses.compact({ model: agent.id, input: 'Known usage missing required detail.' },
    { idempotencyKey: 'auxiliary-missing-detail' }), error => error.status === 503 && error.code === 'provider_unsupported_usage_detail');
  provider.state.missingDetail = false;
  assert.equal(await db.prepare("SELECT COUNT(*) AS n FROM usage_ledger l JOIN usage_reservations r ON r.id=l.reservation_id WHERE r.account_id=? AND l.resource='model'")
    .bind(principal.owner).first('n'), 6, 'Known usage settles despite unsupported wire details');
  for (const endpoint of ['compact', 'input_tokens']) {
    const invalid = await mf.dispatchFetch(`https://agents.test/v1/responses/${endpoint}`, {
      method: 'POST', headers: { Authorization: `Bearer ${principal.apiKey}`, 'Content-Type': 'application/json' },
      body: JSON.stringify({ model: agent.id, input: 'hello', unknown: true }) });
    assert.equal(invalid.status, 400);
  }
}
