import assert from 'node:assert/strict';
import { isDeepStrictEqual } from 'node:util';
import { createNativeSemanticsScenario } from './native-semantics-journey.mjs';

const capacity = 4096;
const outputBudget = 64;
const oldMarker = 'OLD-WINDOW-CONTENT';
const newMarker = 'LATEST-WINDOW-CONTENT';
const policyMarker = 'DEVELOPER-WINDOW-KEEP';
const fixedMarker = 'NATIVE-IMMUTABLE-PREFIX';
const oldInput = `${oldMarker}:` + 'a'.repeat(2200);
const latestInput = `${newMarker}:` + 'b'.repeat(2200);

export function windowFixtureCount(body) {
  // Synthetic endpoint contract, not a claim about a production tokenizer.
  return Buffer.byteLength(JSON.stringify({ input: body.input, tools: body.tools ?? [] }), 'utf8');
}

export function windowFixtureUsage(body) {
  const input = windowFixtureCount(body);
  return { input_tokens: input, output_tokens: 1, total_tokens: input + 1,
    input_tokens_details: { cached_tokens: 0, cache_write_tokens: 0 },
    output_tokens_details: { reasoning_tokens: 0 } };
}

function answer(index) {
  return { type: 'message', id: `upstream-window-${index}`, role: 'assistant', status: 'completed',
    content: [{ type: 'output_text', text: `window-answer-${index}`, annotations: [], logprobs: [] }] };
}

function mentions(body, marker) {
  return JSON.stringify(body.input).split(marker).length - 1;
}

function assertCountPreparedMatches(measurements, generation, tokens) {
  const fields = ['model', 'input', 'tools', 'tool_choice', 'parallel_tool_calls', 'text',
    'reasoning', 'truncation', 'personality'];
  const projection = Object.fromEntries(fields.filter(field => Object.hasOwn(generation, field))
    .map(field => [field, generation[field]]));
  assert.ok(measurements.some(body => isDeepStrictEqual(body, projection) && windowFixtureCount(body) === tokens),
    'The public count measures the complete same native input, tools and countable controls as generation');
}

function expectInput({ old, latest, additional }) {
  return body => {
    assert.equal(mentions(body, fixedMarker), 1, 'The protected pinned prefix appears once');
    assert.equal(mentions(body, policyMarker), 1, 'Historical developer guidance cannot be truncated');
    assert.equal(mentions(body, oldMarker), old ? 1 : 0);
    assert.equal(mentions(body, newMarker), latest ? 1 : 0);
    if (additional) assert.equal(mentions(body, additional), 1);
    assert.ok(windowFixtureCount(body) <= capacity - outputBudget,
      'The actual provider request, not only a planning copy, fits the selected input budget');
  };
}

async function allowFiveSyntheticSessions(context, owner) {
  const now = Math.floor(Date.now() / 1000);
  const subscriptions = [{ id: `synthetic-window-go-${owner}`, product_id: 'agents', plan_id: 'agents-go',
    status: 'active', current_period_start: now - 10, current_period_end: now + 3600,
    entitlements: ['agents.access', 'agents.tier.go'] }];
  await context.db.prepare("INSERT INTO subscription_snapshots(account_id,product_id,subscription_json,fetched_at,valid_until) VALUES(?,'agents',?,?,?) ON CONFLICT(account_id,product_id) DO UPDATE SET subscription_json=excluded.subscription_json,fetched_at=excluded.fetched_at,valid_until=excluded.valid_until")
    .bind(owner, JSON.stringify(subscriptions), now, now + 300).run();
}

async function assertModelLedger(scenario, responses) {
  const rows = (await scenario.db.prepare(`SELECT r.idempotency_key,r.state,CAST(l.quantity AS TEXT) AS quantity,
    CAST(l.amount_microusd AS TEXT) AS amount,l.price_version FROM usage_ledger l
    JOIN usage_reservations r ON r.id=l.reservation_id WHERE r.account_id=? AND l.resource='model'
    ORDER BY r.idempotency_key`).bind(scenario.principal.owner).all()).results;
  assert.deepEqual(rows, responses.map((response, index) => ({ idempotency_key: `completion:${response.value.id}:round:0`, state: 'settled',
    quantity: String(windowFixtureUsage(scenario.state.requests[index]).total_tokens), amount: '0', price_version: 'byok' }))
    .sort((left, right) => left.idempotency_key.localeCompare(right.idempotency_key)));
}

async function outcomeWithoutInference(scenario, body, key) {
  const before = scenario.state.calls;
  const response = await scenario.mf.dispatchFetch('https://agents.test/v1/responses', {
    method: 'POST', headers: { Authorization: `Bearer ${scenario.principal.apiKey}`,
      'Content-Type': 'application/json', 'Idempotency-Key': key },
    body: JSON.stringify({ model: scenario.agent.id, ...body }),
  });
  const value = await response.json();
  assert.ok(response.status === 400 || response.status === 200,
    `Context rejection must be a request error or an accepted failed Response, got ${response.status}`);
  if (response.status === 200) assert.equal(value.status, 'failed');
  assert.equal(value.error?.code, 'context_length_exceeded');
  assert.equal(scenario.state.calls, before, 'An oversized rejected request cannot buy model work');
  return { status: response.status, response_id: value.object === 'response' ? value.id : null };
}

export async function runNativeWindowJourney(context) {
  const checks = [expectInput({ old: true, latest: false }), expectInput({ old: false, latest: true }),
    expectInput({ old: false, latest: true, additional: 'SAME-HEAD-SHORT-INPUT' }),
    expectInput({ old: true, latest: false, additional: 'ORIGINAL-HISTORICAL-FORK' }),
    expectInput({ old: false, latest: true, additional: 'TRUNCATED-HISTORICAL-FORK' })];
  const actions = checks.map((check, index) => ({ check, output: [answer(index)], usage: windowFixtureUsage }));
  const scenario = await createNativeSemanticsScenario(context, 'window', actions);
  scenario.state.count = windowFixtureCount;
  await allowFiveSyntheticSessions(context, scenario.principal.owner);
  scenario.agent = await context.request('PATCH', `/v1/agents/${scenario.agent.id}`, {
    model: { ...scenario.agent.model, context_window: capacity, max_output_tokens: outputBudget }, compaction_percent: 99,
  }, 200, scenario.principal.apiKey);
  const invoke = (body, key) => scenario.invoke({ ...body }, 200, key);
  const source = await invoke({ truncation: 'auto', input: [
    { role: 'developer', content: policyMarker }, { role: 'user', content: oldInput },
  ] }, 'native-window-original');
  assert.equal(source.value.status, 'completed');
  const original = await scenario.read(source.value.id);
  const originalItems = (await scenario.client.responses.inputItems.list(source.value.id)).data;
  const sessionId = source.headers.get('x-agents-session');
  const before = await context.request('GET', `/v1/sessions/${sessionId}`, undefined, 200, scenario.principal.apiKey);
  assert.ok(Number.isSafeInteger(before.last_input_at_ms));
  assert.ok(Number.isSafeInteger(before.input_revision));
  const continuedBody = { previous_response_id: source.value.id, truncation: 'auto', input: latestInput };
  const callsBeforeCount = scenario.state.calls;
  const countsBeforeCount = scenario.state.countRequests.length;
  const counted = await scenario.client.responses.inputTokens.count({ model: scenario.agent.id,
    previous_response_id: source.value.id, truncation: 'auto', input: latestInput });
  const publicMeasurements = scenario.state.countRequests.slice(countsBeforeCount);
  const afterCount = await context.request('GET', `/v1/sessions/${sessionId}`, undefined, 200, scenario.principal.apiKey);
  assert.equal(scenario.state.calls, callsBeforeCount, 'Input counting must not perform inference');
  assert.deepEqual(afterCount.messages, before.messages, 'Count-only truncation must not alter source or active state');
  assert.equal(afterCount.input_revision, before.input_revision);
  assert.equal(afterCount.last_input_at_ms, before.last_input_at_ms);
  const continued = await invoke(continuedBody, 'native-window-continued');
  assert.equal(continued.value.status, 'completed');
  assert.equal(continued.headers.get('x-agents-session'), sessionId);
  assert.equal(continued.value.truncation, 'auto', 'Public policy is not rewritten to the private upstream control');
  assert.equal(scenario.state.requests[1].truncation, 'disabled', 'The provider receives the already selected window');
  const reported = continued.value.usage ?? continued.value.agents_usage;
  assert.equal(reported.input_tokens, counted.input_tokens,
    'Public count and actual generation use the same fitted native context');
  assert.equal(reported.input_tokens, windowFixtureCount(scenario.state.requests[1]));
  assertCountPreparedMatches(publicMeasurements, scenario.state.requests[1], counted.input_tokens);
  assert.deepEqual(await scenario.read(source.value.id), original, 'Truncation cannot rewrite the old canonical Response');
  assert.deepEqual((await scenario.client.responses.inputItems.list(source.value.id)).data, originalItems);
  const after = await context.request('GET', `/v1/sessions/${sessionId}`, undefined, 200, scenario.principal.apiKey);
  assert.deepEqual(after.messages.slice(0, before.messages.length), before.messages,
    'The immutable source journal is retained even when old model context is dropped');
  assert.ok(JSON.stringify(after.messages).includes(oldMarker));
  const sameHead = await invoke({ previous_response_id: continued.value.id, input: 'SAME-HEAD-SHORT-INPUT' }, 'native-window-head');
  assert.equal(sameHead.value.status, 'completed');
  await context.mf.unsafeEvictDurableObject('', 'Session', { name: `${scenario.principal.owner}:${sessionId}` });
  const firstFork = await invoke({ previous_response_id: source.value.id, input: 'ORIGINAL-HISTORICAL-FORK' }, 'native-window-old-fork');
  assert.equal(firstFork.value.status, 'completed');
  const secondFork = await invoke({ previous_response_id: continued.value.id, input: 'TRUNCATED-HISTORICAL-FORK' }, 'native-window-cut-fork');
  assert.equal(secondFork.value.status, 'completed', 'A truncated fork must not recover the old full-source legacy estimate');
  assert.notEqual(firstFork.headers.get('x-agents-session'), secondFork.headers.get('x-agents-session'));
  assert.deepEqual(await scenario.read(source.value.id), original);
  const retry = await invoke({ previous_response_id: continued.value.id, input: 'TRUNCATED-HISTORICAL-FORK' }, 'native-window-cut-fork');
  assert.deepEqual(retry.value, secondFork.value);
  const oversized = 'UNFIT-LATEST-GROUP-' + 'x'.repeat(6000);
  const disabled = await outcomeWithoutInference(scenario, { input: oversized, truncation: 'disabled' }, 'native-window-disabled');
  const unfit = await outcomeWithoutInference(scenario, { previous_response_id: secondFork.value.id, input: oversized,
    truncation: 'auto' }, 'native-window-unfit');
  assert.equal(scenario.state.calls, actions.length, 'Only the five planned generations execute; no summary or retry replay');
  await assertModelLedger(scenario, [source, continued, sameHead, firstFork, secondFork]);
  return { provider_calls: actions.length, synthetic_counter_calls: scenario.state.countRequests.length,
    source_preserved: true, protected_prefix_and_developer_preserved: true, counter_matches_generation: true,
    default_continuation_and_historical_forks_match_native_view: true, disabled, unfit,
    external_network_calls: 0, counter_kind: 'deterministic-synthetic-provider-endpoint-not-production-tokenizer' };
}

export async function runNativeWindowToolsJourney(context) {
  const callId = 'window-client-call';
  const call = { type: 'function_call', id: 'upstream-window-call', call_id: callId,
    name: 'window_echo', arguments: '{}', status: 'completed' };
  const tools = [{ type: 'function', name: 'window_echo', strict: true,
    parameters: { type: 'object', properties: {}, required: [], additionalProperties: false } }];
  const closure = body => {
    expectInput({ old: false, latest: true })(body);
    assert.equal(body.input.filter(item => item.call_id === callId).length, 0,
      'Dropping the oldest closed group removes both its call and result');
  };
  const historical = body => {
    expectInput({ old: true, latest: false, additional: 'TOOL-HISTORICAL-FORK' })(body);
    const pair = body.input.filter(item => item.call_id === callId);
    assert.deepEqual(pair.map(item => item.type), ['function_call', 'function_call_output']);
    assert.equal(pair[1].output, 'historical-result');
  };
  const scenario = await createNativeSemanticsScenario(context, 'window-tools', [
    { output: [call], usage: windowFixtureUsage, check: expectInput({ old: true, latest: false }) },
    { output: [answer('tools-cut')], usage: windowFixtureUsage, check: closure },
    { output: [answer('tools-fork')], usage: windowFixtureUsage, check: historical },
  ]);
  scenario.state.count = windowFixtureCount;
  scenario.agent = await context.request('PATCH', `/v1/agents/${scenario.agent.id}`, {
    model: { ...scenario.agent.model, context_window: capacity, max_output_tokens: outputBudget }, compaction_percent: 99,
  }, 200, scenario.principal.apiKey);
  const invoke = (body, key) => scenario.invoke({ tools, ...body }, 200, key);
  const parent = await invoke({ input: [{ role: 'developer', content: policyMarker },
    { role: 'user', content: oldInput }], truncation: 'auto' }, 'window-tools-parent');
  assert.equal(parent.value.status, 'completed');
  assert.deepEqual(parent.value.output.map(item => item.type), ['function_call']);
  const snapshot = await scenario.read(parent.value.id);
  const output = value => ({ type: 'function_call_output', call_id: callId, output: value });
  const continuedBody = { previous_response_id: parent.value.id, truncation: 'auto',
    input: [output('current-result'), { role: 'user', content: latestInput }] };
  const countsBeforeCount = scenario.state.countRequests.length;
  const count = await scenario.client.responses.inputTokens.count({ model: scenario.agent.id, tools, ...continuedBody });
  const publicMeasurements = scenario.state.countRequests.slice(countsBeforeCount);
  const continued = await invoke(continuedBody, 'window-tools-continued');
  assert.equal(continued.value.status, 'completed');
  assert.equal((continued.value.usage ?? continued.value.agents_usage).input_tokens, count.input_tokens);
  assertCountPreparedMatches(publicMeasurements, scenario.state.requests[1], count.input_tokens);
  const fork = await invoke({ previous_response_id: parent.value.id,
    input: [output('historical-result'), { role: 'user', content: 'TOOL-HISTORICAL-FORK' }] }, 'window-tools-fork');
  assert.equal(fork.value.status, 'completed');
  assert.notEqual(fork.headers.get('x-agents-session'), continued.headers.get('x-agents-session'));
  assert.deepEqual(await scenario.read(parent.value.id), snapshot,
    'The original outstanding-call boundary remains immutable after a dropping continuation');
  const retry = await invoke({ previous_response_id: parent.value.id,
    input: [output('historical-result'), { role: 'user', content: 'TOOL-HISTORICAL-FORK' }] }, 'window-tools-fork');
  assert.deepEqual(retry.value, fork.value);
  assert.equal(scenario.state.calls, 3);
  await assertModelLedger(scenario, [parent, continued, fork]);
  return { provider_calls: 3, synthetic_counter_calls: scenario.state.countRequests.length,
    closed_group_dropped_atomically: true, historical_pending_group_restored_once: true,
    counter_matches_generation: true, external_network_calls: 0 };
}
