import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { isDeepStrictEqual } from 'node:util';
import { createNativeSemanticsScenario, collectNativeEvents } from './native-semantics-journey.mjs';

const dispatcher = '__agents_call_tool';
const catalogStart = '<agents_tool_catalog>\n';
const catalogEnd = '\n</agents_tool_catalog>';
const initial = 'DYNAMIC-CONTEXT-INITIAL-POLICY';
const changed = 'DYNAMIC-CONTEXT-UPDATED-POLICY';
const finalPolicy = 'DYNAMIC-CONTEXT-FINAL-POLICY';
const fields = ['model', 'input', 'tools', 'tool_choice', 'parallel_tool_calls', 'text', 'reasoning', 'truncation', 'personality'];
const counted = body => Object.fromEntries(fields.filter(key => Object.hasOwn(body, key)).map(key => [key, body[key]]));
const measure = body => Buffer.byteLength(JSON.stringify(counted(body)));
const usage = body => ({ input_tokens: measure(body), output_tokens: 1, total_tokens: measure(body) + 1,
  input_tokens_details: { cached_tokens: 0, cache_write_tokens: 0 }, output_tokens_details: { reasoning_tokens: 0 } });
const text = item => typeof item.content === 'string' ? item.content : (item.content ?? []).map(part => part.text ?? '').join('');
const answer = value => ({ type: 'message', id: `upstream-dynamic-${value}`, role: 'assistant', status: 'completed',
  content: [{ type: 'output_text', text: value, annotations: [], logprobs: [] }] });
const responseAction = (value, check = () => {}) => ({ output: [answer(value)], usage, check });
const session = (scenario, id) => scenario.request('GET', `/v1/sessions/${id}`, undefined, 200, scenario.principal.apiKey);
const patch = (scenario, change) => scenario.request('PATCH', `/v1/agents/${scenario.agent.id}`, change, 200, scenario.principal.apiKey);

function catalogs(body) {
  return body.input.flatMap((item, index) => {
    const value = text(item);
    if (!value.startsWith(catalogStart)) return [];
    assert.equal(item.role, 'user', 'Tool schemas are ordinary context, not system instructions');
    assert.ok(value.endsWith(catalogEnd));
    const catalog = JSON.parse(value.slice(catalogStart.length, -catalogEnd.length));
    assert.match(catalog.revision, /^[a-f0-9]{64}$/);
    assert.equal(catalog.configuration_revision, undefined, 'Instruction-only Agent metadata cannot churn an unchanged tool catalog');
    assert.ok(Array.isArray(catalog.tools));
    return [{ index, catalog }];
  });
}

function latestCatalog(body, names) {
  const entries = catalogs(body);
  assert.ok(entries.length, 'Generation receives an explicit effective catalog, including an empty one');
  const catalog = entries.at(-1).catalog;
  assert.deepEqual(catalog.tools.map(tool => tool.name).sort(), [...names].sort());
  assert.equal(body.tool_choice, 'auto', 'A disabled tool set would not test active schema rendering');
  const declaration = body.tools.filter(tool => tool.name === dispatcher);
  assert.equal(declaration.length, 1, 'The real fixed-schema dispatcher exists even for an empty catalog');
  assert.equal(declaration[0].type, 'function');
  assert.equal(declaration[0].parameters.properties.name.type, 'string');
  assert.equal(declaration[0].parameters.properties.arguments.type, 'object');
  assert.deepEqual(Object.keys(declaration[0].parameters.properties).sort(), ['arguments', 'name'],
    'The model does not need to echo trusted catalog provenance in its arguments');
  for (const tool of catalog.tools) {
    assert.ok(!body.tools.some(value => value.name === tool.name), 'Registered schemas cannot also be prepended through top-level tools');
    assert.ok(!JSON.stringify(body.input.filter(item => item.role === 'system')).includes(tool.description),
      'Mutable tool descriptions do not migrate back into system messages');
  }
  assert.ok(!text(body.input[0]).includes(initial), 'The original Agent policy is not frozen inside the first system message');
  return catalog;
}

function sameProviderPrefix(before, after) {
  assert.deepEqual(after.tools, before.tools, 'Actual callable provider declarations stay byte-identical across registered catalog edits');
  assert.deepEqual(after.input.slice(0, before.input.length), before.input,
    'Already submitted context stays intact; new instructions and tool catalogs append at their effective boundary');
}

function output(body, callId) {
  const item = body.input.find(item => item.type === 'function_call_output' && item.call_id === callId);
  assert.ok(item, `The actual matching ${callId} result reaches the next provider round`);
  const value = typeof item.output === 'string' ? item.output : item.output.map(part => {
    assert.equal(part.type, 'input_text'); return part.text;
  }).join('');
  return JSON.parse(value);
}

function dispatchAction(id, name, arguments_, check = () => {}) {
  const action = { output: [], usage, check: body => {
    check(body);
    action.output = [{ type: 'function_call', id: `upstream-${id}`, call_id: id,
      name: dispatcher, arguments: JSON.stringify({ name, arguments: arguments_ }), status: 'completed' }];
  } };
  return action;
}

async function preview(scenario, id, request) {
  const before = await session(scenario, id);
  const begin = scenario.state.countRequests.length;
  const calls = scenario.state.calls;
  const { max_output_tokens: _budget, stream: _stream, ...body } = request;
  const value = await scenario.client.responses.inputTokens.count({ model: scenario.agent.id, ...body });
  const after = await session(scenario, id);
  for (const field of ['configuration', 'messages', 'input_revision', 'last_input_at_ms', 'run_id']) {
    assert.deepEqual(after[field], before[field], `Count planning does not mutate ${field}`);
  }
  assert.equal(scenario.state.calls, calls);
  return { value, requests: scenario.state.countRequests.slice(begin) };
}

function matchesPreview(preview, body) {
  assert.equal(preview.value.input_tokens, measure(body));
  assert.ok(preview.requests.some(request => isDeepStrictEqual(request, counted(body))),
    'Count and generation use the same layout, callable dispatcher and versioned catalog');
}

async function modelRows(scenario, expected) {
  const rows = (await (await scenario.mf.getD1Database('DB')).prepare(`SELECT r.state,l.price_version FROM usage_ledger l
    JOIN usage_reservations r ON r.id=l.reservation_id WHERE r.account_id=? AND l.resource='model'`)
    .bind(scenario.principal.owner).all()).results;
  assert.equal(rows.length, expected);
  assert.ok(rows.every(row => row.state === 'settled' && row.price_version === 'byok'));
  return rows.length;
}

function canonical(value) {
  if (Array.isArray(value)) return value.map(canonical);
  if (value && typeof value === 'object') return Object.fromEntries(Object.keys(value).sort().map(key => [key, canonical(value[key])]));
  return value;
}

async function legacyClockSession(scenario, context) {
  await patch(scenario, { instructions: initial, tools: ['time'], workspace: true });
  const record = await scenario.request('POST', `/v1/agents/${scenario.agent.id}/sessions`, {}, 201, scenario.principal.apiKey);
  const namespace = await context.mf.getDurableObjectNamespace('SESSIONS');
  const actor = namespace.get(namespace.idFromName(`${scenario.principal.owner}:${record.id}`));
  const response = await actor.fetch('https://fixture/__test/dynamic-legacy-seed', { method: 'POST' });
  assert.equal(response.status, 200);
  const seeded = await response.json();
  const pinHash = createHash('sha256').update(JSON.stringify(canonical(seeded.record.pinned))).digest('hex');
  await (await context.mf.getD1Database('DB')).prepare('UPDATE sessions SET config_snapshot_json=?,instructions_hash=? WHERE account_id=? AND id=?')
    .bind(JSON.stringify(seeded.record), pinHash, scenario.principal.owner, record.id).run();
  await context.restart();
  return { id: record.id, ...seeded };
}

export async function runDynamicContextLifecycleJourney(context) {
  let agentA; let agentB; let agentInstructions; let agentC; let agentD;
  let initialRevision; let timeRevision; let timeCatalogCount;
  let legacy;
  const scenario = await createNativeSemanticsScenario(context, 'dynamic-lifecycle', [
    responseAction('DYNAMIC-INITIAL', body => {
      initialRevision = latestCatalog(body, []).revision;
      const callIndex = body.input.findIndex(item => item.type === 'function_call' && item.call_id === legacy.call_id);
      assert.ok(callIndex >= 0, 'An actual old bare-call record survives v2 adoption even though the tool is no longer available');
      assert.equal(body.input[callIndex].name, 'time');
      assert.equal(body.input[callIndex + 1].type, 'function_call_output');
      assert.equal(body.input[callIndex + 1].call_id, legacy.call_id);
      assert.deepEqual(output(body, legacy.call_id), legacy.result);
      assert.notEqual(text(body.input[0]), legacy.legacy_prefix, 'Old pin bytes are provenance, not the live rendering invariant');
    }),
    dispatchAction('dynamic_live_time', 'time', {}, body => {
      timeRevision = latestCatalog(body, ['time']).revision;
      timeCatalogCount = catalogs(body).length;
    }),
    responseAction('DYNAMIC-TIME-OBSERVED', body => {
      latestCatalog(body, ['time']);
      assert.ok(Number.isSafeInteger(output(body, 'dynamic_live_time').unix_ms), 'The actual Rust harness executed the newly enabled clock');
    }),
    responseAction('DYNAMIC-INSTRUCTIONS-ONLY', body => {
      assert.equal(latestCatalog(body, ['time']).revision, timeRevision,
        'An instruction-only revision cannot stale an otherwise valid tool call');
      assert.equal(catalogs(body).length, timeCatalogCount, 'Unchanged tools are not retransmitted merely because Agent instructions changed');
      assert.ok(JSON.stringify(body.input).includes(agentInstructions.instructions));
    }),
    responseAction('DYNAMIC-BASH-CATALOG', body => latestCatalog(body, ['bash'])),
    responseAction('DYNAMIC-EMPTY-CATALOG', body => {
      assert.equal(latestCatalog(body, []).revision, initialRevision,
        'The digest describes actual catalog content, not an ever-changing Agent revision counter');
    }),
  ]);
  scenario.state.count = measure;
  legacy = await legacyClockSession(scenario, context);
  agentA = await patch(scenario, { instructions: initial, tools: [], workspace: true });
  const response = await scenario.mf.dispatchFetch('https://agents.test/v1/responses', { method: 'POST',
    headers: { Authorization: `Bearer ${scenario.principal.apiKey}`, 'Content-Type': 'application/json',
      'X-Agents-Session': legacy.id, 'Idempotency-Key': 'dynamic-initial' },
    body: JSON.stringify({ model: scenario.agent.id, input: 'DYNAMIC-LIFECYCLE-START', tool_choice: 'auto', max_output_tokens: 128 }) });
  const first = { value: await response.json(), headers: response.headers };
  if (scenario.state.failure) throw scenario.state.failure;
  assert.equal(response.status, 200, JSON.stringify(first.value));
  assert.equal(first.value.status, 'completed');
  const id = first.headers.get('x-agents-session');
  assert.equal(id, legacy.id, 'A legacy active Session is adopted, not silently replaced');
  const source = await scenario.read(first.value.id);
  agentB = await patch(scenario, { instructions: changed, tools: ['time'] });
  const useTime = { previous_response_id: first.value.id, input: 'Use the newly available clock.', tool_choice: 'auto', max_output_tokens: 128, stream: true };
  const countedTime = await preview(scenario, id, useTime);
  const events = await collectNativeEvents(await scenario.invoke(useTime, 200, 'dynamic-time'));
  const second = events.at(-1).response;
  assert.equal(second.status, 'completed');
  const calls = second.output.filter(item => item.type === 'function_call');
  assert.deepEqual(calls, [], 'Server-owned tools remain private rather than becoming fabricated caller-local function requests');
  assert.ok(!JSON.stringify(events).includes(dispatcher), 'SSE never exposes the private provider dispatcher envelope');
  const deltas = events.filter(event => event.type === 'response.function_call_arguments.delta').map(event => event.delta).join('');
  assert.equal(deltas, '', 'Partial server-owned envelopes cannot leak through argument delta events');
  const toolJournal = (await session(scenario, id)).messages.flatMap(message => message.tool_calls ?? []);
  const logical = toolJournal.find(call => call.id === 'dynamic_live_time');
  assert.ok(logical); assert.equal(logical.function.name, 'time'); assert.equal(logical.function.arguments, '{}');
  matchesPreview(countedTime, scenario.state.requests[1]);
  sameProviderPrefix(scenario.state.requests[0], scenario.state.requests[1]);
  const replay = await collectNativeEvents(await scenario.invoke(useTime, 200, 'dynamic-time'));
  assert.deepEqual(replay.at(-1).response, second);
  assert.equal(scenario.state.calls, 3, 'An idempotent replay cannot double-dispatch the new wrapper or clock');
  agentInstructions = await patch(scenario, { instructions: finalPolicy });
  const instructions = { previous_response_id: second.id, input: 'Instruction-only continuation.', tool_choice: 'auto', max_output_tokens: 128 };
  const countedInstructions = await preview(scenario, id, instructions);
  const third = await scenario.invoke(instructions, 200, 'dynamic-instructions');
  assert.equal((await session(scenario, id)).configuration.agent_revision, agentInstructions.revision);
  matchesPreview(countedInstructions, scenario.state.requests[3]);
  sameProviderPrefix(scenario.state.requests[2], scenario.state.requests[3]);
  agentC = await patch(scenario, { tools: ['bash'] });
  const bash = { previous_response_id: third.value.id, input: 'Observe the replacement catalog without executing it.', tool_choice: 'auto', max_output_tokens: 128 };
  const countedBash = await preview(scenario, id, bash);
  const fourth = await scenario.invoke(bash, 200, 'dynamic-bash');
  matchesPreview(countedBash, scenario.state.requests[4]);
  sameProviderPrefix(scenario.state.requests[3], scenario.state.requests[4]);
  agentD = await patch(scenario, { tools: [] });
  const empty = { previous_response_id: fourth.value.id, input: 'Observe the empty current catalog.', tool_choice: 'auto', max_output_tokens: 128 };
  const countedEmpty = await preview(scenario, id, empty);
  const fifth = await scenario.invoke(empty, 200, 'dynamic-empty');
  assert.equal(fifth.value.status, 'completed');
  matchesPreview(countedEmpty, scenario.state.requests[5]);
  sameProviderPrefix(scenario.state.requests[4], scenario.state.requests[5]);
  assert.deepEqual(await scenario.read(first.value.id), source, 'Live policy changes do not rewrite sealed historical Responses');
  const final = await session(scenario, id);
  assert.equal(final.configuration.agent_revision, agentD.revision);
  assert.equal(final.messages.flatMap(message => message.tool_calls ?? []).find(call => call.id === legacy.call_id).function.name, 'time',
    'The historical public call identity and logical name are not rewritten into a new dispatcher call');
  const legacyResults = final.messages.filter(message => message.role === 'tool' && message.tool_call_id === legacy.call_id);
  assert.equal(legacyResults.length, 1, 'Migration cannot append a second execution result for an already completed old call');
  assert.deepEqual(JSON.parse(legacyResults[0].content), legacy.result);
  return { provider_calls: 6, actual_clock_calls: 1, active_provider_schema_unchanged: true,
    legacy_fixture: 'completed bare clock call/result in pre-cache Session snapshot',
    original_legacy_call_id_preserved: legacy.call_id, legacy_effect_replayed: false,
    catalog_sequence: ['empty', 'time', 'time-instructions-only', 'bash', 'empty'],
    instruction_only_catalog_digest_stable: true, exact_read_only_count_matches_generation: true,
    public_stream_keeps_server_dispatch_private: true, journal_keeps_logical_tool_name_and_arguments: true, old_business_journeys_replayed: 0,
    model_ledger_rows: await modelRows(scenario, 6), external_network_calls: 0 };
}

function latch() {
  let resolve;
  return { promise: new Promise(done => { resolve = done; }), resolve: () => resolve() };
}

export async function runDynamicContextStaleJourney(context) {
  const entered = latch(); const release = latch();
  const enteredRemoval = latch(); const releaseRemoval = latch();
  let agentB; let agentC; let oldRevision; let currentRevision;
  const local = { type: 'function', name: 'local_probe', description: 'The caller returns an independent observation.',
    parameters: { type: 'object', properties: { value: { type: 'integer' } }, required: ['value'], additionalProperties: false } };
  const stale = dispatchAction('dynamic_stale_time', 'time', {}, body => { oldRevision = latestCatalog(body, ['time']).revision; entered.resolve(); });
  stale.gate = release;
  const removed = dispatchAction('dynamic_revoked_time', 'time', {}, body => {
    currentRevision = latestCatalog(body, ['time', 'bash']).revision;
    assert.notEqual(currentRevision, oldRevision);
    const result = output(body, 'dynamic_stale_time');
    assert.equal(result.error.code, 'tool_catalog_changed');
    assert.equal(result.error.revision, currentRevision, 'The correction carries the actual current catalog revision');
    assert.equal(result.unix_ms, undefined);
    enteredRemoval.resolve();
  });
  removed.gate = releaseRemoval;
  const scenario = await createNativeSemanticsScenario(context, 'dynamic-stale', [
    responseAction('DYNAMIC-STALE-SEED', body => latestCatalog(body, ['time'])),
    stale,
    removed,
    dispatchAction('dynamic_invalid_bash', 'bash', { command: 17 }, body => {
      latestCatalog(body, ['bash']);
      const result = output(body, 'dynamic_revoked_time');
      assert.equal(result.error.code, 'tool_revoked'); assert.equal(result.unix_ms, undefined);
    }),
    { output: [{ type: 'function_call', id: 'upstream-dynamic-client', call_id: 'dynamic_client_probe',
      name: 'local_probe', arguments: '{"value":17}', status: 'completed' }], usage, check: body => {
      assert.equal(output(body, 'dynamic_invalid_bash').error.code, 'invalid_tool_arguments',
        'The fixed outer schema cannot replace backend validation of a tool-specific input schema');
      assert.ok(body.tools.some(tool => tool.name === 'local_probe'), 'Caller-local functions keep their own declarations and grants');
    } },
    responseAction('DYNAMIC-CLIENT-OBSERVATION-17', body => {
      const result = body.input.find(item => item.type === 'function_call_output' && item.call_id === 'dynamic_client_probe');
      assert.ok(result, 'An ordinary caller-local callback remains paired with its real provider call');
      assert.ok(JSON.stringify(result.output).includes('CLIENT-OBSERVATION-17'));
    }),
  ]);
  scenario.state.count = measure;
  await patch(scenario, { instructions: initial, tools: ['time'], workspace: true });
  const first = await scenario.invoke({ input: 'DYNAMIC-STALE-START', tools: [local], tool_choice: 'auto', max_output_tokens: 256 }, 200, 'dynamic-stale-start');
  const id = first.headers.get('x-agents-session');
  const pending = scenario.invoke({ previous_response_id: first.value.id, input: 'A queued old catalog decision.',
    tools: [local], tool_choice: 'auto', max_output_tokens: 256 }, 200, 'dynamic-stale-pending');
  try {
    await Promise.race([entered.promise, pending.then(() => { throw Error('The old decision did not reach the controlled busy boundary'); }),
      new Promise((_, reject) => setTimeout(() => reject(Error('Busy provider boundary deadline')), 10000).unref())]);
    agentB = await patch(scenario, { instructions: changed, tools: ['time', 'bash'] });
    release.resolve();
    await Promise.race([enteredRemoval.promise, pending.then(() => { throw Error('The current-catalog decision did not reach the removal boundary'); }),
      new Promise((_, reject) => setTimeout(() => reject(Error('Removal provider boundary deadline')), 10000).unref())]);
    agentC = await patch(scenario, { tools: ['bash'] });
    releaseRemoval.resolve();
    const result = await pending;
    assert.equal(result.value.status, 'completed');
    const client = result.value.output.filter(item => item.type === 'function_call').find(item => item.name === 'local_probe');
    assert.ok(client); assert.equal(client.arguments, '{"value":17}');
    assert.ok(!JSON.stringify(result.value).includes(dispatcher));
    const final = await scenario.invoke({ previous_response_id: result.value.id,
      input: [{ type: 'function_call_output', call_id: client.call_id, output: 'CLIENT-OBSERVATION-17' }],
      tools: [local], tool_choice: 'auto', max_output_tokens: 256 }, 200, 'dynamic-local-return');
    assert.equal(final.value.status, 'completed');
    assert.equal(final.headers.get('x-agents-session'), id);
    const effects = (await session(scenario, id)).messages.filter(message => message.role === 'tool');
    assert.equal(effects.filter(message => JSON.stringify(message.content).includes('unix_ms')).length, 0,
      'Neither stale nor revoked decisions can execute the old clock');
    assert.equal(scenario.state.calls, 6);
    return { provider_calls: 6, actual_clock_calls: 0, actual_workspace_starts: 0,
      stale_catalog_closed_with_current_revision: true, removed_tool_closed_without_effect: true,
      inner_schema_enforced: true, caller_local_callback_preserved: true,
      model_ledger_rows: await modelRows(scenario, 6), external_network_calls: 0 };
  } finally { release.resolve(); releaseRemoval.resolve(); await pending.catch(() => {}); }
}

export async function runDynamicContextCompactJourney(context) {
  const path = '/workspace/dynamic-context-proof.txt';
  const summaryMarker = 'DYNAMIC-SUMMARY-RETAINED-PATH-AND-CLOCK';
  let agentB; let firstClock; let secondClock;
  const summary = responseAction('placeholder');
  summary.check = body => {
    assert.deepEqual(body.tools, [], 'The summarizer observes source data rather than gaining dispatch authority');
    assert.match(text(body.input[0]), /^Summarize this agent session for continuation\./);
    const source = JSON.stringify(body.input);
    assert.ok(source.includes(path) && source.includes(String(firstClock)) && source.includes('dynamic_memory_time'));
    summary.output = [answer(`${summaryMarker}: path=${path}; original_clock_unix_ms=${firstClock}; retain the current task.`)];
  };
  const scenario = await createNativeSemanticsScenario(context, 'dynamic-compact', [
    dispatchAction('dynamic_memory_time', 'time', {}, body => latestCatalog(body, ['time'])),
    responseAction('DYNAMIC-MEMORY-SOURCE', body => { firstClock = output(body, 'dynamic_memory_time').unix_ms; assert.ok(Number.isSafeInteger(firstClock)); }),
    summary,
    responseAction('DYNAMIC-COMPACT-CONTINUATION', body => {
      assert.equal(catalogs(body).length, 1, 'Compaction retains only the current effective tool catalog');
      latestCatalog(body, ['time', 'bash']);
      assert.ok(JSON.stringify(body.input).includes(`${summaryMarker}: path=${path}; original_clock_unix_ms=${firstClock}`));
      assert.ok(!JSON.stringify(body.input).includes('DYNAMIC-UNCOMPACTED-USER-WINDOW'));
      assert.ok(JSON.stringify(body.input).includes(changed));
    }),
    dispatchAction('dynamic_cold_time', 'time', {}, body => {
      latestCatalog(body, ['time', 'bash']);
      assert.equal(catalogs(body).length, 1, 'Cold recovery cannot append the same catalog again');
      assert.ok(JSON.stringify(body.input).includes(path) && JSON.stringify(body.input).includes(String(firstClock)));
    }),
    responseAction('DYNAMIC-COLD-MEMORY-OBSERVED', body => { secondClock = output(body, 'dynamic_cold_time').unix_ms; assert.ok(Number.isSafeInteger(secondClock)); }),
    responseAction('DYNAMIC-HISTORICAL-FORK', body => {
      latestCatalog(body, ['time', 'bash']);
      assert.ok(JSON.stringify(body.input).includes(path));
      assert.ok(!JSON.stringify(body.input).includes('DYNAMIC-COLD-USER-REQUEST'), 'A sealed old boundary cannot borrow later source turns');
      assert.ok(!JSON.stringify(body.input).includes('DYNAMIC-COLD-MEMORY-OBSERVED'));
    }),
  ]);
  scenario.state.count = measure;
  await patch(scenario, { instructions: initial, tools: ['time'], workspace: true });
  const first = await scenario.invoke({ input: `DYNAMIC-UNCOMPACTED-USER-WINDOW: retain exact path ${path} and observe the clock.`,
    tool_choice: 'auto', max_output_tokens: 256 }, 200, 'dynamic-memory-source');
  const id = first.headers.get('x-agents-session');
  const source = await scenario.read(first.value.id);
  const sourceItems = (await scenario.client.responses.inputItems.list(first.value.id)).data;
  agentB = await patch(scenario, { instructions: changed, tools: ['time', 'bash'] });
  const compact = await scenario.invoke({ previous_response_id: first.value.id, input: 'Compact and retain that exact path and clock observation.',
    tool_choice: 'auto', max_output_tokens: 256, context_management: [{ type: 'compaction', compact_threshold: 1 }] }, 200, 'dynamic-memory-compact');
  assert.equal(compact.value.status, 'completed');
  assert.equal(compact.value.output.filter(item => item.type === 'compaction').length, 1);
  await context.restart();
  const coldBody = { previous_response_id: compact.value.id, input: 'DYNAMIC-COLD-USER-REQUEST: retain the old path and use the current clock.',
    tool_choice: 'auto', max_output_tokens: 256 };
  const coldCount = await preview(scenario, id, coldBody);
  const cold = await scenario.invoke(coldBody, 200, 'dynamic-memory-cold');
  assert.equal(cold.value.status, 'completed');
  assert.equal(cold.headers.get('x-agents-session'), id);
  matchesPreview(coldCount, scenario.state.requests[4]);
  const forkBody = { previous_response_id: first.value.id, input: 'An independent historical continuation of the original path.',
    tool_choice: 'auto', max_output_tokens: 256 };
  const forkCount = await preview(scenario, id, forkBody);
  const fork = await scenario.invoke(forkBody, 200, 'dynamic-memory-fork');
  assert.equal(fork.value.status, 'completed');
  assert.notEqual(fork.headers.get('x-agents-session'), id);
  matchesPreview(forkCount, scenario.state.requests[6]);
  assert.deepEqual(await scenario.read(first.value.id), source);
  assert.deepEqual((await scenario.client.responses.inputItems.list(first.value.id)).data, sourceItems);
  return { provider_calls: 7, actual_clock_calls: 2, summaries: 1, concrete_retained_path: path,
    exact_original_clock_retained: Number.isSafeInteger(firstClock), cold_current_clock_observed: Number.isSafeInteger(secondClock),
    latest_catalog_and_policy_survive_compaction: true, historical_source_unchanged: true,
    model_ledger_rows: await modelRows(scenario, 7), external_network_calls: 0 };
}
