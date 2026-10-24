import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import OpenAI from 'openai';
import { createBrowserPrincipal } from './browser-principal.mjs';
import { runResponsesSdkJourney } from './responses-sdk.mjs';

const delay = milliseconds => new Promise(resolve => setTimeout(resolve, milliseconds));
const frame = value => `data: ${JSON.stringify(value)}\n\n`;

export function createResponsesProviderFixture() {
  const routes = new Map();
  return {
    register(name, actions) {
      const state = {
        baseUrl: `https://responses-${name}.example.com/v1`, model: `responses-${name}`,
        key: `synthetic-responses-${name}`, actions, calls: 0, requests: [], failure: null,
      };
      routes.set(state.baseUrl + '/chat/completions', state);
      return state;
    },
    async handle(request) {
      const state = routes.get(request.url);
      if (!state) return undefined;
      try {
        assert.equal(request.headers.get('authorization'), `Bearer ${state.key}`);
        const input = await request.json();
        assert.equal(input.model, state.model);
        const action = state.actions[state.calls++];
        assert.ok(action, 'A completed or rejected operation must not dispatch extra inference');
        state.requests.push(input);
        action.check?.(input);
        if (action.wait) await action.wait;
        if (action.error) return Response.json({ error: { message: 'Synthetic provider failure' } }, { status: action.error });
        const usage = { prompt_tokens: 150, completion_tokens: 10, total_tokens: 160,
          prompt_tokens_details: { cached_tokens: 30 }, completion_tokens_details: { reasoning_tokens: 0 } };
        if (action.reasoningUnknown) delete usage.completion_tokens_details;
        if (action.partialWait) {
          const initial = frame({ id: `provider-${state.calls}`, model: state.model,
            choices: [{ index: 0, delta: { role: 'assistant', content: 'Disclosed before isolate replacement.' }, finish_reason: null }] });
          const terminal = frame({ id: `provider-${state.calls}`, model: state.model,
            choices: [{ index: 0, delta: { content: ' Late result.' }, finish_reason: 'stop' }], usage }) + 'data: [DONE]\n\n';
          let cancelled = false;
          const body = new ReadableStream({
            start(controller) {
              controller.enqueue(new TextEncoder().encode(initial));
              action.partialWait.then(() => {
                if (!cancelled) {
                  controller.enqueue(new TextEncoder().encode(terminal));
                  controller.close();
                }
              });
            },
            cancel() { cancelled = true; },
          });
          return new Response(body, { headers: { 'Content-Type': 'text/event-stream' } });
        }
        if (input.stream === false) {
          assert.equal(action.kind, 'summary', 'Only compaction should use a non-streamed upstream request');
          return Response.json({ id: `provider-${state.calls}`, model: state.model,
            choices: [{ index: 0, message: { role: 'assistant', content: action.text }, finish_reason: 'stop' }], usage });
        }
        assert.equal(input.stream, true);
        const delta = action.calls
          ? { role: 'assistant', tool_calls: action.calls.map((call, index) => ({ index, id: call.id,
            type: 'function', function: { name: call.name, arguments: JSON.stringify(call.arguments ?? {}) } })) }
          : { role: 'assistant', content: action.text ?? '' };
        return new Response(frame({ id: `provider-${state.calls}`, model: state.model,
          choices: [{ index: 0, delta, finish_reason: action.calls ? 'tool_calls' : action.finish ?? 'stop' }], usage })
          + 'data: [DONE]\n\n', { headers: { 'Content-Type': 'text/event-stream' } });
      } catch (error) {
        state.failure = error;
        throw error;
      }
    },
  };
}

async function bounded(promise, milliseconds = 8000) {
  let timer;
  try {
    return await Promise.race([promise, new Promise((_, reject) => {
      timer = setTimeout(() => reject(new Error('Responses journey exceeded its observation deadline')), milliseconds);
    })]);
  } finally { clearTimeout(timer); }
}

async function waitUntil(predicate, milliseconds = 8000) {
  const deadline = Date.now() + milliseconds;
  while (true) {
    const remaining = deadline - Date.now();
    assert.ok(remaining > 0, 'Responses state did not reach the expected boundary');
    if (await bounded(Promise.resolve(predicate()), remaining)) return;
    await delay(10);
  }
}

async function fixture({ mf, db, encryptionKey, request, providers }, name, actions, options = {}) {
  const { context_window = 8192, ...agentOptions } = options;
  const principal = await createBrowserPrincipal({ mf, db, encryptionKey, owner: `responses-${name}-owner` });
  const state = providers.register(name, actions);
  const credential = await principal.browser('POST', '/v1/provider-keys', {
    protocol: 'openai', base_url: state.baseUrl, api_key: state.key,
  }, 201);
  const agent = await request('POST', '/v1/agents', {
    name: `Responses ${name}`, instructions: 'Fixed agent prefix must remain in every model context.',
    model: { provider: 'byok', credential_id: credential.id, model: state.model,
      context_window, max_output_tokens: 512 }, tools: ['time'], workspace: false,
    compaction_percent: 80, ...agentOptions,
  }, 201, principal.apiKey);
  const observations = { responses: [], events: [] };
  function observe(value) {
    if (value?.object === 'response') observations.responses.push(value);
    return value;
  }
  function checkSchema() {
    assert.ok(process.env.MAGENTS_SCHEMA_PYTHON, 'Set MAGENTS_SCHEMA_PYTHON to the pinned SDK verification interpreter');
    const script = fileURLToPath(new URL('../e2e/validate_responses_wire.py', import.meta.url));
    const checked = spawnSync(process.env.MAGENTS_SCHEMA_PYTHON, [script], {
      input: JSON.stringify(observations), encoding: 'utf8', timeout: 15000, maxBuffer: 1024 * 1024,
    });
    assert.equal(checked.status, 0, `Actual ${name} wire objects failed strict SDK schema validation: ${checked.stdout || checked.error?.name || checked.signal}`);
    const report = JSON.parse(checked.stdout);
    assert.equal(report.valid, true);
  }
  async function invoke(body, expected = 200, headers = {}) {
    const response = await bounded(mf.dispatchFetch('https://agents.test/v1/responses', {
      method: 'POST', headers: { Authorization: `Bearer ${principal.apiKey}`, 'Content-Type': 'application/json', ...headers },
      body: JSON.stringify({ model: agent.id, ...body }),
    }));
    const text = await bounded(response.text());
    if (state.failure) throw state.failure;
    assert.equal(response.status, expected, `POST /v1/responses: ${text}`);
    return { value: observe(JSON.parse(text)), headers: response.headers };
  }
  async function read(id, suffix = '', expected = 200) {
    return observe(await request('GET', `/v1/responses/${id}${suffix}`, undefined, expected, principal.apiKey));
  }
  async function finish() {
    const sessions = await bounded(request('GET', `/v1/agents/${agent.id}/sessions`, undefined, 200, principal.apiKey));
    for (const session of sessions.data) {
      await waitUntil(async () => {
        const status = await request('GET', `/v1/sessions/${session.id}`, undefined, 200, principal.apiKey);
        return !status.run_id && ['idle', 'closed'].includes(status.status);
      });
      await bounded(request('DELETE', `/v1/sessions/${session.id}`, undefined, 200, principal.apiKey));
    }
    const held = await db.prepare("SELECT COUNT(*) AS count FROM usage_reservations WHERE account_id=?1 AND state='reserved'")
      .bind(principal.owner).first('count');
    assert.equal(held, 0, 'Terminal/archive cleanup must not leave a run or storage funding hold');
    await request('DELETE', `/v1/agents/${agent.id}`, undefined, 200, principal.apiKey);
    await principal.browser('DELETE', `/v1/provider-keys/${credential.id}`);
    if (state.failure) throw state.failure;
    checkSchema();
  }
  return { state, principal, agent, invoke, read, finish, observations, checkSchema };
}

export async function runResponsesNodeSdkJourney(context) {
  const scenario = await fixture(context, 'node-sdk', [
    { text: 'Node SDK first.' },
    { text: 'Node SDK streamed.' },
    { calls: [{ id: 'node-local-call', name: 'local_echo', arguments: { value: 'sdk-local-value' } }],
      check(input) { assert.ok(input.tools.some(tool => tool.function.name === 'local_echo')); } },
    { text: 'Node SDK local tool complete.', check(input) {
      const result = input.messages.find(message => message.role === 'tool' && message.tool_call_id === 'node-local-call');
      assert.equal(result?.content, 'sdk-local-result');
    } },
  ]);
  try {
    const sdk = await runResponsesSdkJourney({ apiKey: scenario.principal.apiKey, baseURL: 'https://agents.test/v1',
      agentId: scenario.agent.id, fetch: (input, init) => context.mf.dispatchFetch(input, init) });
    scenario.observations.events.push(...sdk.events);
    const snapshots = await Promise.all(sdk.responseIds.map(id => scenario.read(id)));
    await bounded(context.mf.unsafeEvictDurableObject('', 'Session', { name: `${scenario.principal.owner}:${sdk.sessionId}` }));
    assert.deepEqual(await Promise.all(sdk.responseIds.map(id => scenario.read(id))), snapshots,
      'Cold reconstruction must retain every immutable response, not only the session head');
    const replay = await context.mf.dispatchFetch(`https://agents.test/v1/responses/${sdk.responseIds[1]}?stream=true`, {
      headers: { Authorization: `Bearer ${scenario.principal.apiKey}` },
    });
    assert.equal(replay.status, 200);
    const events = (await replay.text()).split('\n\n').map(block => block.split('\n').find(line => line.startsWith('data: ')))
      .filter(Boolean).map(line => JSON.parse(line.slice(6)));
    assert.deepEqual(events, sdk.events, 'Cold replay must preserve all previously disclosed sequence numbers and payloads');
    scenario.observations.events.push(...events);
    const beforeDelete = await context.request('GET', `/v1/sessions/${sdk.sessionId}`, undefined, 200, scenario.principal.apiKey);
    const deleted = await context.request('DELETE', `/v1/responses/${sdk.responseIds[0]}`, undefined, 200, scenario.principal.apiKey);
    assert.equal(deleted.deleted, true);
    await scenario.read(sdk.responseIds[0], '', 404);
    assert.deepEqual(await scenario.read(sdk.responseIds.at(-1)), snapshots.at(-1));
    assert.deepEqual((await context.request('GET', `/v1/sessions/${sdk.sessionId}`, undefined, 200, scenario.principal.apiKey)).messages,
      beforeDelete.messages, 'Deleting one response representation cannot erase source history');
    assert.equal(scenario.state.calls, 4, 'SDK reads, replay and stale-parent rejection cannot dispatch inference');
  } finally { await scenario.finish(); }
}

export async function runResponsesLocalToolsJourney(context) {
  const calls = [
    { id: 'server-time', name: 'time' },
    { id: 'local-one', name: 'read_local_a', arguments: { path: 'one.txt' } },
    { id: 'local-two', name: 'read_local_b', arguments: { path: 'two.txt' } },
  ];
  const scenario = await fixture(context, 'mixed-local', [
    { calls },
    { text: 'Both local outputs were consumed exactly once.', check(input) {
      assert.equal(input.messages.filter(message => message.role === 'tool' && message.tool_call_id === 'server-time').length, 1);
      assert.equal(input.messages.find(message => message.tool_call_id === 'local-one')?.content, 'first local value');
      assert.equal(input.messages.find(message => message.tool_call_id === 'local-two')?.content, 'second local value');
    } },
  ]);
  const tools = ['read_local_a', 'read_local_b'].map(name => ({ type: 'function', name,
    parameters: { type: 'object', properties: { path: { type: 'string' } }, required: ['path'], additionalProperties: false }, strict: true }));
  try {
    const { value: handoff, headers } = await scenario.invoke({ input: 'Use the server clock and both caller-local functions.', tools });
    const sessionId = headers.get('x-agents-session');
    assert.ok(sessionId);
    assert.equal(handoff.status, 'completed');
    const external = handoff.output.filter(item => item.type === 'function_call' && item.name.startsWith('read_local_'));
    assert.deepEqual(external.map(item => item.call_id), ['local-one', 'local-two']);
    const before = await context.request('GET', `/v1/sessions/${sessionId}`, undefined, 200, scenario.principal.apiKey);
    assert.equal(before.status, 'idle');
    assert.equal(before.run_id, null);
    assert.equal(before.messages.filter(message => message.role === 'tool' && message.tool_call_id === 'server-time').length, 1);
    await bounded(context.mf.unsafeEvictDurableObject('', 'Session', { name: `${scenario.principal.owner}:${sessionId}` }));
    assert.deepEqual(await scenario.read(handoff.id), handoff, 'Pending caller-local tool state must survive a cold DO');
    const output = (id, value) => ({ type: 'function_call_output', call_id: id, output: value });
    const valid = [output('local-one', 'first local value'), output('local-two', 'second local value')];
    for (const invalid of [valid.slice(0, 1), [valid[0], valid[0]], [valid[0], output('unknown-call', 'intrusion')]]) {
      await scenario.invoke({ previous_response_id: handoff.id, input: invalid, tools }, 400);
      const unchanged = await context.request('GET', `/v1/sessions/${sessionId}`, undefined, 200, scenario.principal.apiKey);
      assert.deepEqual(unchanged.messages, before.messages);
      assert.equal(unchanged.input_revision, before.input_revision);
      assert.equal(scenario.state.calls, 1);
    }
    await context.request('DELETE', `/v1/sessions/${sessionId}`, undefined, 200, scenario.principal.apiKey);
    assert.equal((await context.request('GET', `/v1/sessions/${sessionId}`, undefined, 200, scenario.principal.apiKey)).status, 'closed');
    const { value: resumed } = await scenario.invoke({ previous_response_id: handoff.id, input: valid, tools });
    assert.equal(resumed.status, 'completed');
    assert.equal(resumed.previous_response_id, handoff.id);
    assert.equal(scenario.state.calls, 2);
    assert.deepEqual(await scenario.read(handoff.id), handoff);
  } finally { await scenario.finish(); }
}

export async function runResponsesCompactionJourney(context) {
  const custom = 'CUSTOM-RESPONSE-CONSTRAINT: Preserve this exact instruction after compact.';
  const fixed = 'Fixed agent prefix must remain in every model context.';
  function checkPrompt(input, customized) {
    const instructions = input.messages.filter(message => ['system', 'developer'].includes(message.role)).map(message => message.content).join('\n');
    assert.ok(instructions.includes(fixed), 'Compaction must not replace fixed instructions with a summary');
    assert.equal(instructions.includes(custom), customized, 'Custom instructions have response-scoped exact lifetime across compact');
  }
  const scenario = await fixture(context, 'compact', [
    { text: 'Initial answer.', check: input => checkPrompt(input, true) },
    { kind: 'summary', text: 'Earlier source context preserved.' },
    { text: 'After the first compact.', check: input => checkPrompt(input, true) },
    { kind: 'summary', text: 'Earlier source context preserved again.' },
    { text: 'After the second compact without request override.', check: input => checkPrompt(input, false) },
  ], { compaction_percent: 1 });
  try {
    const first = await scenario.invoke({ input: 'Keep the goal.', instructions: custom });
    const sessionId = first.headers.get('x-agents-session');
    const before = await context.request('GET', `/v1/sessions/${sessionId}`, undefined, 200, scenario.principal.apiKey);
    const second = await scenario.invoke({ previous_response_id: first.value.id, input: 'Continue after compact.', instructions: custom });
    const third = await scenario.invoke({ previous_response_id: second.value.id, input: 'Continue without request override.' });
    assert.equal(third.value.status, 'completed');
    assert.equal(third.value.instructions, null);
    const after = await context.request('GET', `/v1/sessions/${sessionId}`, undefined, 200, scenario.principal.apiKey);
    assert.ok(after.compact_count >= 2, 'The scenario must actually exercise repeated compaction');
    assert.deepEqual(after.pinned, before.pinned);
    assert.deepEqual(after.messages.slice(0, before.messages.length), before.messages);
    assert.deepEqual(await scenario.read(first.value.id), first.value);
    assert.equal(scenario.state.calls, 5);
  } finally { await scenario.finish(); }
}

export async function runResponsesBackgroundRaceJourney(context) {
  let release;
  const pending = new Promise(resolve => { release = resolve; });
  const scenario = await fixture(context, 'background', [
    { text: 'Initial linear head.' },
    { text: 'The only admitted child.', wait: pending },
  ]);
  try {
    const initial = await scenario.invoke({ input: 'Create a head.' });
    const children = await Promise.all([0, 1].map(index => context.mf.dispatchFetch('https://agents.test/v1/responses', {
      method: 'POST', headers: { Authorization: `Bearer ${scenario.principal.apiKey}`, 'Content-Type': 'application/json' },
      body: JSON.stringify({ model: scenario.agent.id, previous_response_id: initial.value.id,
        input: `Concurrent child ${index}`, background: true }),
    })));
    assert.deepEqual(children.map(response => response.status).sort(), [200, 409], 'Only one child may claim the same head');
    const accepted = await children.find(response => response.status === 200).json();
    await children.find(response => response.status === 409).arrayBuffer();
    assert.ok(['queued', 'in_progress'].includes(accepted.status));
    await waitUntil(() => scenario.state.calls === 2);
    for (let index = 0; index < 3; index++) {
      const polled = await scenario.read(accepted.id);
      assert.equal(polled.id, accepted.id);
      assert.ok(['queued', 'in_progress'].includes(polled.status));
    }
    assert.equal(scenario.state.calls, 2, 'Polling cannot create inference');
    release();
    await waitUntil(async () => (await scenario.read(accepted.id)).status === 'completed');
    assert.deepEqual(await scenario.read(initial.value.id), initial.value);
  } finally { release(); await scenario.finish(); }
}

export async function runResponsesStorageAndFailureJourney(context) {
  const scenario = await fixture(context, 'storage-failure', [
    { text: 'Ephemeral successful answer.' },
    { error: 503 },
    { text: 'Output reached the configured budget.', finish: 'length' },
  ]);
  try {
    const ephemeral = await scenario.invoke({ input: 'Do not retain a retrievable response.', store: false });
    assert.equal(ephemeral.value.status, 'completed');
    assert.equal(ephemeral.value.store, false);
    await scenario.read(ephemeral.value.id, '', 404);
    await scenario.invoke({ previous_response_id: ephemeral.value.id, input: 'No retained parent.' }, 404);
    await context.request('DELETE', `/v1/sessions/${ephemeral.headers.get('x-agents-session')}`, undefined, 200, scenario.principal.apiKey);
    const failed = await scenario.invoke({ input: 'Return an explicit provider failure.', background: true });
    await waitUntil(async () => (await scenario.read(failed.value.id)).status === 'failed');
    const failure = await scenario.read(failed.value.id);
    assert.equal(typeof failure.error?.code, 'string');
    assert.equal(failure.output.length, 0, 'An upstream error cannot fabricate a successful assistant answer');
    await context.request('DELETE', `/v1/sessions/${failed.headers.get('x-agents-session')}`, undefined, 200, scenario.principal.apiKey);
    const limited = await scenario.invoke({ input: 'Return a budget-limited response.' });
    assert.equal(limited.value.status, 'incomplete');
    assert.equal(limited.value.incomplete_details?.reason, 'max_output_tokens');
    assert.equal(scenario.state.calls, 3, 'Failures, retrieval and no-store rejection cannot replay execution');
  } finally { await scenario.finish(); }
}

export async function runResponsesCancellationJourney(context) {
  let release;
  const pending = new Promise(resolve => { release = resolve; });
  const scenario = await fixture(context, 'cancellation', [
    { text: 'A late provider result cannot revive a cancelled response.', wait: pending },
  ]);
  try {
    const { value: accepted, headers } = await scenario.invoke({ input: 'Run in the background until cancellation.', background: true });
    const sessionId = headers.get('x-agents-session');
    await waitUntil(() => scenario.state.calls === 1);
    const observer = await bounded(context.mf.dispatchFetch(`https://agents.test/v1/responses/${accepted.id}?stream=true`, {
      headers: { Authorization: `Bearer ${scenario.principal.apiKey}` },
    }));
    assert.equal(observer.status, 200);
    const wirePromise = observer.text();
    const sdkEvents = [];
    const client = new OpenAI({ apiKey: scenario.principal.apiKey, baseURL: 'https://agents.test/v1', maxRetries: 0,
      timeout: 8000, fetch: (input, init) => context.mf.dispatchFetch(input, init) });
    const sdkOutcome = client.responses.retrieve(accepted.id, { stream: true }).then(async stream => {
      try {
        for await (const event of stream) sdkEvents.push(event);
        return null;
      } catch (error) { return error; }
    }, error => error);
    const before = await context.request('GET', `/v1/sessions/${sessionId}`, undefined, 200, scenario.principal.apiKey);
    const cancel = await context.request('POST', `/v1/responses/${accepted.id}/cancel`, {}, 200, scenario.principal.apiKey);
    scenario.observations.responses.push(cancel);
    assert.equal(cancel.id, accepted.id);
    assert.ok(['queued', 'in_progress', 'cancelled'].includes(cancel.status));
    const repeated = await context.request('POST', `/v1/responses/${accepted.id}/cancel`, {}, 200, scenario.principal.apiKey);
    scenario.observations.responses.push(repeated);
    assert.equal(repeated.id, accepted.id);
    release();
    await waitUntil(async () => (await scenario.read(accepted.id)).status === 'cancelled');
    const final = await scenario.read(accepted.id);
    assert.equal(final.status, 'cancelled');
    assert.deepEqual(final.usage, { input_tokens: 150, input_tokens_details: { cached_tokens: 30 },
      output_tokens: 10, output_tokens_details: { reasoning_tokens: 0 }, total_tokens: 160 },
    'Cancellation after a known provider result must retain the actually reported usage');
    const events = (await bounded(wirePromise)).split('\n\n')
      .map(block => block.split('\n').find(line => line.startsWith('data: '))).filter(Boolean)
      .map(line => JSON.parse(line.slice(6)));
    assert.equal(events.at(-1).type, 'error');
    assert.equal(events.at(-1).code, 'response_cancelled');
    assert.ok(!events.some(event => ['response.completed', 'response.failed', 'response.cancelled'].includes(event.type)),
      'Cancellation must not fabricate a completed/failed response or invent an SDK event type');
    const sdkError = await bounded(sdkOutcome);
    assert.ok(sdkError instanceof Error || sdkEvents.at(-1)?.type === 'error', 'The real SDK must surface cancellation, not a false success');
    scenario.observations.events.push(...events);
    assert.equal(scenario.state.calls, 1, 'Cancellation and late completion cannot dispatch a replacement inference');
    const after = await context.request('GET', `/v1/sessions/${sessionId}`, undefined, 200, scenario.principal.apiKey);
    assert.equal(after.input_revision, before.input_revision, 'Cancellation is not new user input');
  } finally { release(); await scenario.finish(); }
}

export async function runResponsesIdempotencyRaceJourney(context) {
  const scenario = await fixture(context, 'idempotency', [
    { text: 'The only inference for this idempotency receipt.' },
    { text: 'The later head does not change the original receipt.' },
  ]);
  try {
    const body = { model: scenario.agent.id, input: 'Create exactly one response.', background: true };
    const dispatch = () => context.mf.dispatchFetch('https://agents.test/v1/responses', {
      method: 'POST', headers: { Authorization: `Bearer ${scenario.principal.apiKey}`,
        'Content-Type': 'application/json', 'Idempotency-Key': 'synthetic-duplicate-create' }, body: JSON.stringify(body),
    });
    const responses = await Promise.all([dispatch(), dispatch()]);
    const values = [];
    for (const response of responses) {
      assert.equal(response.status, 200);
      values.push(await response.json());
    }
    assert.equal(values[0].id, values[1].id);
    await waitUntil(async () => (await scenario.read(values[0].id)).status === 'completed');
    const first = await scenario.read(values[0].id);
    assert.equal(scenario.state.calls, 1);
    const sessions = await context.request('GET', `/v1/agents/${scenario.agent.id}/sessions`, undefined, 200, scenario.principal.apiKey);
    assert.equal(sessions.data.length, 1, 'Concurrent duplicate requests cannot allocate two sessions');
    await scenario.invoke({ previous_response_id: first.id, input: 'Advance the same chain.' });
    const replay = await dispatch();
    assert.equal(replay.status, 200);
    assert.deepEqual(await replay.json(), first, 'Idempotency retry returns the original response even after the chain advances');
    await scenario.invoke({ input: 'A different canonical request.' }, 409, { 'Idempotency-Key': 'synthetic-duplicate-create' });
    assert.equal(scenario.state.calls, 2);
  } finally { await scenario.finish(); }
}

export async function runSessionViewsJourney(context) {
  const scenario = await fixture(context, 'history-pages', [
    { text: 'Imported source context acknowledged.' }, { text: 'New append acknowledged.' },
  ], { context_window: 131072 });
  const input = Array.from({ length: 110 }, (_, index) => ({ role: 'user',
    content: `Source entry ${index}: ` + 'synthetic immutable source data; '.repeat(30) }));
  try {
    const created = await scenario.invoke({ input });
    const sessionId = created.headers.get('x-agents-session');
    const endpoint = `https://agents.test/v1/sessions/${sessionId}`;
    const headers = { Authorization: `Bearer ${scenario.principal.apiKey}` };
    const legacyResponse = await context.mf.dispatchFetch(endpoint, { headers });
    assert.equal(legacyResponse.status, 200);
    const legacyText = await legacyResponse.text();
    const legacy = JSON.parse(legacyText);
    assert.equal(legacy.messages.length, 111, 'Legacy status must retain the entire imported source journal');
    const smallResponse = await context.mf.dispatchFetch(endpoint + '/view', { headers });
    assert.equal(smallResponse.status, 200);
    const etag = smallResponse.headers.get('etag');
    assert.ok(etag);
    const smallText = await smallResponse.text();
    const small = JSON.parse(smallText);
    assert.ok(!('messages' in small) && !('pinned' in small), 'Polling metadata cannot retransfer the source transcript or fixed prefix');
    assert.ok(Buffer.byteLength(smallText) < Buffer.byteLength(legacyText) / 10,
      'The metadata projection should materially reduce wire bytes for a real large journal');
    const same = await context.mf.dispatchFetch(endpoint + '/view', {
      headers: { ...headers, 'If-None-Match': etag },
    });
    assert.equal(same.status, 304);
    assert.equal(await same.text(), '');
    const guest = await context.mf.dispatchFetch(endpoint + '/view', { headers: { 'If-None-Match': etag } });
    assert.equal(guest.status, 401, 'Authentication must precede a matching private cache validator');
    await guest.arrayBuffer();
    const newest = await context.request('GET', `/v1/sessions/${sessionId}/messages`, undefined, 200, scenario.principal.apiKey);
    assert.equal(newest.data.length, 50);
    assert.equal(newest.start, 61);
    assert.equal(newest.end, 111);
    const entries = [...newest.data];
    let page = newest;
    while (page.has_older) {
      page = await context.request('GET', `/v1/sessions/${sessionId}/messages?before=${page.before_cursor}&limit=37`, undefined, 200, scenario.principal.apiKey);
      entries.unshift(...page.data);
    }
    assert.deepEqual(entries.map(entry => entry.sequence), Array.from({ length: 111 }, (_, index) => index));
    assert.deepEqual(entries.map(entry => entry.message), legacy.messages, 'Joining real source pages must reproduce legacy history exactly once');
    const future = newest.after_cursor.replace(/_[0-9a-f]+$/, '_ffffffff');
    await context.request('GET', `/v1/sessions/${sessionId}/messages?after=${future}`, undefined, 400, scenario.principal.apiKey);
    const other = await context.request('POST', `/v1/agents/${scenario.agent.id}/sessions`, {}, 201, scenario.principal.apiKey);
    await context.request('GET', `/v1/sessions/${other.id}/messages?after=${newest.after_cursor}`, undefined, 400, scenario.principal.apiKey);
    await scenario.invoke({ previous_response_id: created.value.id, input: 'Append only one new user message.' });
    const delta = await context.request('GET', `/v1/sessions/${sessionId}/messages?after=${newest.after_cursor}`, undefined, 200, scenario.principal.apiKey);
    assert.equal(delta.data.length, 2);
    assert.deepEqual(delta.data.map(entry => entry.sequence), [111, 112]);
    assert.equal(delta.data[0].message.content, 'Append only one new user message.');
    const changed = await context.mf.dispatchFetch(endpoint + '/view', { headers: { ...headers, 'If-None-Match': etag } });
    assert.equal(changed.status, 200);
    assert.notEqual(changed.headers.get('etag'), etag);
    await changed.arrayBuffer();
    assert.equal(scenario.state.calls, 2, 'History paging and conditional polling cannot trigger model execution');
    context.diagnostic?.(JSON.stringify({ event: 'session.bandwidth_fixture', source_entries: 111,
      legacy_bytes: Buffer.byteLength(legacyText), metadata_bytes: Buffer.byteLength(smallText),
      unchanged_body_bytes: 0, delta_entries: delta.data.length, delta_bytes: Buffer.byteLength(JSON.stringify(delta)) }));
  } finally { await scenario.finish(); }
}

export async function runResponsesInputAndOptionsJourney(context) {
  const scenario = await fixture(context, 'input-options', [{
    text: '{"ok":true}',
    check(input) {
      const protectedContext = input.messages.filter(message => ['system', 'developer'].includes(message.role))
        .map(message => message.content).join('\n');
      assert.ok(protectedContext.includes('Fixed agent prefix must remain in every model context.'));
      assert.ok(protectedContext.includes('Imported system instruction.'));
      assert.ok(protectedContext.includes('Imported developer instruction.'));
      assert.ok(input.messages.some(message => message.role === 'assistant' && message.content === 'Imported assistant context.'));
      assert.equal(input.messages.find(message => message.tool_call_id === 'imported-call')?.content, 'Imported local result.');
      assert.equal(input.temperature, 0.2);
      assert.equal(input.top_p, 0.8);
      assert.equal(input.parallel_tool_calls, false);
      assert.equal(input.max_tokens, 200);
      assert.deepEqual(input.response_format, { type: 'json_object' });
    },
  }]);
  try {
    for (const invalid of [
      { temperature: 2.1 }, { top_p: -1 }, { max_output_tokens: 0 },
      { fabricated_parameter: true },
      { input: [{ type: 'function_call_output', call_id: 'orphan', output: 'fake output' }] },
      { tools: [{ type: 'function', name: 'time', parameters: { type: 'object' } }] },
    ]) {
      await scenario.invoke({ input: 'Reject invalid control before execution.', ...invalid }, 400);
      assert.equal(scenario.state.calls, 0);
    }
    const { value: response } = await scenario.invoke({
      input: [
        { role: 'system', content: 'Imported system instruction.' },
        { role: 'developer', content: [{ type: 'input_text', text: 'Imported developer instruction.' }] },
        { role: 'user', content: 'Earlier imported user context.' },
        { role: 'assistant', content: [{ type: 'output_text', text: 'Imported assistant context.' }] },
        { type: 'function_call', call_id: 'imported-call', name: 'imported_local', arguments: '{}' },
        { type: 'function_call_output', call_id: 'imported-call', output: 'Imported local result.' },
        { role: 'user', content: [{ type: 'input_text', text: 'Respond with a JSON object.' }] },
      ], temperature: 0.2, top_p: 0.8, parallel_tool_calls: false, max_output_tokens: 200,
      text: { format: { type: 'json_object' } },
      tools: [{ type: 'function', name: 'imported_local',
        parameters: { type: 'object', properties: {}, additionalProperties: false }, strict: true }],
    });
    assert.equal(response.status, 'completed');
    const text = response.output.filter(item => item.type === 'message')
      .flatMap(item => item.content).filter(part => part.type === 'output_text').map(part => part.text).join('');
    assert.deepEqual(JSON.parse(text), { ok: true });
    assert.equal(response.temperature, 0.2);
    assert.equal(response.top_p, 0.8);
    assert.equal(response.max_output_tokens, 200);
    assert.deepEqual(response.text?.format, { type: 'json_object' });
    assert.equal(scenario.state.calls, 1);
  } finally { await scenario.finish(); }
}

export async function runResponsesUnknownUsageJourney(context) {
  const scenario = await fixture(context, 'unknown-reasoning', [{ text: 'Known totals, unavailable reasoning breakdown.', reasoningUnknown: true }]);
  try {
    const { value: response } = await scenario.invoke({ input: 'Report the result without inventing token details.' });
    assert.equal(response.status, 'completed');
    assert.equal(response.usage, null, 'Incomplete breakdown cannot be serialized as an invalid standard SDK usage object');
    assert.deepEqual(response.agents_usage, {
      input_tokens: 150, input_tokens_details: { cached_tokens: 30 },
      output_tokens: 10, output_tokens_details: { reasoning_tokens: null }, total_tokens: 160,
    }, 'Unknown reasoning cannot discard known totals or fabricate a zero count');
    assert.deepEqual(await scenario.read(response.id), response);
    const modelRows = await context.db.prepare("SELECT CAST(l.amount_microusd AS TEXT) AS amount,l.price_version FROM usage_ledger l JOIN usage_reservations r ON r.id=l.reservation_id WHERE r.account_id=?1 AND l.resource='model'")
      .bind(scenario.principal.owner).all();
    assert.deepEqual(modelRows.results, [{ amount: '0', price_version: 'byok' }]);
    assert.equal(scenario.state.calls, 1);
  } finally { await scenario.finish(); }
}

export async function runResponsesMidstreamResetJourney(context) {
  let release;
  const partialWait = new Promise(resolve => { release = resolve; });
  const scenario = await fixture(context, 'midstream-reset', [{ partialWait }]);
  let reader;
  try {
    const raw = await bounded(context.mf.dispatchFetch('https://agents.test/v1/responses', {
      method: 'POST', headers: { Authorization: `Bearer ${scenario.principal.apiKey}`, 'Content-Type': 'application/json' },
      body: JSON.stringify({ model: scenario.agent.id, input: 'Produce a streamed answer.', stream: true }),
    }));
    assert.equal(raw.status, 200);
    const sessionId = raw.headers.get('x-agents-session');
    reader = raw.body.getReader();
    const decoder = new TextDecoder();
    let buffer = '';
    const disclosed = [];
    let delta;
    while (!delta) {
      const part = await bounded(reader.read());
      assert.equal(part.done, false, 'The controlled upstream must disclose text before its terminal result');
      buffer += decoder.decode(part.value, { stream: true });
      let boundary;
      while ((boundary = buffer.indexOf('\n\n')) >= 0) {
        const block = buffer.slice(0, boundary);
        buffer = buffer.slice(boundary + 2);
        const data = block.split('\n').find(line => line.startsWith('data: '));
        if (!data) continue;
        const event = JSON.parse(data.slice(6));
        disclosed.push(event);
        if (event.type === 'response.output_text.delta') delta = event;
      }
    }
    const responseId = disclosed.find(event => event.type === 'response.created').response.id;
    assert.ok(Number.isSafeInteger(delta.sequence_number));
    await bounded(context.mf.unsafeEvictDurableObject('', 'Session', { name: `${scenario.principal.owner}:${sessionId}` }));
    await waitUntil(async () => (await scenario.read(responseId)).status === 'failed');
    const final = await scenario.read(responseId);
    assert.equal(final.error?.code, 'server_error');
    assert.equal(final.agents_error_code, 'provider_outcome_unknown');
    const replay = await context.mf.dispatchFetch(`https://agents.test/v1/responses/${responseId}?stream=true&starting_after=${delta.sequence_number}`, {
      headers: { Authorization: `Bearer ${scenario.principal.apiKey}` },
    });
    assert.equal(replay.status, 200);
    const events = (await bounded(replay.text())).split('\n\n')
      .map(block => block.split('\n').find(line => line.startsWith('data: '))).filter(Boolean)
      .map(line => JSON.parse(line.slice(6)));
    assert.ok(events.length > 0);
    assert.ok(events.every(event => event.sequence_number > delta.sequence_number),
      'Recovery must not reuse a sequence number already disclosed before reset');
    assert.equal(events.at(-1).type, 'response.failed');
    assert.equal(events.at(-1).response.id, responseId);
    assert.equal(scenario.state.calls, 1, 'An uncertain external operation cannot be repeated after isolate replacement');
    const outstanding = await context.db.prepare("SELECT idempotency_key,state FROM usage_reservations WHERE account_id=?1 AND state='reserved'")
      .bind(scenario.principal.owner).all();
    assert.ok(outstanding.results.some(row => row.idempotency_key.startsWith('completion:')),
      'Unknown provider usage must remain an explicit unresolved receipt, not fabricated zero settlement');
    const modelRows = await context.db.prepare("SELECT COUNT(*) AS count FROM usage_ledger l JOIN usage_reservations r ON r.id=l.reservation_id WHERE r.account_id=?1 AND l.resource='model'")
      .bind(scenario.principal.owner).first('count');
    assert.equal(modelRows, 0);
    scenario.observations.events.push(...disclosed, ...events);
    scenario.checkSchema();
    context.diagnostic?.(JSON.stringify({ event: 'response.midstream_reset_fixture', disclosed_sequence: delta.sequence_number,
      terminal_sequence: events.at(-1).sequence_number, provider_calls: scenario.state.calls,
      unresolved_completion_receipts: outstanding.results.filter(row => row.idempotency_key.startsWith('completion:')).length }));
    // This isolated Miniflare account intentionally retains the unknown receipt.
    // Releasing it would claim nonexistent provider evidence. Suite disposal,
    // unlike a real account operation, removes the complete synthetic database.
  } finally {
    release();
    if (reader) await bounded(reader.cancel()).catch(() => {});
  }
}
