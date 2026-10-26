import assert from 'node:assert/strict';
import OpenAI from 'openai';
import { createBrowserPrincipal } from './browser-principal.mjs';
import { collectNativeEvents, createNativeSemanticsScenario } from './native-semantics-journey.mjs';

const program = code => ({ type: 'function_call', id: 'private-program-item', call_id: 'private-program-call',
  name: '__agents_program', arguments: JSON.stringify({ code }), status: 'completed' });
const answer = { type: 'message', id: 'private-answer', role: 'assistant', status: 'completed',
  content: [{ type: 'output_text', text: 'Program finished.', annotations: [], logprobs: [] }] };

// Both operations use the actual SDK, Rust actor, native Loader and shared
// wallet. Only model transport is controlled; no external service is called.
export async function runProgrammaticTime(context) {
  const principal = await createBrowserPrincipal({ ...context, owner: 'programmatic-time-owner' });
  await principal.browser('GET', '/v1/account');
  const state = context.providers.register('programmatic-time', [
    { output: [program('const a = await tools.time({}); const b = await tools.time({}); text(JSON.stringify({a,b}));')], check(body) {
      assert.equal(body.tools.filter(tool => tool.name === '__agents_program').length, 1);
      assert.ok(!body.tools.some(tool => tool.type === 'programmatic_tool_calling'), 'Our runtime, not an upstream hosted interpreter');
    } },
    { output: [answer], check(body) {
      assert.equal(body.input.filter(item => item.type === 'function_call_output' && item.call_id === 'private-program-call').length, 1);
    } },
    { output: [program('await tools.time({forbidden:true}); text("unreachable");')] },
    { output: [answer] },
  ]);
  const credential = await principal.browser('POST', '/v1/provider-keys', {
    protocol: 'openai', base_url: state.baseUrl, api_key: state.key,
  }, 201);
  const agent = await context.request('POST', '/v1/agents', { name: 'Programmatic time',
    instructions: 'Use the supplied program interface.', workspace: false, tools: ['time'], compaction_percent: 80,
    model: { provider: 'byok', credential_id: credential.id, protocol: 'responses', model: state.model,
      context_window: 32768, max_output_tokens: 512 } }, 201, principal.apiKey);
  const client = new OpenAI({ apiKey: principal.apiKey, baseURL: 'https://agents.test/v1', maxRetries: 0,
    timeout: 20000, fetch: (input, init) => context.mf.dispatchFetch(input, init) });
  const body = { model: agent.id, input: 'Run two time calls in one program.',
    tools: [{ type: 'programmatic_tool_calling' }], max_tool_calls: 4 };
  async function invoke(input, key) {
    const wire = await client.responses.create({ ...input, stream: true }, {
      headers: { 'Idempotency-Key': key },
    }).asResponse();
    assert.equal(wire.status, 200, wire.status === 200 ? '' : await wire.text());
    const events = await collectNativeEvents(wire);
    if (state.failure) throw state.failure;
    assert.ok(!JSON.stringify(events).includes('__agents_'), 'Private aliases are not public protocol');
    const terminal = events.findLast(event => ['response.completed', 'response.failed'].includes(event.type));
    assert.ok(terminal, 'Require a genuine canonical terminal');
    const retained = await (await client.responses.retrieve(terminal.response.id).asResponse()).json();
    assert.deepEqual(retained, terminal.response, 'Raw GET must equal raw SSE; SDK convenience fields are not wire');
    return { value: terminal.response, events };
  }
  const first = await invoke(body, 'programmatic-time-first');
  assert.equal(first.value.status, 'completed');
  const instruction = first.value.output.find(item => item.type === 'program');
  const output = first.value.output.find(item => item.type === 'program_output');
  assert.ok(instruction?.fingerprint);
  assert.equal(output.call_id, instruction.call_id);
  assert.equal(output.status, 'completed');
  const nested = first.value.output.filter(item => item.type === 'function_call');
  const nestedResults = first.value.output.filter(item => item.type === 'function_call_output');
  assert.equal(nested.length, 2);
  assert.equal(nestedResults.length, 2);
  for (const item of [...nested, ...nestedResults]) {
    assert.deepEqual(item.caller, { type: 'program', caller_id: instruction.call_id });
  }
  context.validateWire(first.value, first.events);
  const clocks = JSON.parse(output.result);
  assert.equal(typeof clocks.a.utc, 'string');
  assert.equal(typeof clocks.b.utc, 'string');
  assert.ok(Number.isFinite(clocks.a.unix_ms) && clocks.b.unix_ms >= clocks.a.unix_ms);
  const before = await charges(context.db, principal.owner);
  const retried = await invoke(body, 'programmatic-time-first');
  assert.equal(retried.value.id, first.value.id);
  assert.deepEqual(await charges(context.db, principal.owner), before, 'Same-key replay never creates another isolate, CPU slot or request charge');
  assert.equal(state.calls, 2, 'No inference replay or hidden repair');
  assert.deepEqual(before.map(row => [row.unit, Number(row.quantity), Number(row.amount_microusd)]).sort(), [
    ['dynamic_worker_cpu_slot_100ms', 1, 2], ['dynamic_worker_creation_day', 1, 2000], ['dynamic_worker_request', 1, 1],
  ].sort(), 'One accepted program uses one source request and one total configured CPU allocation across both awaits');
  const denied = await invoke({ ...body, input: 'Exercise invalid nested arguments.' }, 'programmatic-invalid-time');
  const failedProgram = denied.value.output.find(item => item.type === 'program_output');
  assert.equal(failedProgram.status, 'incomplete');
  assert.match(failedProgram.result, /programmatic_argument_schema_violation/);
  context.validateWire(denied.value, denied.events);
  assert.equal(state.calls, 4, 'Invalid arguments do not trigger paid repair');
  const resources = await context.db.prepare("SELECT count(*) n FROM usage_ledger WHERE account_id=? AND resource='container'").bind(principal.owner).first();
  assert.equal(resources.n, 0, 'A program never gains workspace or Bash authority from pinned time');
  return { provider_calls: 4, origin_fetches: 2, successful_time_calls: 2, denied_time_side_effects: 0,
    idempotent_replay: true, default_promise_protection: true };
}

export async function runProgrammaticMcp(context) {
  const remote = context.providers.state;
  const before = remote.calls;
  const scenario = await createNativeSemanticsScenario(context, 'programmatic-mcp', [
    { output: [program('const a = await tools.owned.echo({value:"雪"}); const b = await tools.owned.echo({value:a.echo}); text(JSON.stringify({a,b}));')], check(body) {
      const declaration = body.tools.find(tool => tool.name === '__agents_program');
      assert.ok(declaration?.description.includes('echo'));
      assert.ok(!body.tools.some(tool => tool.name?.startsWith('__agents_mcp_')), 'Program-only MCP is not directly callable');
      assert.ok(!JSON.stringify(body).includes(remote.expectedAuth));
    } },
    { output: [answer] },
  ]);
  await scenario.principal.browser('GET', '/v1/account');
  const body = { model: scenario.agent.id, input: 'Call owned echo twice from one program.', max_tool_calls: 2,
    tools: [{ type: 'programmatic_tool_calling' }, { type: 'mcp', server_label: 'owned', server_url: 'https://mcp.example.com/mcp',
      authorization: remote.expectedAuth, require_approval: 'never', allowed_callers: ['programmatic'] }] };
  const wire = await scenario.client.responses.create({ ...body, stream: true }, { headers: { 'Idempotency-Key': 'programmatic-mcp-once' } }).asResponse();
  assert.equal(wire.status, 200, wire.status === 200 ? '' : await wire.text());
  const events = await collectNativeEvents(wire);
  if (scenario.state.failure) throw scenario.state.failure;
  const value = events.findLast(event => event.type === 'response.completed')?.response;
  assert.ok(value);
  const output = value.output.find(item => item.type === 'program_output');
  assert.equal(output?.status, 'completed');
  assert.deepEqual(JSON.parse(output.result), { a: { echo: '雪' }, b: { echo: '雪' } });
  assert.equal(remote.calls, before + 2);
  assert.equal(scenario.state.calls, 2);
  context.validateWire(value, events);
  assert.deepEqual(await (await scenario.client.responses.retrieve(value.id).asResponse()).json(), value);
  const replayEvents = await collectNativeEvents(await scenario.client.responses.create({ ...body, stream: true }, { headers: { 'Idempotency-Key': 'programmatic-mcp-once' } }).asResponse());
  const replay = replayEvents.findLast(event => event.type === 'response.completed')?.response;
  assert.ok(replay);
  assert.equal(replay.id, value.id);
  assert.equal(remote.calls, before + 2, 'An accepted program replay never repeats either MCP effect');
  assert.equal(scenario.state.calls, 2);
  assert.deepEqual((await charges(context.db, scenario.principal.owner)).map(row => [row.unit, Number(row.quantity), Number(row.amount_microusd)]).sort(), [
    ['dynamic_worker_cpu_slot_100ms', 1, 2], ['dynamic_worker_creation_day', 1, 2000], ['dynamic_worker_request', 1, 1],
  ].sort());
  return { model_calls: 2, actual_mcp_calls: 2, same_origin_fetches: 1, program_only_caller: true, structured_result_join: true, idempotent_effects: true };
}

export async function runProgrammaticAdmissionFailures(context) {
  const scenario = await createNativeSemanticsScenario(context, 'programmatic-unavailable', []);
  const failures = [];
  for (const missing of ['loader', 'price']) {
    await context.configureProgramCapability(missing);
    const response = await scenario.client.responses.create({ model: scenario.agent.id, input: 'Do not spend inference on unavailable execution.',
      tools: [{ type: 'programmatic_tool_calling' }] }).asResponse();
    assert.equal(response.status, 422);
    assert.equal((await response.json()).error.code, 'programmatic_unavailable');
    assert.equal(scenario.state.calls, 0, 'Missing execution authority or price is rejected before any inference');
    await context.request('GET', '/v1/agents', undefined, 200, scenario.principal.apiKey);
    failures.push(missing);
  }
  assert.deepEqual(await charges(context.db, scenario.principal.owner), []);
  return { disabled_only_programmatic: failures, ordinary_api_available: true, model_calls: 0, dynamic_charges: 0 };
}

async function charges(db, owner) {
  return (await db.prepare("SELECT unit, CAST(quantity AS TEXT) quantity, CAST(amount_microusd AS TEXT) amount_microusd FROM usage_ledger WHERE account_id=? AND unit LIKE 'dynamic_worker_%' ORDER BY unit").bind(owner).all()).results;
}
