import assert from 'node:assert/strict';
import { isDeepStrictEqual } from 'node:util';
import { createNativeSemanticsScenario } from './native-semantics-journey.mjs';

const start = '<agents_configuration_update>\n';
const end = '\n</agents_configuration_update>';
const initial = 'SESSION-CONFIG-INITIAL-A';
const changed = 'SESSION-CONFIG-UPDATED-B';
const latest = 'SESSION-CONFIG-LATEST-C';
const countFields = ['model', 'input', 'tools', 'tool_choice', 'parallel_tool_calls', 'text',
  'reasoning', 'truncation', 'personality'];
const counted = body => Object.fromEntries(countFields.filter(key => Object.hasOwn(body, key))
  .map(key => [key, body[key]]));
const measure = body => Buffer.byteLength(JSON.stringify(counted(body)));
const usage = body => ({ input_tokens: measure(body), output_tokens: 1, total_tokens: measure(body) + 1,
  input_tokens_details: { cached_tokens: 0, cache_write_tokens: 0 },
  output_tokens_details: { reasoning_tokens: 0 } });
const answer = name => ({ type: 'message', id: `upstream-config-${name}`, role: 'assistant', status: 'completed',
  content: [{ type: 'output_text', text: `config-answer-${name}`, annotations: [], logprobs: [] }] });
const callTime = name => ({ type: 'function_call', id: `upstream-config-${name}`, call_id: name,
  name: 'time', arguments: '{}', status: 'completed' });
const text = item => typeof item.content === 'string' ? item.content : (item.content ?? []).map(part => part.text ?? '').join('');

function configurations(body) {
  return body.input.flatMap((item, index) => {
    const content = text(item);
    if (!content.startsWith(start)) return [];
    assert.equal(item.role, 'system', 'An actual platform revision remains authoritative, not caller text');
    assert.ok(content.endsWith(end));
    const value = JSON.parse(content.slice(start.length, -end.length));
    assert.ok(Number.isSafeInteger(value.agent_revision) && value.agent_revision >= 0);
    assert.equal(typeof value.instructions, 'string');
    assert.ok(Array.isArray(value.tools));
    return [{ index, value }];
  });
}

function currentConfiguration(body, agent, count, unavailable = []) {
  const events = configurations(body);
  assert.equal(events.length, count, 'The same effective Agent revision cannot append repeatedly during tool rounds/recovery');
  assert.equal(events.at(-1).value.agent_revision, agent.revision);
  assert.equal(events.at(-1).value.instructions, agent.instructions);
  const available = agent.tools.filter(name => !unavailable.includes(name)).sort();
  assert.deepEqual(events.at(-1).value.tools.map(tool => tool.name), available);
  assert.deepEqual(events.at(-1).value.unavailable_tools ?? [], unavailable);
  assert.deepEqual(body.tools.map(tool => tool.name), available,
    'The actual provider catalog matches effective permissions, not historical or incompatible tools');
  return events;
}

function retainsPrefix(before, after) {
  assert.deepEqual(after.input.slice(0, before.input.length), before.input,
    'An Agent edit appends a revision instead of rewriting already-submitted input');
  assert.deepEqual(after.input[0], before.input[0]);
  assert.ok(text(after.input[0]).includes(initial), 'The original fixed prefix remains the original pin');
}

const session = (scenario, id) => scenario.request('GET', `/v1/sessions/${id}`, undefined, 200, scenario.principal.apiKey);
const patch = (scenario, body) => scenario.request('PATCH', `/v1/agents/${scenario.agent.id}`, body, 200, scenario.principal.apiKey);

async function preview(scenario, id, body) {
  const before = await session(scenario, id);
  const start = scenario.state.countRequests.length;
  const calls = scenario.state.calls;
  const { max_output_tokens: _output, ...input } = body;
  const result = await scenario.client.responses.inputTokens.count({ model: scenario.agent.id, ...input });
  const after = await session(scenario, id);
  for (const key of ['pinned', 'configuration', 'messages', 'input_revision', 'last_input_at_ms', 'run_id']) {
    assert.deepEqual(after[key], before[key], `Count is read-only for Session ${key}`);
  }
  assert.equal(scenario.state.calls, calls);
  return { result, requests: scenario.state.countRequests.slice(start) };
}

function matchesPreview(preview, generation) {
  assert.ok(preview.requests.some(request => isDeepStrictEqual(request, counted(generation))),
    'Read-only latest-configuration planning and generation must submit the same counted representation');
  assert.equal(preview.result.input_tokens, measure(generation));
}

export function toolResult(body, id) {
  const item = body.input.find(item => item.type === 'function_call_output' && item.call_id === id);
  assert.ok(item, `The next round must receive the genuine ${id} effect/denial result`);
  assert.ok(Array.isArray(item.output) && item.output.length > 0,
    'The server tool renderer returns a nonempty native content-part array');
  assert.ok(item.output.every(part => part.type === 'input_text' && typeof part.text === 'string'),
    'Clock/denial results must preserve actual text parts, not stringify arbitrary objects');
  return JSON.parse(item.output.map(part => part.text).join(''));
}

async function modelRows(scenario, expected) {
  const db = await scenario.mf.getD1Database('DB');
  const rows = (await db.prepare(`SELECT r.state,l.price_version FROM usage_ledger l
    JOIN usage_reservations r ON r.id=l.reservation_id WHERE r.account_id=? AND l.resource='model'`)
    .bind(scenario.principal.owner).all()).results;
  assert.equal(rows.length, expected);
  assert.ok(rows.every(row => row.state === 'settled' && row.price_version === 'byok'));
  return rows.length;
}

async function revisionRows(scenario) {
  const db = await scenario.mf.getD1Database('DB');
  return (await db.prepare(`SELECT revision,payload FROM agent_configuration_revisions
    WHERE account_id=? AND agent_id=? ORDER BY revision`)
    .bind(scenario.principal.owner, scenario.agent.id).all()).results;
}

export async function runSessionConfigLifecycleJourney(context) {
  let agentB;
  let agentC;
  const compacted = 'SESSION-CONFIG-COMPACTED-WINDOW';
  const scenario = await createNativeSemanticsScenario(context, 'config-lifecycle', [
    { output: [answer('initial')], usage },
    { output: [callTime('config_added_time')], usage, check: body => currentConfiguration(body, agentB, 1, ['bash']) },
    { output: [answer('time-result')], usage, check: body => {
      currentConfiguration(body, agentB, 1, ['bash']);
      const result = toolResult(body, 'config_added_time');
      assert.ok(Number.isSafeInteger(result.unix_ms));
      assert.ok(Math.abs(result.unix_ms - Date.now()) < 10000, 'Newly enabled time executes in the actual Rust harness');
    } },
    { output: [{ ...answer('summary'), content: [{ type: 'output_text', text: compacted, annotations: [], logprobs: [] }] }], usage,
      check: body => {
        // Trusted configuration is sealed separately, never executed as summary
        // guidance. The next real generation below must restore latest C itself.
        assert.equal(configurations(body).length, 0);
        assert.deepEqual(body.tools, []);
        assert.match(text(body.input[0]), /^Summarize this agent session for continuation\./);
        assert.ok(JSON.stringify(body.input).includes('SESSION-CONFIG-INITIAL-USER'),
          'Compaction receives the actual conversation source rather than a fabricated current-policy prompt');
        assert.ok(body.input.some(item => item.type === 'function_call_output' && item.call_id === 'config_added_time'));
      } },
    { output: [answer('compacted')], usage, check: body => {
      currentConfiguration(body, agentC, 1);
      assert.ok(JSON.stringify(body.input).includes(compacted));
      assert.ok(!JSON.stringify(body.input).includes('SESSION-CONFIG-INITIAL-USER'));
      assert.ok(text(body.input[0]).includes(initial));
    } },
    { output: [answer('cold')], usage, check: body => currentConfiguration(body, agentC, 1) },
  ]);
  scenario.state.count = measure;
  const agentA = await patch(scenario, { instructions: initial, tools: [] });
  const first = await scenario.invoke({ input: 'SESSION-CONFIG-INITIAL-USER', max_output_tokens: 64 }, 200, 'session-config-initial');
  assert.equal(first.value.status, 'completed');
  const id = first.headers.get('x-agents-session');
  const before = await session(scenario, id);
  const original = await scenario.read(first.value.id);
  const originalItems = (await scenario.client.responses.inputItems.list(first.value.id)).data;
  assert.equal(before.pinned.agent_revision, agentA.revision);
  assert.equal(before.configuration ?? null, null);
  agentB = await patch(scenario, { instructions: changed, tools: ['time', 'bash'], workspace: true });
  assert.ok(agentB.revision > agentA.revision);
  const afterPatch = await session(scenario, id);
  assert.equal(afterPatch.last_input_at_ms, before.last_input_at_ms, 'Management edits do not extend user-input inactivity');
  assert.equal(afterPatch.input_revision, before.input_revision);
  const continuation = { previous_response_id: first.value.id, input: 'SESSION-CONFIG-USE-ADDED-TIME', max_output_tokens: 64 };
  const countB = await preview(scenario, id, continuation);
  const second = await scenario.invoke(continuation, 200, 'session-config-enabled-time');
  assert.equal(second.value.status, 'completed');
  assert.equal(second.headers.get('x-agents-session'), id, 'A management edit cannot silently create a replacement Session');
  retainsPrefix(scenario.state.requests[0], scenario.state.requests[1]);
  matchesPreview(countB, scenario.state.requests[1]);
  const adopted = await session(scenario, id);
  assert.deepEqual(adopted.pinned, before.pinned, 'Configuration overlays cannot rewrite the original pin');
  assert.equal(adopted.pinned.workspace, false);
  assert.equal(adopted.configuration.agent_revision, agentB.revision);
  assert.equal(adopted.configuration.instructions, changed);
  assert.deepEqual(adopted.configuration.tools.map(tool => tool.name), ['time']);
  assert.deepEqual(adopted.configuration.unavailable_tools, ['bash'],
    'An Agent edit cannot silently expand a pre-existing Session\'s workspace authority');
  const repeated = await scenario.invoke(continuation, 200, 'session-config-enabled-time');
  assert.deepEqual(repeated.value, second.value);
  assert.equal(scenario.state.calls, 3, 'Replay cannot execute the newly enabled tool again');

  const beforeNoOp = await revisionRows(scenario);
  const noOp = await patch(scenario, { instructions: changed, tools: ['time', 'bash'], workspace: true });
  assert.ok(noOp.revision > agentB.revision);
  assert.deepEqual(await revisionRows(scenario), beforeNoOp,
    'A semantic no-op management edit cannot append a duplicate instruction/tool configuration');

  agentC = await patch(scenario, { instructions: latest, tools: [], workspace: false });
  assert.ok(agentC.revision > agentB.revision);
  const compact = await scenario.invoke({ previous_response_id: second.value.id,
    input: 'SESSION-CONFIG-COMPACT-NOW', max_output_tokens: 64,
    context_management: [{ type: 'compaction', compact_threshold: 1 }],
  }, 200, 'session-config-compact');
  assert.equal(compact.value.status, 'completed');
  assert.equal(compact.headers.get('x-agents-session'), id);
  assert.equal(compact.value.output.filter(item => item.type === 'compaction').length, 1);
  assert.equal(scenario.state.calls, 5);
  await context.restart();
  const restored = await session(scenario, id);
  assert.equal(restored.configuration.agent_revision, agentC.revision);
  assert.equal(restored.configuration.instructions, latest);
  assert.deepEqual(restored.configuration.tools, []);
  const coldBody = { previous_response_id: compact.value.id, input: 'SESSION-CONFIG-COLD-CONTINUE', max_output_tokens: 64 };
  const countCold = await preview(scenario, id, coldBody);
  const cold = await scenario.invoke(coldBody, 200, 'session-config-cold');
  assert.equal(cold.value.status, 'completed');
  assert.equal(cold.headers.get('x-agents-session'), id);
  retainsPrefix(scenario.state.requests[4], scenario.state.requests[5]);
  matchesPreview(countCold, scenario.state.requests[5]);
  assert.deepEqual(await scenario.read(first.value.id), original);
  assert.deepEqual((await scenario.client.responses.inputItems.list(first.value.id)).data, originalItems);
  const revisions = await revisionRows(scenario);
  assert.deepEqual(revisions.map(row => row.revision), [agentA.revision, agentB.revision, agentC.revision]);
  assert.deepEqual(revisions.map(row => JSON.parse(row.payload).instructions), [initial, changed, latest]);
  assert.ok(!(await session(scenario, id)).messages.some(message => text(message).startsWith(start)),
    'Internal configuration events are not manufactured caller source messages');
  return { provider_calls: 6, actual_time_calls: 1, summaries: 1, same_session: true,
    original_pin_and_old_response_immutable: true, latest_configuration_survives_compaction_and_cold: true,
    replay_and_semantic_no_op_have_no_duplicate_revision: true,
    incompatible_workspace_tool_reported_not_granted: true, count_preview_equals_generation: true,
    model_ledger_rows: await modelRows(scenario, 6), external_network_calls: 0 };
}

function latch() {
  let resolve;
  return { promise: new Promise(done => { resolve = done; }), resolve: () => resolve() };
}

export async function runSessionConfigBusyRevocationJourney(context) {
  const entered = latch();
  const release = latch();
  let agentB;
  const scenario = await createNativeSemanticsScenario(context, 'config-busy', [
    { output: [callTime('config_busy_time')], usage, gate: release, check: body => {
      assert.ok(body.tools.some(tool => tool.name === 'time'));
      assert.equal(configurations(body).length, 0);
      entered.resolve();
    } },
    { output: [answer('busy-revoked')], usage, check: body => {
      currentConfiguration(body, agentB, 1);
      const denied = toolResult(body, 'config_busy_time');
      assert.equal(denied.error.code, 'tool_revoked');
      assert.equal(denied.unix_ms, undefined, 'An old model decision cannot execute a newly revoked clock');
    } },
  ]);
  scenario.state.count = measure;
  await patch(scenario, { instructions: initial, tools: ['time'] });
  const pending = scenario.invoke({ input: 'SESSION-CONFIG-BUSY-TIME', max_output_tokens: 64 }, 200, 'session-config-busy');
  try {
    await Promise.race([entered.promise,
      pending.then(() => { throw Error('Execution completed before the busy provider boundary'); }),
      new Promise((_, reject) => setTimeout(() => reject(Error('Provider did not reach busy boundary')), 10000).unref()),
    ]);
    const sessions = await scenario.request('GET', `/v1/agents/${scenario.agent.id}/sessions`, undefined, 200, scenario.principal.apiKey);
    assert.equal(sessions.data.length, 1);
    const id = sessions.data[0].id;
    const running = await session(scenario, id);
    assert.ok(running.run_id);
    agentB = await patch(scenario, { instructions: changed, tools: [] });
    const queued = await session(scenario, id);
    assert.equal(queued.last_input_at_ms, running.last_input_at_ms);
    assert.equal(queued.input_revision, running.input_revision);
    assert.equal(queued.run_id, running.run_id, 'A configuration edit does not admit a second execution');
    release.resolve();
    const result = await pending;
    assert.equal(result.value.status, 'completed');
    assert.equal(result.headers.get('x-agents-session'), id);
    const done = await session(scenario, id);
    assert.equal(done.configuration.agent_revision, agentB.revision);
    assert.deepEqual(done.configuration.tools, []);
    const effects = done.messages.filter(message => message.role === 'tool').map(message => JSON.parse(message.content));
    assert.equal(effects.length, 1);
    assert.equal(effects[0].error.code, 'tool_revoked');
    assert.equal(effects[0].unix_ms, undefined);
    retainsPrefix(scenario.state.requests[0], scenario.state.requests[1]);
    assert.equal(scenario.state.calls, 2);
    return { provider_calls: 2, actual_clock_effects: 0, old_decision_denied_at_safe_boundary: true,
      management_edit_does_not_steer_or_extend_user_clock: true, same_run_and_session: true,
      model_ledger_rows: await modelRows(scenario, 2), external_network_calls: 0 };
  } finally { release.resolve(); await pending.catch(() => {}); }
}

export async function runSessionConfigHistoricalForkJourney(context) {
  let agentB;
  const scenario = await createNativeSemanticsScenario(context, 'config-history', [
    { output: [answer('history-source')], usage },
    { output: [answer('history-head')], usage, check: body => currentConfiguration(body, agentB, 1) },
    { output: [answer('history-fork')], usage, check: body => {
      currentConfiguration(body, agentB, 1);
      const input = JSON.stringify(body.input);
      assert.ok(!input.includes('SESSION-CONFIG-LATER-HEAD-USER') && !input.includes('config-answer-history-head'),
        'A historical branch cannot borrow the live Session head\'s later messages or output');
    } },
  ]);
  scenario.state.count = measure;
  await patch(scenario, { instructions: initial, tools: [] });
  const source = await scenario.invoke({ input: 'SESSION-CONFIG-HISTORICAL-SOURCE', max_output_tokens: 64 }, 200, 'session-config-history-source');
  assert.equal(source.value.status, 'completed');
  const sourceId = source.headers.get('x-agents-session');
  const original = await scenario.read(source.value.id);
  const originalItems = (await scenario.client.responses.inputItems.list(source.value.id)).data;
  agentB = await patch(scenario, { instructions: changed, tools: ['time'] });
  const head = await scenario.invoke({ previous_response_id: source.value.id, input: 'SESSION-CONFIG-LATER-HEAD-USER',
    max_output_tokens: 64 }, 200, 'session-config-history-head');
  assert.equal(head.value.status, 'completed');
  assert.equal(head.headers.get('x-agents-session'), sourceId);
  const headValue = await scenario.read(head.value.id);
  const body = { previous_response_id: source.value.id, input: 'SESSION-CONFIG-NEW-FORK-WORK', max_output_tokens: 64 };
  const counted = await preview(scenario, sourceId, body);
  const fork = await scenario.invoke(body, 200, 'session-config-history-fork');
  assert.equal(fork.value.status, 'completed');
  assert.notEqual(fork.headers.get('x-agents-session'), sourceId);
  retainsPrefix(scenario.state.requests[0], scenario.state.requests[2]);
  matchesPreview(counted, scenario.state.requests[2]);
  assert.deepEqual(await scenario.read(source.value.id), original);
  assert.deepEqual((await scenario.client.responses.inputItems.list(source.value.id)).data, originalItems);
  assert.deepEqual(await scenario.read(head.value.id), headValue);
  const branch = await session(scenario, fork.headers.get('x-agents-session'));
  assert.equal(branch.configuration.agent_revision, agentB.revision);
  assert.ok(!JSON.stringify(originalItems).includes(changed));
  assert.equal(scenario.state.calls, 3);
  return { provider_calls: 3, historical_response_and_input_immutable: true,
    current_configuration_appended_only_for_new_branch_work: true, no_later_head_history_leak: true,
    branch_count_preview_equals_generation: true, independent_branch_session: true,
    model_ledger_rows: await modelRows(scenario, 3), external_network_calls: 0 };
}

export async function runSessionConfigIssuedRecoveryJourney(context) {
  const db = await context.configureIssued(true);
  try {
    const scenario = await createNativeSemanticsScenario({ ...context, db }, 'config-issued', []);
    await patch(scenario, { instructions: initial, tools: ['bash'], workspace: true });
    const record = await scenario.request('POST', `/v1/agents/${scenario.agent.id}/sessions`, {}, 201, scenario.principal.apiKey);
    const namespace = await context.mf.getDurableObjectNamespace('SESSIONS');
    const actor = namespace.get(namespace.idFromName(`${scenario.principal.owner}:${record.id}`));
    const runId = 'run_configuration_issued_fixture';
    const callId = 'configuration_issued_bash';
    const execution = `${runId.length}:${runId}:1:${callId.length}:${callId}`;
    const boot = 'configuration-issued-original-boot';
    const response = await actor.fetch('https://fixture/__test/configuration-issued-seed', {
      method: 'POST', headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ run_id: runId, call_id: callId, execution_id: execution, boot }),
    });
    assert.equal(response.status, 200);
    const seeded = await response.json();
    await patch(scenario, { instructions: changed, tools: [] });
    await context.restart();
    let observed;
    const deadline = Date.now() + 12000;
    do {
      observed = await session(scenario, record.id);
      if (observed.last_error === 'tool_outcome_unknown') break;
      await new Promise(resolve => setTimeout(resolve, 20));
    } while (Date.now() < deadline);
    assert.equal(observed.last_error, 'tool_outcome_unknown', JSON.stringify(observed));
    assert.equal(observed.configuration ?? null, null,
      'The new revocation cannot overwrite authority needed to reconcile the original issued operation');
    const effects = observed.messages.filter(message => message.role === 'tool');
    assert.equal(effects.length, 1);
    assert.equal(effects[0].tool_call_id, callId);
    assert.equal(JSON.parse(effects[0].content).error.code, 'tool_outcome_unknown');
    assert.ok(!JSON.stringify(observed.messages).includes('tool_revoked'));
    const currentNamespace = await context.mf.getDurableObjectNamespace('SESSIONS');
    const currentActor = currentNamespace.get(currentNamespace.idFromName(`${scenario.principal.owner}:${record.id}`));
    const inspect = await (await currentActor.fetch('https://fixture/__test/configuration-issued-inspect')).json();
    assert.equal(inspect.boot, boot);
    assert.deepEqual(inspect.fences, { [seeded.fence]: boot }, 'Recovery retains the exact original execution fence/identity');
    assert.deepEqual(inspect.attempts, [], 'No container start, restore, new command POST or original-daemon request is possible after loss');
    assert.equal(scenario.state.calls, 0, 'Reconciliation cannot buy another inference decision');
    assert.equal((await (await context.mf.getD1Database('DB')).prepare('SELECT COUNT(*) AS n FROM response_routes WHERE account_id=?')
      .bind(scenario.principal.owner).first('n')), 0, 'The fixture uses persisted issued state, not a fresh public Response POST');
    return { fixture: 'synthetic-persisted-issued-bash-with-lost-container', provider_calls: 0,
      actual_container_effect_success_claimed: false, original_execution_id: execution,
      unknown_preserved_not_revoked: true, original_boot_and_dispatch_fence_retained: true,
      new_container_and_command_dispatches: 0, model_ledger_rows: await modelRows(scenario, 0), external_network_calls: 0 };
  } finally { await context.configureIssued(false); }
}
