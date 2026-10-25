import assert from 'node:assert/strict';
import OpenAI from 'openai';
import { createBrowserPrincipal } from './browser-principal.mjs';

const encoder = new TextEncoder();
const secret = 'PRIVATE_NATIVE_REASONING_MUST_NOT_LEAK';
const ciphertext = 'PRIVATE_NATIVE_CIPHERTEXT_MUST_NOT_LEAK';
const answer = '原生流的第一片🙂与最后一片。';
const summary = 'Check the visible evidence.';
const annotation = { type: 'url_citation', start_index: 0, end_index: 2,
  title: 'Synthetic source', url: 'https://evidence.example.com/source' };
const logprobs = [{ token: '原', bytes: [229, 142, 159], logprob: -0.1, top_logprobs: [] }];
const usage = { input_tokens: 120, output_tokens: 30, total_tokens: 150,
  input_tokens_details: { cached_tokens: 12, cache_write_tokens: 0 },
  output_tokens_details: { reasoning_tokens: 5 } };

function latch() {
  let resolve;
  const promise = new Promise(done => { resolve = done; });
  return { promise, resolve };
}

async function deadline(operation, label, milliseconds = 12000) {
  let timer;
  try {
    return await Promise.race([operation, new Promise((_, reject) => {
      timer = setTimeout(() => reject(new Error(`${label}: observation deadline`)), milliseconds);
    })]);
  } finally { clearTimeout(timer); }
}

function nativeFrames(id, model, output, status = 'completed') {
  const frames = [];
  const push = (type, fields) => frames.push({ type, sequence_number: frames.length, ...fields });
  push('response.created', { response: { id, object: 'response', status: 'in_progress' } });
  for (const [output_index, item] of output.entries()) {
    const start = { ...item, status: 'in_progress' };
    if (item.type === 'message') start.content = [];
    if (item.type === 'reasoning') start.summary = [];
    if (item.type === 'custom_tool_call') start.input = '';
    if (item.type === 'function_call') start.arguments = '';
    push('response.output_item.added', { output_index, item: start });
    const at = { output_index, item_id: item.id };
    if (item.type === 'reasoning') {
      push('response.reasoning_summary_part.added', { ...at, summary_index: 0, part: { type: 'summary_text', text: '' } });
      push('response.reasoning_summary_text.delta', { ...at, summary_index: 0, delta: summary });
      push('response.reasoning_summary_text.done', { ...at, summary_index: 0, text: summary });
      push('response.reasoning_summary_part.done', { ...at, summary_index: 0, part: item.summary[0] });
    }
    if (item.type === 'message') {
      for (const [content_index, part] of item.content.entries()) {
        const pos = { ...at, content_index };
        const text = part.type === 'refusal' ? 'refusal' : 'text';
        push('response.content_part.added', { ...pos, part: { ...part, [text]: '' } });
        if (part.type === 'output_text') {
          // Deliberately split Unicode at arbitrary byte boundaries in the transport below.
          for (const delta of [part.text.slice(0, 4), part.text.slice(4)]) {
            push('response.output_text.delta', { ...pos, delta, logprobs });
          }
          for (const [annotation_index, value] of part.annotations.entries()) {
            push('response.output_text.annotation.added', { ...pos, annotation_index, annotation: value });
          }
          push('response.output_text.done', { ...pos, text: part.text, logprobs });
        } else {
          push('response.refusal.delta', { ...pos, delta: part.refusal });
          push('response.refusal.done', { ...pos, refusal: part.refusal });
        }
        push('response.content_part.done', { ...pos, part });
      }
    }
    if (item.type === 'custom_tool_call') {
      push('response.custom_tool_call_input.delta', { ...at, delta: item.input });
      push('response.custom_tool_call_input.done', { ...at, input: item.input });
    }
    if (item.type === 'function_call') {
      push('response.function_call_arguments.delta', { ...at, delta: item.arguments });
      push('response.function_call_arguments.done', { ...at, arguments: item.arguments, name: item.name });
    }
    push('response.output_item.done', { output_index, item });
  }
  push(`response.${status}`, { response: { id, object: 'response', model, status, output, usage,
    error: status === 'failed' ? { code: 'server_error', message: 'Synthetic known-usage failure' } : null,
    incomplete_details: null } });
  return frames;
}

function transport(frames, gate, state) {
  let closed = false;
  return new Response(new ReadableStream({
    async start(controller) {
      try {
        for (const [index, frame] of frames.entries()) {
          if (gate && index === frames.length - 1) await gate.promise;
          if (closed) return;
          const bytes = encoder.encode(`event: ${frame.type}\ndata: ${JSON.stringify(frame)}\n\n`);
          for (let offset = 0; offset < bytes.length;) {
            const length = 1 + (offset % 17);
            controller.enqueue(bytes.slice(offset, offset + length));
            offset += length;
          }
          if (index === frames.length - 1) state.terminalSent = true;
        }
        controller.close();
      } catch (error) { controller.error(error); }
    },
    cancel() { closed = true; gate?.resolve(); },
  }), { headers: { 'Content-Type': 'text/event-stream' } });
}

export function createNativeSemanticsProvider() {
  const routes = new Map();
  return {
    register(name, actions) {
      const state = { baseUrl: `https://native-${name}.example.com/v1`, model: `native-${name}`,
        key: `synthetic-native-${name}`, calls: 0, requests: [], actions, terminalSent: false, failure: null };
      routes.set(`${state.baseUrl}/responses`, state);
      return state;
    },
    async handle(request) {
      const state = routes.get(request.url);
      if (!state) return undefined;
      try {
        assert.equal(request.headers.get('authorization'), `Bearer ${state.key}`);
        const body = await request.json();
        assert.equal(body.store, false);
        assert.equal(body.stream, true);
        assert.equal(body.model, state.model);
        for (const key of ['previous_response_id', 'conversation', 'background']) assert.equal(body[key], undefined);
        const action = state.actions[state.calls++];
        assert.ok(action, 'Native continuation cannot dispatch unplanned/replayed inference');
        state.requests.push(body);
        action.check?.(body);
        const id = `upstream-native-${state.calls}`;
        return transport(nativeFrames(id, state.model, action.output, action.status), action.gate, state);
      } catch (error) { state.failure = error; throw error; }
    },
  };
}

async function setup(context, name, actions) {
  const { mf, db, encryptionKey, providers, request } = context;
  const principal = await createBrowserPrincipal({ mf, db, encryptionKey, owner: `native-${name}-owner` });
  const state = providers.register(name, actions);
  const credential = await principal.browser('POST', '/v1/provider-keys', {
    protocol: 'openai', base_url: state.baseUrl, api_key: state.key,
  }, 201);
  const agent = await request('POST', '/v1/agents', { name: `Native semantics ${name}`,
    instructions: 'NATIVE-IMMUTABLE-PREFIX', model: { provider: 'byok', credential_id: credential.id,
      protocol: 'responses', model: state.model, context_window: 32768, max_output_tokens: 512 },
    workspace: false, tools: [], compaction_percent: 80 }, 201, principal.apiKey);
  async function invoke(body, expected = 200, key) {
    const result = await mf.dispatchFetch('https://agents.test/v1/responses', {
      method: 'POST', headers: { Authorization: `Bearer ${principal.apiKey}`, 'Content-Type': 'application/json',
        ...(key ? { 'Idempotency-Key': key } : {}) }, body: JSON.stringify({ model: agent.id, ...body }),
    });
    if (body.stream) { assert.equal(result.status, expected); return result; }
    const text = await deadline(result.text(), 'JSON response');
    if (state.failure) throw state.failure;
    assert.equal(result.status, expected, text);
    return { value: JSON.parse(text), headers: result.headers };
  }
  async function read(id) { return request('GET', `/v1/responses/${id}`, undefined, 200, principal.apiKey); }
  const client = new OpenAI({ apiKey: principal.apiKey, baseURL: 'https://agents.test/v1', maxRetries: 0,
    timeout: 12000, fetch: (input, init) => mf.dispatchFetch(input, init) });
  return { ...context, principal, state, agent, client, invoke, read };
}

async function consume(response, onEvent = () => {}) {
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  const events = [];
  let buffer = '';
  while (true) {
    const { done, value } = await reader.read();
    if (done) break;
    buffer += decoder.decode(value, { stream: true }).replaceAll('\r\n', '\n');
    let end;
    while ((end = buffer.indexOf('\n\n')) >= 0) {
      const frame = buffer.slice(0, end);
      buffer = buffer.slice(end + 2);
      const data = frame.split('\n').filter(line => line.startsWith('data:')).map(line => line.slice(5).trimStart()).join('\n');
      if (!data || data === '[DONE]') continue;
      const event = JSON.parse(data);
      events.push(event);
      onEvent(event);
    }
  }
  buffer += decoder.decode();
  assert.equal(buffer.trim(), '', 'SSE must not end with a truncated event');
  return events;
}

function assertCanonical(events) {
  assert.equal(events[0].type, 'response.created');
  const id = events[0].response.id;
  let sequence = -1;
  const added = new Map();
  for (const event of events) {
    assert.ok(Number.isInteger(event.sequence_number) && event.sequence_number > sequence);
    sequence = event.sequence_number;
    if (event.response) assert.equal(event.response.id, id);
    if (event.type === 'response.output_item.added') {
      assert.ok(event.item.id.startsWith(`item_${id}_0_`));
      added.set(event.output_index, event.item.id);
    }
    if (event.item_id) assert.equal(event.item_id, added.get(event.output_index));
    if (event.type === 'response.output_item.done') assert.equal(event.item.id, added.get(event.output_index));
  }
  const wire = JSON.stringify(events);
  assert.ok(!wire.includes(secret));
  assert.ok(!wire.includes(ciphertext));
  assert.ok(!wire.includes('upstream-reasoning') && !wire.includes('upstream-message'));
  return id;
}

async function assertKnownModelReceipts(scenario, responseIds) {
  const rows = await scenario.db.prepare(`
    SELECT r.idempotency_key,r.state,CAST(l.quantity AS TEXT) AS quantity,l.unit,
      CAST(l.amount_microusd AS TEXT) AS amount,l.price_version
    FROM usage_ledger l JOIN usage_reservations r ON r.id=l.reservation_id
    WHERE r.account_id=? AND l.resource='model' ORDER BY r.idempotency_key
  `).bind(scenario.principal.owner).all();
  const expected = responseIds.map(id => ({ idempotency_key: `completion:${id}:round:0`, state: 'settled',
    quantity: String(usage.total_tokens), unit: 'tokens', amount: '0', price_version: 'byok' }))
    .sort((left, right) => left.idempotency_key.localeCompare(right.idempotency_key));
  assert.deepEqual(rows.results, expected, 'Every known native round, including failure, settles exactly once');
}

export async function runNativeRichStreaming(context) {
  const gate = latch();
  const observed = latch();
  const reasoning = { id: 'upstream-reasoning', type: 'reasoning', summary: [{ type: 'summary_text', text: summary }],
    content: [{ type: 'reasoning_text', text: secret }], encrypted_content: ciphertext, hidden_vendor_field: secret };
  const message = { id: 'upstream-message', type: 'message', role: 'assistant', status: 'completed', phase: 'final_answer',
    content: [{ type: 'output_text', text: answer, annotations: [annotation], logprobs },
      { type: 'refusal', refusal: 'A separate unsafe subrequest is refused.' }] };
  const continued = { id: 'upstream-continuation', type: 'message', role: 'assistant', status: 'completed',
    content: [{ type: 'output_text', text: 'The private native context was retained.', annotations: [], logprobs: [] }] };
  const scenario = await setup(context, 'rich-stream', [{ output: [reasoning, message], gate },
    { output: [continued], check(body) {
      const carried = body.input.filter(item => item.type === 'reasoning');
      assert.equal(carried.length, 1, 'Private native reasoning is neither dropped nor duplicated on continuation');
      assert.equal(carried[0].encrypted_content, ciphertext);
      assert.equal(carried[0].content[0].text, secret);
    } }]);
  let collecting;
  try {
    const stream = await scenario.client.responses.create({ model: scenario.agent.id,
      input: 'Expose only the short summary and final answer.', stream: true, include: ['message.output_text.logprobs'] });
    collecting = (async () => {
      const events = [];
      for await (const event of stream) {
        events.push(event);
        if (event.type === 'response.output_text.delta') observed.resolve(event);
      }
      return events;
    })();
    const first = await deadline(observed.promise, 'Native first delta before terminal');
    assert.ok(first.delta.length > 0);
    assert.equal(scenario.state.terminalSent, false, 'First native delta must be observable while terminal is still latched');
    gate.resolve();
    const events = await deadline(collecting, 'Rich native terminal');
    const id = assertCanonical(events);
    assert.equal(events.at(-1).type, 'response.completed');
    const completed = events.at(-1).response;
    assert.deepEqual(completed.output.map(item => item.type), ['reasoning', 'message']);
    assert.deepEqual(completed.output[0].summary, reasoning.summary);
    assert.equal(completed.output[1].phase, 'final_answer');
    assert.deepEqual(completed.output[1].content[0].annotations, [annotation]);
    assert.deepEqual(completed.output[1].content[0].logprobs, logprobs);
    assert.equal(completed.output[1].content[1].refusal, message.content[1].refusal);
    assert.equal(events.filter(event => event.type === 'response.output_text.delta').map(event => event.delta).join(''), answer);
    assert.equal(events.filter(event => event.type === 'response.reasoning_summary_text.delta').map(event => event.delta).join(''), summary);
    assert.ok(events.findIndex(event => event.type === 'response.reasoning_summary_text.delta')
      < events.findIndex(event => event.type === 'response.output_text.delta'));
    assert.deepEqual(await scenario.request('GET', `/v1/responses/${id}?include%5B%5D=message.output_text.logprobs`,
      undefined, 200, scenario.principal.apiKey), completed);
    assert.equal(scenario.state.calls, 1);
    const reported = completed.usage ?? completed.agents_usage;
    assert.equal(reported.input_tokens, usage.input_tokens);
    assert.equal(reported.output_tokens, usage.output_tokens);
    assert.equal(reported.total_tokens, usage.total_tokens);
    assert.ok(completed.usage, 'Every standard detail is explicitly reported by this fixture');
    assert.equal(completed.usage.input_tokens_details.cache_write_tokens, 0);
    assert.equal(completed.usage.output_tokens_details.reasoning_tokens, 5);
    const continuation = await scenario.invoke({ previous_response_id: id, input: 'Continue with the same native context.' });
    assert.equal(continuation.value.status, 'completed');
    assert.ok(!JSON.stringify(continuation.value).includes(secret));
    assert.ok(!JSON.stringify(continuation.value).includes(ciphertext));
    assert.deepEqual(await scenario.request('GET', `/v1/responses/${id}?include%5B%5D=message.output_text.logprobs`,
      undefined, 200, scenario.principal.apiKey), completed);
    assert.equal(scenario.state.calls, 2);
    await assertKnownModelReceipts(scenario, [id, continuation.value.id]);
    return { provider_calls: scenario.state.calls, first_delta_before_terminal: true, unicode_chunking: true,
      private_native_context_retained: true };
  } finally {
    gate.resolve();
    await collecting?.catch(() => {});
  }
}

export async function runNativeTypedToolFork(context) {
  const tools = [{ type: 'namespace', name: 'project', description: 'Client-owned project operations', tools: [
    { type: 'custom', name: 'patch', format: { type: 'text' } },
    { type: 'function', name: 'lookup', strict: true, parameters: { type: 'object', properties: { value: { type: 'string' } },
      required: ['value'], additionalProperties: false } },
  ] }];
  const calls = [
    { type: 'custom_tool_call', id: 'upstream-custom', call_id: 'custom-call', namespace: 'project', name: 'patch', input: 'PATCH 雪🙂', status: 'completed' },
    { type: 'function_call', id: 'upstream-function', call_id: 'function-call', namespace: 'project', name: 'lookup', arguments: '{"value":"雪"}', status: 'completed' },
  ];
  const replies = suffix => [
    { type: 'custom_tool_call_output', call_id: 'custom-call', output: `patch-${suffix}` },
    { type: 'function_call_output', call_id: 'function-call', output: `lookup-${suffix}` },
  ];
  const answerItem = suffix => ({ type: 'message', id: `upstream-${suffix}`, role: 'assistant', status: 'completed',
    content: [{ type: 'output_text', text: `typed-${suffix}`, annotations: [], logprobs: [] }] });
  const check = suffix => body => {
    for (const returned of replies(suffix)) assert.ok(body.input.some(item => item.type === returned.type
      && item.call_id === returned.call_id && item.output === returned.output));
    assert.equal(body.input.filter(item => item.call_id === 'custom-call' && item.type === 'custom_tool_call').length, 1);
  };
  const scenario = await setup(context, 'typed-fork', [{ output: calls },
    { output: [answerItem('left')], check: check('left') }, { output: [answerItem('right')], check: check('right') }]);
  const parent = await scenario.invoke({ input: 'Use the two client tools.', tools });
  assert.deepEqual(parent.value.output.map(item => [item.type, item.namespace, item.name]),
    calls.map(item => [item.type, item.namespace, item.name]));
  const wrong = await scenario.invoke({ previous_response_id: parent.value.id, tools,
    input: [{ type: 'function_call_output', call_id: 'custom-call', output: 'wrong' }, replies('left')[1]] }, 400);
  assert.equal(wrong.value.error.code, 'tool_output_type_mismatch');
  assert.equal(scenario.state.calls, 1, 'Invalid typed return must be rejected before provider admission');
  const left = await scenario.invoke({ previous_response_id: parent.value.id, tools, input: replies('left') }, 200, 'native-typed-left');
  const rightBody = { previous_response_id: parent.value.id, tools, input: replies('right') };
  const right = await scenario.invoke(rightBody, 200, 'native-typed-right');
  assert.notEqual(left.headers.get('x-agents-session'), right.headers.get('x-agents-session'));
  assert.equal(right.value.previous_response_id, parent.value.id);
  assert.deepEqual(await scenario.read(parent.value.id), parent.value);
  const retry = await scenario.invoke(rightBody, 200, 'native-typed-right');
  assert.deepEqual(retry.value, right.value);
  assert.equal(scenario.state.calls, 3, 'Typed historical fork retry cannot replay inference');
  assert.ok(!JSON.stringify(scenario.state.requests[2]).includes('typed-left'), 'Historical fork excludes its sibling answer');
  await assertKnownModelReceipts(scenario, [parent.value.id, left.value.id, right.value.id]);
  return { provider_calls: scenario.state.calls, typed_return_rejected_before_dispatch: true, historical_fork_isolated: true };
}

export async function runNativeKnownFailure(context) {
  const scenario = await setup(context, 'known-failure', [{ status: 'failed', output: [{ type: 'message', id: 'upstream-message',
    role: 'assistant', status: 'incomplete', content: [{ type: 'output_text', text: 'Partial disclosed answer.', annotations: [], logprobs: [] }] }] }]);
  const response = await scenario.invoke({ input: 'Synthetic failing turn.', stream: true });
  const events = await deadline(consume(response), 'Known native failure');
  const id = assertCanonical(events);
  assert.equal(events.at(-1).type, 'response.failed');
  const failed = events.at(-1).response;
  assert.equal(failed.status, 'failed');
  assert.equal((failed.usage ?? failed.agents_usage).total_tokens, usage.total_tokens);
  assert.equal(failed.usage.input_tokens_details.cache_write_tokens, 0);
  assert.deepEqual(await scenario.read(id), failed);
  assert.equal(scenario.state.calls, 1);
  await assertKnownModelReceipts(scenario, [id]);
  return { provider_calls: 1, failed_terminal_preserves_known_usage: true };
}
