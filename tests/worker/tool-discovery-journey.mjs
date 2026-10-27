import assert from 'node:assert/strict';
import OpenAI from 'openai';
import { createBrowserPrincipal } from './browser-principal.mjs';

const encoder = new TextEncoder();
const search = '__agents_tool_search';
const invoke = '__agents_invoke_discovered';
const usage = { input_tokens: 100, output_tokens: 10, total_tokens: 110,
  input_tokens_details: { cached_tokens: 0, cache_write_tokens: 0 }, output_tokens_details: { reasoning_tokens: 0 } };
export const discoveryTools = [{ type: 'namespace', name: 'crm', description: 'Customer order tools', tools: [
  { type: 'function', name: 'orders', description: 'Find open orders', defer_loading: true, strict: true,
    parameters: { type: 'object', properties: { id: { type: 'string' } }, required: ['id'], additionalProperties: false } },
] }, { type: 'tool_search' }];
const call = (id, name, arguments_) => ({ type: 'function_call', id: `upstream-${id}`, call_id: id,
  name, arguments: JSON.stringify(arguments_), status: 'completed' });
const answer = text => ({ type: 'message', id: 'upstream-message', role: 'assistant', status: 'completed',
  content: [{ type: 'output_text', text, annotations: [], logprobs: [] }] });

function response(output, model, id) {
  const frames = [];
  const push = (type, fields) => frames.push({ type, sequence_number: frames.length, ...fields });
  push('response.created', { response: { id, object: 'response', status: 'in_progress' } });
  output.forEach((item, output_index) => {
    push('response.output_item.added', { output_index, item: { ...item, status: 'in_progress',
      ...(item.type === 'function_call' ? { arguments: '' } : { content: [] }) } });
    if (item.type === 'function_call') {
      const at = { output_index, item_id: item.id };
      const half = Math.floor(item.arguments.length / 2);
      push('response.function_call_arguments.delta', { ...at, delta: item.arguments.slice(0, half) });
      push('response.function_call_arguments.delta', { ...at, delta: item.arguments.slice(half) });
      push('response.function_call_arguments.done', { ...at, arguments: item.arguments, name: item.name });
    }
    if (item.type === 'message') {
      item.content.forEach((part, content_index) => {
        const at = { output_index, item_id: item.id, content_index };
        push('response.content_part.added', { ...at, part: { ...part, text: '' } });
        push('response.output_text.delta', { ...at, delta: part.text, logprobs: [] });
        push('response.output_text.done', { ...at, text: part.text, logprobs: [] });
        push('response.content_part.done', { ...at, part });
      });
    }
    push('response.output_item.done', { output_index, item });
  });
  push('response.completed', { response: { id, model, object: 'response', status: 'completed', output, usage, error: null, incomplete_details: null } });
  return new Response(new ReadableStream({ start(controller) {
    for (const frame of frames) controller.enqueue(encoder.encode(`event: ${frame.type}\ndata: ${JSON.stringify(frame)}\n\n`));
    controller.close();
  } }), { headers: { 'Content-Type': 'text/event-stream' } });
}

export function createDiscoveryProvider() {
  const states = new Map();
  return {
    register(name, actions, origin = 'https://api.deepseek.com') {
      const key = `synthetic-discovery-${name}`;
      const state = { key, origin, model: 'deepseek-flash', requests: [], actions, calls: 0, failure: null };
      states.set(key, state);
      return state;
    },
    async handle(request) {
      const state = states.get(request.headers.get('Authorization')?.replace(/^Bearer /, ''));
      if (!state) return undefined;
      try {
        assert.equal(new URL(request.url).origin, state.origin);
        const body = await request.json();
        assert.equal(request.method, 'POST');
        if (new URL(request.url).pathname.endsWith('/responses/input_tokens')) return Response.json({ object: 'response.input_tokens', input_tokens: 100 });
        assert.equal(new URL(request.url).pathname, '/responses');
        assert.equal(body.store, false);
        assert.equal(body.stream, true);
        assert.equal(body.model, state.model);
        const action = state.actions[state.calls++];
        assert.ok(action, 'No unexpected inference or hidden repair is allowed');
        action.check?.(body, state);
        state.requests.push(body);
        return response(action.output, state.model, `upstream-discovery-${state.calls}`);
      } catch (error) { state.failure = error; throw error; }
    },
  };
}

async function setup(context, name, actions, origin) {
  const principal = await createBrowserPrincipal({ ...context, owner: `discovery-${name}-owner` });
  const state = context.providers.register(name, actions, origin);
  const credential = await principal.browser('POST', '/v1/provider-keys', { protocol: 'openai',
    base_url: state.origin, api_key: state.key }, 201);
  const agent = await context.request('POST', '/v1/agents', { name: `Discovery ${name}`, instructions: 'PINNED-DISCOVERY-PREFIX',
    model: { provider: 'byok', credential_id: credential.id, protocol: 'responses', model: state.model,
      context_window: 32768, max_output_tokens: 512 }, workspace: false, tools: [], compaction_percent: 80 }, 201, principal.apiKey);
  const client = new OpenAI({ apiKey: principal.apiKey, baseURL: 'https://agents.test/v1', maxRetries: 0,
    timeout: 15000, fetch: (input, init) => context.mf.dispatchFetch(input, init) });
  return { principal, state, agent, client };
}

async function collect(client, body, options) {
  const events = [];
  for await (const event of await client.responses.create({ ...body, stream: true }, options)) events.push(event);
  let sequence = -1;
  for (const event of events) {
    assert.ok(event.sequence_number > sequence);
    sequence = event.sequence_number;
  }
  assert.ok(!JSON.stringify(events).includes('__agents_'), 'Private dispatch identities never leak into public events');
  const terminal = events.findLast(event => ['response.completed', 'response.failed'].includes(event.type));
  assert.ok(terminal, 'A genuine canonical terminal is required');
  return { events, value: terminal.response };
}

function schemaWasLoaded(body, state) {
  assert.deepEqual(body.tools, state.requests[0].tools, 'Discovery must not invalidate the stable tool declaration prefix');
  assert.deepEqual(body.input[0], state.requests[0].input[0], 'Pinned instructions remain byte-for-Value stable');
  assert.ok(body.input.some(item => item.type === 'function_call_output' && JSON.stringify(item.output).includes('orders')),
    'The ordinary supported provider tool result contains the loaded schema');
  assert.ok(!body.input.some(item => item.type === 'tool_search_output'), 'DeepSeek must not depend on an ignored native item type');
}

export async function runServerDiscovery(context) {
  const scenario = await setup(context, 'server', [
    { output: [call('load', search, { paths: ['crm'] })], check(body) {
      assert.ok(body.tools.some(tool => tool.name === search));
      assert.ok(body.tools.some(tool => tool.name === invoke));
      assert.ok(!JSON.stringify(body.tools).includes('"required":["id"]'), 'Deferred parameter schema is not loaded at the prefix');
    } },
    { output: [call('orders', invoke, { tool: 'crm.orders', arguments: { id: 'C1' } })], check: schemaWasLoaded },
    { output: [answer('订单已读取。')], check(body, state) {
      schemaWasLoaded(body, state);
      assert.ok(body.input.some(item => item.type === 'function_call_output' && item.call_id === 'orders' && item.output === 'client-order-result'));
    } },
  ]);
  const body = { model: scenario.agent.id, input: 'Find the open orders', tools: discoveryTools, reasoning: { effort: 'none' } };
  const result = await collect(scenario.client, body, { headers: { 'Idempotency-Key': 'discovery-first' } });
  if (scenario.state.failure) throw scenario.state.failure;
  assert.equal(result.value.status, 'completed');
  assert.deepEqual(result.value.output.map(item => item.type), ['tool_search_call', 'tool_search_output', 'function_call']);
  assert.equal(result.value.output[0].call_id, null);
  assert.equal(result.value.output[1].tools[0].tools[0].name, 'orders');
  const pending = result.value.output[2];
  assert.equal(pending.name, 'orders');
  assert.equal(pending.namespace, 'crm');
  assert.deepEqual(JSON.parse(pending.arguments), { id: 'C1' });
  assert.equal(scenario.state.calls, 2);
  const replay = (await collect(scenario.client, body, { headers: { 'Idempotency-Key': 'discovery-first' } })).value;
  assert.equal(replay.id, result.value.id);
  assert.equal(scenario.state.calls, 2, 'A same-key retry never repeats discovery or inference');
  const next = await collect(scenario.client, { model: scenario.agent.id, tools: discoveryTools, previous_response_id: result.value.id,
    input: [{ type: 'function_call_output', call_id: pending.call_id, output: 'client-order-result' }], reasoning: { effort: 'none' } });
  if (scenario.state.failure) throw scenario.state.failure;
  assert.equal(next.value.status, 'completed');
  assert.equal(scenario.state.calls, 3);
  const rows = await context.db.prepare("SELECT CAST(l.quantity AS TEXT) quantity FROM usage_ledger l JOIN usage_reservations r ON r.id=l.reservation_id WHERE r.account_id=? AND l.resource='model'").bind(scenario.principal.owner).all();
  assert.deepEqual(rows.results.map(row => row.quantity), ['110', '110', '110']);
  return { provider_calls: 3, stable_initial_tools: true, canonical_namespace: true, replay_calls: 0 };
}

export async function runInvalidDiscoveryArguments(context) {
  const scenario = await setup(context, 'invalid-inner', [
    { output: [call('load', search, { paths: ['crm'] })] },
    { output: [call('bad', invoke, { tool: 'crm.orders', arguments: { id: 42 } })], check: schemaWasLoaded },
  ]);
  const result = await collect(scenario.client, { model: scenario.agent.id, tools: discoveryTools,
    input: 'Disobedient synthetic strict tool', reasoning: { effort: 'none' } });
  if (scenario.state.failure) throw scenario.state.failure;
  assert.equal(result.value.status, 'failed');
  assert.ok(!result.events.some(event => event.type === 'response.output_item.done' && event.item.type === 'function_call'),
    'Invalid inner parameters never become a completed client handoff');
  assert.equal(scenario.state.calls, 2, 'Server strict validation never performs a hidden paid repair');
  const rows = await context.db.prepare("SELECT CAST(l.quantity AS TEXT) quantity FROM usage_ledger l JOIN usage_reservations r ON r.id=l.reservation_id WHERE r.account_id=? AND l.resource='model'").bind(scenario.principal.owner).all();
  assert.equal(rows.results.length, 2);
  assert.ok(rows.results.every(row => row.quantity === '110'), 'Both known disobedient rounds are still accounted');
  return { provider_calls: 2, invalid_handoffs: 0, hidden_repairs: 0, known_usage_settlements: 2 };
}

export async function runNativeDiscovery(context) {
  const scenario = await setup(context, 'native', [
    { output: [call('byok-search', search, { paths: ['crm'] })], check(body) {
      assert.ok(body.tools.some(tool => tool.name === search));
      assert.ok(!body.tools.some(tool => tool.type === 'tool_search'));
    } },
    { output: [call('byok-orders', invoke, { tool: 'crm.orders', arguments: { id: 'C1' } })], check: schemaWasLoaded },
    { output: [answer('BYOK local-authority discovery completed.')], check(body) {
      assert.ok(body.input.some(item => item.type === 'function_call_output' && item.call_id === 'byok-orders'));
    } },
  ], 'https://discovery-native.provider.example');
  const result = await collect(scenario.client, { model: scenario.agent.id, tools: discoveryTools, input: 'Use local-authority discovery.' });
  if (scenario.state.failure) throw scenario.state.failure;
  assert.equal(result.value.status, 'completed');
  assert.deepEqual(result.value.output.map(item => item.type), ['tool_search_call', 'tool_search_output', 'function_call']);
  const pending = result.value.output[2];
  const next = await collect(scenario.client, { model: scenario.agent.id, tools: discoveryTools, previous_response_id: result.value.id,
    input: [{ type: 'function_call_output', call_id: pending.call_id, output: 'native-result' }] });
  if (scenario.state.failure) throw scenario.state.failure;
  assert.equal(next.value.status, 'completed');
  assert.equal(scenario.state.calls, 3);
  return { provider_calls: 3, local_search_authority: true, owned_loaded_schemas_retained: true };
}

export async function runInvalidClientSearch(context) {
  const scenario = await setup(context, 'client-invalid', [{ output: [call('client-search', search, {})] }]);
  const tools = [{ type: 'tool_search', execution: 'client', parameters: { type: 'object', properties: { query: { type: 'string' } }, required: ['query'], additionalProperties: false } }];
  const result = await collect(scenario.client, { model: scenario.agent.id, input: 'Invalid client search fixture', tools, reasoning: { effort: 'none' } });
  if (scenario.state.failure) throw scenario.state.failure;
  assert.equal(result.value.status, 'failed');
  assert.ok(!result.events.some(event => event.type === 'response.output_item.done' && event.item.type === 'tool_search_call'),
    'Invalid client-search arguments never publish completed handoff before server validation');
  assert.equal(scenario.state.calls, 1);
  return { provider_calls: 1, completed_invalid_search_handoffs: 0 };
}

export async function runCallerAuthority(context) {
  let calls = 0;
  for (const forged of [false, true]) {
    const item = call('program-only', 'only_program', {});
    if (forged) item.caller = { type: 'program', caller_id: 'not-a-runtime-authorization' };
    const scenario = await setup(context, forged ? 'caller-forged' : 'caller-direct', [{ output: [item] }]);
    const result = await collect(scenario.client, { model: scenario.agent.id, input: 'Programmatic caller negative fixture', reasoning: { effort: 'none' },
      tools: [{ type: 'function', name: 'only_program', allowed_callers: ['programmatic'], parameters: { type: 'object' } }] });
    if (scenario.state.failure) throw scenario.state.failure;
    assert.equal(result.value.status, 'failed');
    assert.ok(!result.events.some(event => event.type === 'response.output_item.done' && event.item.type === 'function_call'),
      'Neither direct nor provider-forged program caller can publish a completed restricted handoff');
    assert.equal(scenario.state.calls, 1);
    calls += scenario.state.calls;
  }
  return { provider_calls: calls, unauthorized_handoffs: 0, raw_program_metadata_grants_authority: false };
}
