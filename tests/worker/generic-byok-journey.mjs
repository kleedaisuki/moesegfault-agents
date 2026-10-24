import assert from 'node:assert/strict';
import { createBrowserPrincipal } from './browser-principal.mjs';

const redirectTarget = 'https://redirect-generic.example.com/steal';
const answer = 'Generic credential time completed.';
const thought = { type: 'thinking', thinking: 'Use the clock.', signature: 'fixture-signed-thought' };
const usage = { input_tokens: 100, cache_creation_input_tokens: 20, cache_read_input_tokens: 30, output_tokens: 0 };
const event = value => `data: ${JSON.stringify(value)}\n\n`;
const sse = body => new Response(body, { headers: { 'Content-Type': 'text/event-stream' } });

function anthropicResponse(state, first, truncated = false) {
  const frames = [{ type: 'message_start', message: { id: `generic-${state.calls}`, type: 'message', role: 'assistant', model: state.model, content: [], stop_reason: null, usage } }];
  if (first) {
    frames.push(
      { type: 'content_block_start', index: 0, content_block: { type: 'thinking', thinking: '' } },
      { type: 'content_block_delta', index: 0, delta: { type: 'thinking_delta', thinking: thought.thinking } },
      { type: 'content_block_delta', index: 0, delta: { type: 'signature_delta', signature: thought.signature } },
      { type: 'content_block_stop', index: 0 },
      { type: 'content_block_start', index: 1, content_block: { type: 'tool_use', id: state.toolId, name: 'time', input: {} } },
      { type: 'content_block_delta', index: 1, delta: { type: 'input_json_delta', partial_json: '{}' } },
      { type: 'content_block_stop', index: 1 },
    );
  } else {
    frames.push(
      { type: 'content_block_start', index: 0, content_block: { type: 'text', text: '' } },
      { type: 'content_block_delta', index: 0, delta: { type: 'text_delta', text: answer } },
      { type: 'content_block_stop', index: 0 },
    );
  }
  // Anthropic output usage is cumulative; a second delta replaces, not adds to, 5.
  frames.push({ type: 'message_delta', delta: {}, usage: { output_tokens: 5 } },
    { type: 'message_delta', delta: { stop_reason: first ? 'tool_use' : 'end_turn', stop_sequence: null }, usage: { output_tokens: first ? 10 : 15 } });
  if (!truncated) frames.push({ type: 'message_stop' });
  return sse(frames.map(event).join(''));
}

function openaiResponse(state, first, truncated = false) {
  const delta = first ? { role: 'assistant', tool_calls: [{ index: 0, id: state.toolId, type: 'function', function: { name: 'time', arguments: '{}' } }] }
    : { role: 'assistant', content: answer };
  const body = event({ id: `generic-${state.calls}`, model: state.model, choices: [{ index: 0, delta, finish_reason: first ? 'tool_calls' : 'stop' }],
    usage: { prompt_tokens: 150, completion_tokens: first ? 10 : 15, total_tokens: first ? 160 : 165, prompt_tokens_details: { cached_tokens: 30 }, cost: 0.001234 } });
  return sse(body + (truncated ? '' : 'data: [DONE]\n\n'));
}

function checkTime(value) {
  const clock = JSON.parse(value);
  assert.ok(Number.isSafeInteger(clock.unix_ms));
  assert.ok(Math.abs(clock.unix_ms - Date.now()) < 10000);
}

function checkRequest(state, request, input) {
  assert.equal(input.model, state.model);
  assert.equal(input.stream, true);
  if (state.protocol === 'openai') {
    assert.ok(request.headers.get('authorization') === `Bearer ${state.key}`, 'Generic OpenAI calls must use only the rotated user key');
    assert.equal(request.headers.get('x-api-key'), null);
    assert.ok(input.tools.some(tool => tool.type === 'function' && tool.function.name === 'time'));
    if (state.round === 2) checkTime(input.messages.find(message => message.role === 'tool' && message.tool_call_id === state.toolId).content);
    return;
  }
  assert.ok(request.headers.get('x-api-key') === state.key, 'Anthropic calls must use the exact owned x-api-key');
  assert.equal(request.headers.get('authorization'), null, 'Never leak a platform Bearer key to an Anthropic endpoint');
  assert.equal(request.headers.get('anthropic-version'), '2023-06-01');
  assert.equal(input.max_tokens, 2048);
  assert.ok(input.tools.some(tool => tool.name === 'time' && tool.input_schema.type === 'object'));
  assert.deepEqual(input.system.at(-1).cache_control, { type: 'ephemeral' });
  if (state.round !== 2) return;
  const assistant = input.messages.find(message => message.role === 'assistant' && message.content.some(block => block.type === 'tool_use'));
  assert.deepEqual(assistant.content, [thought, { type: 'tool_use', id: state.toolId, name: 'time', input: {} }]);
  const result = input.messages.flatMap(message => message.content).find(block => block.type === 'tool_result' && block.tool_use_id === state.toolId);
  checkTime(result.content);
}

export function createGenericProviderFixture() {
  const routes = new Map();
  let redirectCalls = 0;
  return {
    get redirectCalls() { return redirectCalls; },
    register(protocol) {
      const baseUrl = `https://api.${protocol}.example.com/v1`;
      const state = { protocol, baseUrl, model: `custom-unregistered-${protocol}`, toolId: `generic-time-${protocol}`, key: `synthetic-${protocol}-rotated-user-key`, mode: 'normal', calls: 0, round: 0, failure: null };
      routes.set(baseUrl + (protocol === 'anthropic' ? '/messages' : '/chat/completions'), state);
      return state;
    },
    async handle(request) {
      if (request.url === redirectTarget) {
        redirectCalls++;
        throw new Error('Generic credential redirects must never forward credentials');
      }
      const state = routes.get(request.url);
      if (!state) return undefined;
      state.calls++;
      state.round++;
      try {
        const input = await request.json();
        checkRequest(state, request, input);
        if (state.mode === 'redirect') return new Response(null, { status: 302, headers: { Location: redirectTarget } });
        if (state.mode === 'malformed') return sse('data: {invalid-json}\n\n');
        if (state.mode === 'truncated') return state.protocol === 'anthropic' ? anthropicResponse(state, false, true) : openaiResponse(state, false, true);
        assert.ok(state.round <= 2, 'A generic BYOK turn must stop after its answer');
        return state.protocol === 'anthropic' ? anthropicResponse(state, state.round === 1) : openaiResponse(state, state.round === 1);
      } catch (error) {
        state.failure = error;
        throw error;
      }
    },
  };
}

async function bounded(operation) {
  let timer;
  try {
    return await Promise.race([operation, new Promise((_, reject) => { timer = setTimeout(() => reject(new Error('Generic BYOK turn exceeded 8 seconds')), 8000); })]);
  } finally { clearTimeout(timer); }
}

async function chat(mf, principal, agent, session, stream) {
  return mf.dispatchFetch('https://agents.test/v1/chat/completions', {
    method: 'POST', headers: { Authorization: `Bearer ${principal.apiKey}`, 'Content-Type': 'application/json', 'X-Agents-Session': session.id },
    body: JSON.stringify({ model: agent.id, messages: [{ role: 'user', content: 'Call time and report its result.' }], stream }),
  });
}

async function assertNoHolds(db, owner) {
  const wallet = await db.prepare('SELECT CAST(spent_microusd AS TEXT) AS spent,CAST(reserved_microusd AS TEXT) AS held FROM usage_accounts WHERE account_id=?1').bind(owner).first();
  assert.ok(BigInt(wallet.spent) > 0n, 'Generic BYOK must still pay metered platform resources');
  assert.equal(wallet.held, '0');
}

export async function runGenericByokJourney({ mf, db, encryptionKey, request, providers, protocol }) {
  const principal = await createBrowserPrincipal({ mf, db, encryptionKey, owner: `generic-${protocol}-owner` });
  const state = providers.register(protocol);
  const suppliedUrl = protocol === 'anthropic' ? new URL(state.baseUrl).origin : state.baseUrl + '/';
  const credential = await principal.browser('POST', '/v1/provider-keys', { protocol, base_url: suppliedUrl, name: 'Generic fixture', api_key: 'synthetic-pre-rotation-user-key' }, 201);
  assert.ok(credential.id && credential.id !== protocol);
  assert.equal(credential.protocol, protocol);
  assert.equal(credential.base_url, state.baseUrl);
  assert.equal(credential.api_key, undefined);
  const stored = await db.prepare('SELECT key_encrypted,key_version FROM provider_keys WHERE account_id=?1 AND id=?2').bind(principal.owner, credential.id).first();
  assert.notEqual(stored.key_encrypted, 'synthetic-pre-rotation-user-key');
  const sameProtocol = await principal.browser('POST', '/v1/provider-keys', { protocol, base_url: suppliedUrl, api_key: 'synthetic-independent-credential-key' }, 201);
  assert.notEqual(sameProtocol.id, credential.id, 'Users may own multiple independent credentials of one protocol');
  await principal.browser('POST', '/v1/provider-keys', { protocol, base_url: 'https://127.0.0.1/v1', api_key: state.key }, 400);
  const foreignPrincipal = await createBrowserPrincipal({ mf, db, encryptionKey, owner: `generic-${protocol}-foreign` });
  await foreignPrincipal.browser('PUT', `/v1/provider-keys/${credential.id}`, { api_key: 'synthetic-foreign-change-denied' }, 404);
  await foreignPrincipal.browser('DELETE', `/v1/provider-keys/${credential.id}`);
  assert.ok(!(await foreignPrincipal.browser('GET', '/v1/provider-keys')).data.some(row => row.id === credential.id));
  const model = { provider: 'byok', credential_id: credential.id, model: state.model, context_window: 98304, max_output_tokens: 2048 };
  const foreign = await request('POST', '/v1/agents', { name: 'Denied generic credential', model }, 400);
  assert.equal(foreign.error.code, 'invalid_request_error');
  const agent = await request('POST', '/v1/agents', { name: `Generic ${protocol}`, model, tools: ['time'], workspace: false }, 201, principal.apiKey);
  assert.deepEqual(agent.model, model, 'Generic models must retain explicit budgets without a platform registry entry');
  const session = await request('POST', `/v1/agents/${agent.id}/sessions`, {}, 201, principal.apiKey);
  await principal.browser('PUT', `/v1/provider-keys/${credential.id}`, { api_key: state.key, name: 'Rotated name' });
  const rotated = await db.prepare('SELECT key_encrypted,key_version FROM provider_keys WHERE account_id=?1 AND id=?2').bind(principal.owner, credential.id).first();
  assert.equal(rotated.key_version, stored.key_version + 1);
  assert.notEqual(rotated.key_encrypted, stored.key_encrypted);
  for (const changed of [{ protocol: protocol === 'openai' ? 'anthropic' : 'openai', base_url: state.baseUrl }, { protocol, base_url: 'https://different.example.com/v1' }]) {
    const rejected = await principal.browser('PUT', `/v1/provider-keys/${credential.id}`, { ...changed, api_key: 'synthetic-route-change-denied' }, 409);
    assert.equal(rejected.error.code, 'credential_route_immutable');
  }
  const listedRows = (await principal.browser('GET', '/v1/provider-keys')).data;
  assert.ok(listedRows.every(row => !('api_key' in row) && !('key_encrypted' in row)));
  const listed = listedRows.find(row => row.id === credential.id);
  assert.equal(listed.protocol, protocol);
  assert.equal(listed.base_url, state.baseUrl);
  assert.equal(listed.name, 'Rotated name');
  const response = await bounded(chat(mf, principal, agent, session, true));
  assert.equal(response.status, 200);
  const wire = await bounded(response.text());
  if (state.failure) throw state.failure;
  assert.ok(wire.trimEnd().endsWith('data: [DONE]'));
  const chunks = wire.split('\n\n').filter(frame => frame.startsWith('data: ') && frame !== 'data: [DONE]').map(frame => JSON.parse(frame.slice(6)));
  assert.ok(chunks.some(chunk => chunk.choices?.some(choice => choice.delta?.content?.includes(answer))));
  const reported = chunks.find(chunk => chunk.usage)?.usage;
  assert.deepEqual(reported, { prompt_tokens: 300, completion_tokens: 25, total_tokens: 325, prompt_tokens_details: { cached_tokens: 60 } });
  assert.equal(state.calls, 2);
  const completed = await request('GET', `/v1/sessions/${session.id}`, undefined, 200, principal.apiKey);
  assert.deepEqual(completed.pinned, session.pinned, 'Rotating a key/name must not change an existing session prefix');
  // The next prompt retains the last assistant completion; run usage aggregates all rounds.
  assert.equal(completed.context_tokens, 150 + 15);
  if (protocol === 'anthropic') assert.deepEqual(completed.messages.find(message => message.tool_calls?.length).anthropic_content[0], thought);
  await request('DELETE', `/v1/sessions/${session.id}`, undefined, 200, principal.apiKey);
  await assertNoHolds(db, principal.owner);
  const costs = await db.prepare("SELECT CAST(l.amount_microusd AS TEXT) AS amount FROM usage_ledger l JOIN usage_reservations r ON r.id=l.reservation_id WHERE r.account_id=?1 AND l.resource='model'").bind(principal.owner).all();
  assert.deepEqual(costs.results, [{ amount: '0' }, { amount: '0' }]);
  for (const mode of ['redirect', 'malformed', 'truncated']) {
    await request('POST', `/v1/sessions/${session.id}/resume`, {}, 200, principal.apiKey);
    state.mode = mode;
    state.round = 0;
    const calls = state.calls;
    const result = await bounded(chat(mf, principal, agent, session, false));
    if (state.failure) throw state.failure;
    assert.equal(result.status, 502, `${protocol} ${mode} must fail closed`);
    assert.equal((await result.json()).error.code, 'provider_error');
    assert.equal(state.calls, calls + 1, 'A failed transport must not replay or fall back to a platform provider');
    assert.equal(providers.redirectCalls, 0);
    const failed = await request('GET', `/v1/sessions/${session.id}`, undefined, 200, principal.apiKey);
    assert.equal(failed.messages.filter(message => message.role === 'assistant').length, completed.messages.filter(message => message.role === 'assistant').length, 'Incomplete provider responses must never be committed as assistant answers');
    await request('DELETE', `/v1/sessions/${session.id}`, undefined, 200, principal.apiKey);
    await assertNoHolds(db, principal.owner);
  }
  await principal.browser('DELETE', `/v1/provider-keys/${credential.id}`);
  await principal.browser('DELETE', `/v1/provider-keys/${sameProtocol.id}`);
  assert.ok(!(await principal.browser('GET', '/v1/provider-keys')).data.some(row => row.id === credential.id));
}
