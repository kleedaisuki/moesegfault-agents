import assert from 'node:assert/strict';
import { isDeepStrictEqual } from 'node:util';
import { createNativeSemanticsScenario } from './native-semantics-journey.mjs';

const marker = '<agents_context_update>\n';
const endMarker = '\n</agents_context_update>';
const fixed = 'NATIVE-IMMUTABLE-PREFIX';
const policyA = 'CACHE-INSTRUCTIONS-A';
const policyB = 'CACHE-INSTRUCTIONS-B';
const fields = ['model', 'input', 'tools', 'tool_choice', 'parallel_tool_calls', 'text',
  'reasoning', 'truncation', 'personality'];
const projection = body => Object.fromEntries(fields.filter(key => Object.hasOwn(body, key))
  .map(key => [key, body[key]]));

function count(body) {
  // This endpoint measures fixture bytes, not production tokens or cache hits.
  return Buffer.byteLength(JSON.stringify(projection(body)));
}

function usage(body) {
  const input = count(body);
  return { input_tokens: input, output_tokens: 1, total_tokens: input + 1,
    input_tokens_details: { cached_tokens: 0, cache_write_tokens: 0 },
    output_tokens_details: { reasoning_tokens: 0 } };
}

function answer(index) {
  return { type: 'message', id: `upstream-cache-answer-${index}`, role: 'assistant', status: 'completed',
    content: [{ type: 'output_text', text: `cache-answer-${index}`, annotations: [], logprobs: [] }] };
}

function text(item) {
  if (typeof item.content === 'string') return item.content;
  return (item.content ?? []).map(part => part.text ?? '').join('');
}

function updates(body) {
  return body.input.flatMap((item, index) => {
    const content = text(item);
    if (!content.startsWith(marker)) return [];
    assert.equal(item.role, 'system', 'Trusted runtime updates retain system authority on DeepSeek Responses');
    assert.ok(content.endsWith(endMarker));
    const value = JSON.parse(content.slice(marker.length, -endMarker.length));
    assert.ok(Object.hasOwn(value, 'instructions'), 'An omitted instruction must be represented as an explicit reset');
    assert.match(value.tool_set_sha256, /^[a-f0-9]{64}$/);
    assert.equal(value.environment.workspace, false);
    assert.ok(Object.hasOwn(value.environment, 'runtime_profile'));
    return [{ index, value }];
  });
}

function assertStablePrefix(before, after) {
  assert.deepEqual(after.input.slice(0, before.input.length), before.input,
    'A new instruction snapshot must append after the complete previously submitted native input');
  assert.deepEqual(after.input[0], before.input[0], 'The pinned first system item is byte-stable');
  assert.equal(text(after.input[0]).includes(fixed), true);
}

function assertCurrent(body, instructions, expectedTransitions) {
  const snapshots = updates(body);
  assert.equal(snapshots.length, expectedTransitions, 'Unchanged context must not produce duplicate snapshots');
  assert.equal(snapshots.at(-1).value.instructions, instructions);
  assert.ok(!text(body.input[0]).includes(policyA) && !text(body.input[0]).includes(policyB),
    'Response-scoped instructions must not rewrite the cacheable pinned prefix');
  return snapshots;
}

async function snapshot(scenario, sessionId) {
  return scenario.request('GET', `/v1/sessions/${sessionId}`, undefined, 200, scenario.principal.apiKey);
}

async function countWithoutMutation(scenario, sessionId, request) {
  const before = await snapshot(scenario, sessionId);
  const calls = scenario.state.calls;
  const measurementStart = scenario.state.countRequests.length;
  const measured = await scenario.client.responses.inputTokens.count({ model: scenario.agent.id, ...request });
  const after = await snapshot(scenario, sessionId);
  assert.deepEqual(after.messages, before.messages, 'Count-only projection cannot append the context event to source history');
  assert.deepEqual(after.pinned, before.pinned, 'Count-only projection cannot change the pinned contract');
  assert.equal(after.input_revision, before.input_revision);
  assert.equal(after.last_input_at_ms, before.last_input_at_ms);
  assert.equal(after.run_id, before.run_id);
  assert.equal(scenario.state.calls, calls, 'Read-only count cannot dispatch inference');
  return { measured, requests: scenario.state.countRequests.slice(measurementStart) };
}

function assertMeasuredGeneration(measurement, body) {
  assert.ok(measurement.requests.some(request => isDeepStrictEqual(request, projection(body))),
    'Count and generation must use the same prepared native input, tool schemas and countable controls');
  assert.equal(measurement.measured.input_tokens, count(body));
}

async function knownModelRows(scenario, responseIds) {
  const db = await scenario.mf.getD1Database('DB');
  const rows = (await db.prepare(`SELECT r.idempotency_key,r.state,CAST(l.quantity AS TEXT) AS quantity,
    CAST(l.amount_microusd AS TEXT) AS amount,l.price_version FROM usage_ledger l
    JOIN usage_reservations r ON r.id=l.reservation_id WHERE r.account_id=? AND l.resource='model'
    ORDER BY r.idempotency_key`).bind(scenario.principal.owner).all()).results;
  assert.deepEqual(rows, responseIds.map((id, index) => ({ idempotency_key: `completion:${id}:round:0`, state: 'settled',
    quantity: String(usage(scenario.state.requests[index]).total_tokens), amount: '0', price_version: 'byok' }))
    .sort((left, right) => left.idempotency_key.localeCompare(right.idempotency_key)));
  return rows.length;
}

export async function runPromptCacheInstructionsJourney(context) {
  const actions = Array.from({ length: 5 }, (_, index) => ({ output: [answer(index)], usage }));
  const scenario = await createNativeSemanticsScenario(context, 'cache-instructions', actions);
  scenario.state.count = count;
  const create = (body, key) => scenario.invoke({ max_output_tokens: 64, ...body }, 200, key);
  const firstBody = { instructions: policyA, input: 'CACHE-FIRST-USER' };
  const first = await create(firstBody, 'cache-instructions-first');
  assert.equal(first.value.status, 'completed');
  const sessionId = first.headers.get('x-agents-session');
  const pinned = (await snapshot(scenario, sessionId)).pinned;
  assert.equal(pinned.prompt_layout, 'append_only_v1');
  assertCurrent(scenario.state.requests[0], policyA, 1);
  const original = await scenario.read(first.value.id);
  const originalInput = (await scenario.client.responses.inputItems.list(first.value.id)).data;
  assert.ok(!JSON.stringify(originalInput).includes(marker), 'Private runtime events are not fabricated caller input items');

  const sameBody = { previous_response_id: first.value.id, instructions: policyA, input: 'CACHE-SAME-POLICY-USER' };
  const same = await create(sameBody, 'cache-instructions-unchanged');
  assert.equal(same.value.status, 'completed');
  assert.equal(same.headers.get('x-agents-session'), sessionId);
  assertStablePrefix(scenario.state.requests[0], scenario.state.requests[1]);
  assertCurrent(scenario.state.requests[1], policyA, 1);

  const changedBody = { previous_response_id: same.value.id, instructions: policyB, input: 'CACHE-CHANGED-POLICY-USER', max_output_tokens: 64 };
  const changedCount = await countWithoutMutation(scenario, sessionId, changedBody);
  const changed = await create(changedBody, 'cache-instructions-changed');
  assert.equal(changed.value.status, 'completed');
  assertStablePrefix(scenario.state.requests[1], scenario.state.requests[2]);
  assertCurrent(scenario.state.requests[2], policyB, 2);
  assertMeasuredGeneration(changedCount, scenario.state.requests[2]);

  await context.restart();
  const replayed = await create(changedBody, 'cache-instructions-changed');
  assert.deepEqual(replayed.value, changed.value, 'Cold idempotent admission returns the exact retained Response');
  assert.equal(scenario.state.calls, 3, 'Cold retrieval/replay cannot execute or append another context transition');

  const resetBody = { previous_response_id: changed.value.id, input: 'CACHE-OMITTED-INSTRUCTIONS-USER', max_output_tokens: 64 };
  const resetCount = await countWithoutMutation(scenario, sessionId, resetBody);
  const reset = await create(resetBody, 'cache-instructions-reset');
  assert.equal(reset.value.status, 'completed');
  assertStablePrefix(scenario.state.requests[2], scenario.state.requests[3]);
  assertCurrent(scenario.state.requests[3], null, 3);
  assertMeasuredGeneration(resetCount, scenario.state.requests[3]);

  const explicitReset = await create({ previous_response_id: reset.value.id, instructions: null,
    input: 'CACHE-EXPLICIT-NULL-USER' }, 'cache-instructions-null');
  assert.equal(explicitReset.value.status, 'completed');
  assertStablePrefix(scenario.state.requests[3], scenario.state.requests[4]);
  assertCurrent(scenario.state.requests[4], null, 3);
  const current = await snapshot(scenario, sessionId);
  assert.deepEqual(current.pinned, pinned, 'Runtime revisions must not mutate the immutable original session pin');
  assert.ok(!JSON.stringify(current.messages).includes(marker), 'Private context events cannot pollute the public source journal');
  assert.deepEqual(await scenario.read(first.value.id), original, 'Later context transitions do not rewrite historical Responses');
  assert.deepEqual((await scenario.client.responses.inputItems.list(first.value.id)).data, originalInput);
  assert.equal(scenario.state.calls, 5);
  const ledger = await knownModelRows(scenario, [first.value.id, same.value.id, changed.value.id, reset.value.id, explicitReset.value.id]);
  return { provider_calls: 5, instruction_transitions: [policyA, policyB, null], unchanged_snapshot_deduplicated: true,
    missing_and_null_reset_equivalent: true, actual_prepared_count_equals_generation: true,
    cold_same_key_no_reexecution: true, original_pin_and_public_history_unchanged: true,
    model_ledger_rows: ledger, external_network_calls: 0, cache_hits_claimed: false };
}

function tool(name) {
  return { type: 'function', name, description: `Synthetic ${name} declaration`, strict: true,
    parameters: { type: 'object', properties: {}, required: [], additionalProperties: false } };
}

export async function runPromptCacheToolForkJourney(context) {
  const scenario = await createNativeSemanticsScenario(context, 'cache-tool-fork',
    Array.from({ length: 3 }, (_, index) => ({ output: [answer(index)], usage })));
  scenario.state.count = count;
  const retired = tool('retired_cache_tool');
  const current = tool('current_cache_tool');
  const first = await scenario.invoke({ input: 'CACHE-TOOLS-FIRST', instructions: 'CACHE-FORK-A',
    tools: [retired], max_output_tokens: 64 }, 200, 'cache-tools-source');
  assert.equal(first.value.status, 'completed');
  const sourceSession = first.headers.get('x-agents-session');
  const source = await scenario.read(first.value.id);
  const sourceItems = (await scenario.client.responses.inputItems.list(first.value.id)).data;
  const head = await scenario.invoke({ previous_response_id: first.value.id, input: 'CACHE-TOOLS-HEAD',
    instructions: 'CACHE-FORK-B', tools: [retired, current], max_output_tokens: 64 }, 200, 'cache-tools-head');
  assert.equal(head.value.status, 'completed');
  assertStablePrefix(scenario.state.requests[0], scenario.state.requests[1]);
  assertCurrent(scenario.state.requests[1], 'CACHE-FORK-B', 2);
  assert.deepEqual(scenario.state.requests[1].tools.map(value => value.name), [retired.name, current.name]);

  const branchBody = { previous_response_id: first.value.id, input: 'CACHE-TOOLS-BRANCH',
    instructions: 'CACHE-FORK-C', tools: [current], max_output_tokens: 64 };
  const measured = await countWithoutMutation(scenario, sourceSession, branchBody);
  const branch = await scenario.invoke(branchBody, 200, 'cache-tools-branch');
  assert.equal(branch.value.status, 'completed');
  assert.notEqual(branch.headers.get('x-agents-session'), sourceSession, 'A historical tool revision must not rewind the live Session');
  assertStablePrefix(scenario.state.requests[0], scenario.state.requests[2]);
  assertCurrent(scenario.state.requests[2], 'CACHE-FORK-C', 2);
  assertMeasuredGeneration(measured, scenario.state.requests[2]);
  assert.deepEqual(scenario.state.requests[2].tools.map(value => value.name), [current.name],
    'Cache optimization cannot retain a removed alias in the callable provider catalog');
  const branchText = JSON.stringify(scenario.state.requests[2].input);
  assert.ok(!branchText.includes('CACHE-FORK-B') && !branchText.includes('CACHE-TOOLS-HEAD'),
    'A historical context snapshot excludes every later instruction and user-input revision');
  assert.deepEqual(await scenario.read(first.value.id), source);
  assert.deepEqual((await scenario.client.responses.inputItems.list(first.value.id)).data, sourceItems);
  assert.equal(scenario.state.calls, 3);
  const rows = await knownModelRows(scenario, [first.value.id, head.value.id, branch.value.id]);
  return { provider_calls: 3, actual_tool_set_replaced: true, old_alias_not_callable: true,
    historical_source_excludes_future_context: true, branch_count_equals_generation: true,
    independent_branch_session: true, historical_response_immutable: true,
    model_ledger_rows: rows, external_network_calls: 0, cache_hits_claimed: false };
}

export async function runPromptCacheCompactionJourney(context) {
  const summary = 'CACHE-COMPACTION-NEW-WINDOW';
  const historical = 'CACHE-HISTORICAL-DEVELOPER';
  const oldUser = 'CACHE-COMPACTION-OLD-USER';
  const scenario = await createNativeSemanticsScenario(context, 'cache-compaction', [
    { output: [answer('before-compact')], usage },
    { output: [{ ...answer('summary'), content: [{ type: 'output_text', text: summary, annotations: [], logprobs: [] }] }], usage,
      check: body => {
        assert.ok(JSON.stringify(body.input).includes(oldUser), 'The summary sees the original native user context');
        assert.ok(JSON.stringify(body.input).includes(historical));
      } },
    { output: [answer('after-compact')], usage, check: body => {
      const current = updates(body);
      assert.equal(current.length, 1, 'Compaction carries one latest trusted snapshot into the new native generation');
      assert.equal(current[0].value.instructions, policyB);
      assert.ok(!JSON.stringify(body.input).includes(policyA), 'Compaction cannot revive a historical response policy');
      assert.ok(!JSON.stringify(body.input).includes(oldUser), 'Generation uses the replaced working view, not the old source window');
      assert.ok(JSON.stringify(body.input).includes(historical), 'Ordinary protected developer guidance survives the new context layout');
      assert.ok(JSON.stringify(body.input).includes(summary));
      assert.ok(text(body.input[0]).includes(fixed));
    } },
    { output: [answer('reset-after-compact')], usage },
  ]);
  scenario.state.count = count;
  const source = await scenario.invoke({ instructions: policyA, input: [
    { role: 'developer', content: historical }, { role: 'user', content: oldUser },
  ], max_output_tokens: 64 }, 200, 'cache-compaction-source');
  assert.equal(source.value.status, 'completed');
  const original = await scenario.read(source.value.id);
  const originalItems = (await scenario.client.responses.inputItems.list(source.value.id)).data;
  const compact = await scenario.invoke({ previous_response_id: source.value.id,
    instructions: policyB, input: 'CACHE-COMPACTION-CURRENT-TASK', max_output_tokens: 64,
    context_management: [{ type: 'compaction', compact_threshold: 1 }],
  }, 200, 'cache-compaction-once');
  assert.equal(compact.value.status, 'completed');
  assert.equal(compact.value.output.filter(item => item.type === 'compaction').length, 1);
  assert.equal(scenario.state.calls, 3, 'The changed context buys exactly one summary and one continuation');
  const resetBody = { previous_response_id: compact.value.id, input: 'CACHE-COMPACTION-RESET', max_output_tokens: 64 };
  const measured = await countWithoutMutation(scenario, compact.headers.get('x-agents-session'), resetBody);
  const reset = await scenario.invoke(resetBody, 200, 'cache-compaction-reset');
  assert.equal(reset.value.status, 'completed');
  assertStablePrefix(scenario.state.requests[2], scenario.state.requests[3]);
  assertCurrent(scenario.state.requests[3], null, 2);
  assertMeasuredGeneration(measured, scenario.state.requests[3]);
  assert.deepEqual(await scenario.read(source.value.id), original);
  assert.deepEqual((await scenario.client.responses.inputItems.list(source.value.id)).data, originalItems);
  assert.equal(scenario.state.calls, 4);
  const db = await scenario.mf.getD1Database('DB');
  const rows = (await db.prepare(`SELECT r.state,l.price_version FROM usage_ledger l
    JOIN usage_reservations r ON r.id=l.reservation_id WHERE r.account_id=? AND l.resource='model'`)
    .bind(scenario.principal.owner).all()).results;
  assert.equal(rows.length, 4);
  assert.ok(rows.every(row => row.state === 'settled' && row.price_version === 'byok'));
  return { provider_calls: 4, summaries: 1, current_snapshot_rebased_once: true,
    stale_response_policy_not_reintroduced: true, ordinary_protected_roles_preserved: true,
    post_compaction_reset_and_count_match: true, old_source_response_immutable: true,
    model_ledger_rows: rows.length, external_network_calls: 0, cache_hits_claimed: false };
}
