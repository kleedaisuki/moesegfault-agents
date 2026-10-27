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
  assert.equal(output.status, 'completed', output.result);
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
  const resources = await context.db.prepare("SELECT count(*) n FROM usage_ledger l JOIN usage_reservations r ON r.id=l.reservation_id WHERE r.account_id=? AND l.resource='container'").bind(principal.owner).first();
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
  assert.equal(output?.status, 'completed', output?.result);
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
    await assert.rejects(scenario.client.responses.create({ model: scenario.agent.id, input: 'Do not spend inference on unavailable execution.',
      tools: [{ type: 'programmatic_tool_calling' }] }).asResponse(), error => error.status === 422 && error.code === 'programmatic_unavailable');
    assert.equal(scenario.state.calls, 0, 'Missing execution authority or price is rejected before any inference');
    await context.request('GET', '/v1/agents', undefined, 200, scenario.principal.apiKey);
    failures.push(missing);
  }
  assert.deepEqual(await charges(context.db, scenario.principal.owner), []);
  return { disabled_only_programmatic: failures, ordinary_api_available: true, model_calls: 0, dynamic_charges: 0 };
}

async function charges(db, owner) {
  return (await db.prepare("SELECT l.unit, CAST(l.quantity AS TEXT) quantity, CAST(l.amount_microusd AS TEXT) amount_microusd FROM usage_ledger l JOIN usage_reservations r ON r.id=l.reservation_id WHERE r.account_id=? AND l.unit LIKE 'dynamic_worker_%' ORDER BY l.unit").bind(owner).all()).results;
}

export async function runProgrammaticClient(context) {
  const source = 'const nonce = Math.random(); const x = await tools.client({value:7,nonce}); const y = await tools.decorate("value="+x.value); text(JSON.stringify({nonce,x,y}));';
  const scenario = await createNativeSemanticsScenario(context, 'programmatic-client', [
    { output: [program(source)] },
    { output: [answer], check(body) {
      const outputs = body.input.filter(item => item.type === 'function_call_output');
      assert.equal(outputs.length, 1, 'Nested caller outputs enter JavaScript, not unmatched provider call IDs');
      assert.equal(outputs[0].call_id, 'private-program-call');
    } },
  ]);
  await scenario.principal.browser('GET', '/v1/account');
  const tools = [{ type: 'programmatic_tool_calling' },
    { type: 'function', name: 'client', allowed_callers: ['programmatic'], strict: true,
      parameters: { type: 'object', properties: { value: { type: 'integer' }, nonce: { type: 'number' } }, required: ['value', 'nonce'], additionalProperties: false },
      output_schema: { type: 'object', properties: { value: { type: 'integer' } }, required: ['value'], additionalProperties: false } },
    { type: 'custom', name: 'decorate', allowed_callers: ['programmatic'], format: { type: 'text' } }];
  const body = { model: scenario.agent.id, tools, input: 'Run one program through function and custom handoffs.', max_tool_calls: 4 };
  async function invoke(request, key) {
    const wire = await scenario.client.responses.create({ ...request, stream: true }, { headers: { 'Idempotency-Key': key } }).asResponse();
    const events = await collectNativeEvents(wire);
    if (scenario.state.failure) throw scenario.state.failure;
    const value = events.findLast(event => event.type === 'response.completed')?.response;
    assert.ok(value, JSON.stringify(events.findLast(event => event.type === 'response.failed')?.response?.error));
    assert.deepEqual(await (await scenario.client.responses.retrieve(value.id).asResponse()).json(), value);
    context.validateWire(value, events);
    return value;
  }
  const first = await invoke(body, 'program-client-first');
  const parent = first.output.find(item => item.type === 'program');
  const call = first.output.find(item => item.type === 'function_call');
  assert.ok(parent && call);
  assert.deepEqual(call.caller, { type: 'program', caller_id: parent.call_id });
  const nonce = JSON.parse(call.arguments).nonce;
  assert.equal(scenario.state.calls, 1);
  assert.equal(first.usage.total_tokens, 150);
  const runRows = () => context.db.prepare("SELECT id,state FROM usage_reservations WHERE account_id=? AND idempotency_key LIKE 'run:%'").bind(scenario.principal.owner).all();
  assert.deepEqual((await runRows()).results.map(row => [row.id,row.state]), [[first.id,'reserved']], 'The funded execution remains alive across a completed public handoff');
  const returned = { type: 'function_call_output', call_id: call.call_id, caller: call.caller, output: '{"value":7}' };
  await assert.rejects(scenario.client.responses.create({ ...body, previous_response_id: first.id, input: [{ ...returned, caller: { type: 'program', caller_id: 'forged' } }] }).asResponse(), error => error.status === 400);
  const secondBody = { ...body, previous_response_id: first.id, input: [returned] };
  const second = await invoke(secondBody, 'program-client-function');
  const custom = second.output.find(item => item.type === 'custom_tool_call');
  assert.equal(custom?.input, 'value=7');
  assert.deepEqual(custom.caller, call.caller);
  assert.equal(second.usage?.total_tokens ?? 0, 0);
  assert.equal(scenario.state.calls, 1, 'A local result resumes the live program without another model decision');
  assert.equal((await runRows()).results.length, 1);
  await assert.rejects(scenario.client.responses.create({ ...secondBody, input: [{ ...returned, output: '{"value":8}' }] }).asResponse(), error => error.status === 409);
  assert.deepEqual(await invoke(secondBody, 'program-client-function'), second, 'Same-key mailbox input is observed, not applied twice');
  const last = await invoke({ ...body, previous_response_id: second.id,
    input: [{ type: 'custom_tool_call_output', call_id: custom.call_id, caller: custom.caller, output: 'decorated' }] }, 'program-client-custom');
  const output = last.output.find(item => item.type === 'program_output');
  assert.equal(output?.status, 'completed', output?.result);
  assert.equal(output.call_id, parent.call_id);
  assert.deepEqual(JSON.parse(output.result), { nonce, x: { value: 7 }, y: 'decorated' });
  assert.equal(last.usage.total_tokens, 150, 'The child reports only its own model usage');
  assert.equal(scenario.state.calls, 2);
  assert.deepEqual(await (await scenario.client.responses.retrieve(first.id).asResponse()).json(), first, 'Later usage and output cannot rewrite the parent');
  assert.deepEqual((await runRows()).results.map(row => [row.id,row.state]), [[first.id,'settled']]);
  assert.deepEqual((await charges(context.db, scenario.principal.owner)).map(row => [row.unit,Number(row.quantity),Number(row.amount_microusd)]).sort(), [
    ['dynamic_worker_cpu_slot_100ms',1,2], ['dynamic_worker_creation_day',1,2000], ['dynamic_worker_request',1,1],
  ].sort(), 'Three public Responses retain exactly one originating program fetch and CPU allocation');
  return { public_responses: 3, model_calls: 2, client_function_calls: 1, client_custom_calls: 1, native_program_fetches: 1,
    native_heap_nonce_preserved: true, one_funded_execution: true, immutable_parent_usage: true, replayed_mailbox_not_reapplied: true };
}

export async function runProgrammaticApproval(context) {
  const remote=context.providers.state;
  const before=remote.calls;
  const scenario=await createNativeSemanticsScenario(context,'programmatic-approval',[
    {output:[program('const a=await tools.owned.echo({value:"雪"}); const b=await tools.owned.echo({value:a.echo}); text(JSON.stringify({a,b}));')]},
    {output:[answer]},
  ]);
  await scenario.principal.browser('GET','/v1/account');
  const declaration=authorization=>({type:'mcp',server_label:'owned',server_url:'https://mcp.example.com/mcp',authorization,require_approval:'always',allowed_callers:['programmatic']});
  const body={model:scenario.agent.id,input:'Approve the first remote call and decline the second within the same live program.',tools:[{type:'programmatic_tool_calling'},declaration(remote.expectedAuth)],max_tool_calls:4};
  async function invoke(input,key) {
    const events=await collectNativeEvents(await scenario.client.responses.create({...input,stream:true},{headers:{'Idempotency-Key':key}}).asResponse());
    if(scenario.state.failure)throw scenario.state.failure;
    const value=events.findLast(event=>event.type==='response.completed')?.response;
    assert.ok(value,JSON.stringify(events.findLast(event=>event.type==='response.failed')?.response?.error));
    context.validateWire(value,events);
    assert.deepEqual(await(await scenario.client.responses.retrieve(value.id).asResponse()).json(),value);
    return value;
  }
  const first=await invoke(body,'program-approval-first');
  const approval=first.output.find(item=>item.type==='mcp_approval_request');
  assert.ok(approval);
  assert.equal(remote.calls,before,'A program RPC proposal is not human approval');
  assert.equal(scenario.state.calls,1);
  await assert.rejects(scenario.client.responses.create({...body,previous_response_id:first.id,tools:[{type:'programmatic_tool_calling'}],input:[{type:'mcp_approval_response',approval_request_id:approval.id,approve:true}]}).asResponse(),error=>error.status===400&&error.code==='mcp_authorization_required');
  remote.expectedAuth='PROGRAM-APPROVAL-REFRESHED-SECRET';
  const approvedBody={...body,previous_response_id:first.id,tools:[{type:'programmatic_tool_calling'},declaration(remote.expectedAuth)],input:[{type:'mcp_approval_response',approval_request_id:approval.id,approve:true}]};
  const second=await invoke(approvedBody,'program-approval-accepted');
  assert.equal(remote.calls,before+1);
  const next=second.output.find(item=>item.type==='mcp_approval_request');
  assert.ok(next);
  assert.notEqual(next.id,approval.id);
  assert.equal(scenario.state.calls,1,'An approval resumes original JavaScript without another model decision');
  assert.deepEqual(await invoke(approvedBody,'program-approval-accepted'),second);
  assert.equal(remote.calls,before+1);
  const denied=await invoke({...body,previous_response_id:second.id,tools:[{type:'programmatic_tool_calling'},declaration(remote.expectedAuth)],input:[{type:'mcp_approval_response',approval_request_id:next.id,approve:false}]},'program-approval-denied');
  const output=denied.output.find(item=>item.type==='program_output');
  assert.equal(output?.status,'completed',output?.result);
  const result=JSON.parse(output.result);
  assert.deepEqual(result.a,{echo:'雪'});
  assert.equal(result.b.error.code,'mcp_approval_denied');
  assert.equal(remote.calls,before+1,'Declined program RPC cannot send HTTP');
  assert.equal(scenario.state.calls,2);
  assert.deepEqual(await(await scenario.client.responses.retrieve(first.id).asResponse()).json(),first);
  assert.deepEqual((await charges(context.db,scenario.principal.owner)).map(row=>[row.unit,Number(row.quantity),Number(row.amount_microusd)]).sort(),[
    ['dynamic_worker_cpu_slot_100ms',1,2],['dynamic_worker_creation_day',1,2000],['dynamic_worker_request',1,1],
  ].sort());
  return {public_responses:3,model_calls:2,approved_remote_calls:1,denied_remote_calls:0,fresh_credentials:true,one_use_approval:true,originating_program_fetches:1};
}

export async function runProgrammaticConversation(context) {
  const scenario = await createNativeSemanticsScenario(context, 'programmatic-conversation', [
    { output: [program('const nonce=Math.random(); const value=await tools.local({nonce}); text(JSON.stringify({nonce,value}));')] },
    { output: [answer], check(body) {
      const outputs = body.input.filter(item => item.type === 'function_call_output');
      assert.equal(outputs.length, 1);
      assert.equal(outputs[0].call_id, 'private-program-call');
      assert.equal(JSON.stringify(body.input).split('CONVERSATION-PROGRAM-ORIGINAL').length - 1, 1);
    } },
  ]);
  await scenario.principal.browser('GET', '/v1/account');
  const conversation = await scenario.client.conversations.create();
  const tools = [{ type: 'programmatic_tool_calling' }, { type: 'function', name: 'local',
    allowed_callers: ['programmatic'], strict: true,
    parameters: { type: 'object', properties: { nonce: { type: 'number' } }, required: ['nonce'], additionalProperties: false } }];
  const base = { model: scenario.agent.id, conversation: conversation.id, tools, max_tool_calls: 2 };
  async function invoke(body, key) {
    const wire = await scenario.client.responses.create({ ...body, stream: true }, {
      headers: { 'Idempotency-Key': key },
    }).asResponse();
    const events = await collectNativeEvents(wire);
    if (scenario.state.failure) throw scenario.state.failure;
    const value = events.findLast(event => event.type === 'response.completed')?.response;
    assert.ok(value, JSON.stringify(events.findLast(event => event.type === 'response.failed')?.response?.error));
    context.validateWire(value, events);
    assert.deepEqual(await (await scenario.client.responses.retrieve(value.id).asResponse()).json(), value);
    return value;
  }
  const first = await invoke({ ...base, input: 'CONVERSATION-PROGRAM-ORIGINAL' }, 'program-conversation-first');
  const call = first.output.find(item => item.type === 'function_call');
  assert.ok(call?.caller);
  const nonce = JSON.parse(call.arguments).nonce;
  const before = (await scenario.client.conversations.items.list(conversation.id, { order: 'asc' })).data;
  assert.equal(before.filter(item => item.id === call.id).length, 1);
  const body = { ...base, input: [{ type: 'function_call_output', call_id: call.call_id, caller: call.caller, output: 'local-value' }] };
  const last = await invoke(body, 'program-conversation-result');
  const output = last.output.find(item => item.type === 'program_output');
  assert.equal(output?.status, 'completed', output?.result);
  assert.deepEqual(JSON.parse(output.result), { nonce, value: 'local-value' });
  assert.equal(first.usage.total_tokens, 150);
  assert.equal(last.usage.total_tokens, 150);
  assert.equal(scenario.state.calls, 2);
  const items = (await scenario.client.conversations.items.list(conversation.id, { order: 'asc' })).data;
  assert.equal(new Set(items.map(item => item.id)).size, items.length);
  assert.equal(items.filter(item => item.id === call.id).length, 1);
  assert.equal(items.filter(item => item.id === output.id).length, 1);
  assert.equal(items.filter(item => item.type === 'function_call_output' && item.call_id === call.call_id).length, 1);
  assert.deepEqual(await invoke(body, 'program-conversation-result'), last);
  assert.equal(scenario.state.calls, 2);
  assert.deepEqual((await scenario.client.conversations.items.list(conversation.id, { order: 'asc' })).data, items);
  assert.deepEqual(await (await scenario.client.responses.retrieve(first.id).asResponse()).json(), first);
  const escrows = await context.db.prepare(`SELECT response_id,state,remaining_bytes FROM conversation_response_capacity
    WHERE account_id=? AND conversation_id=? ORDER BY response_id`).bind(scenario.principal.owner, conversation.id).all();
  assert.deepEqual(escrows.results, [first.id, last.id].sort().map(response_id => ({ response_id, state: 'converted', remaining_bytes: 0 })));
  const runs = await context.db.prepare(`SELECT id,state FROM usage_reservations WHERE account_id=? AND idempotency_key LIKE 'resources:%'`)
    .bind(scenario.principal.owner).all();
  assert.deepEqual(runs.results, [{ id: first.id, state: 'settled' }]);
  assert.deepEqual((await charges(context.db, scenario.principal.owner)).map(row => [row.unit, Number(row.quantity), Number(row.amount_microusd)]).sort(), [
    ['dynamic_worker_cpu_slot_100ms', 1, 2], ['dynamic_worker_creation_day', 1, 2000], ['dynamic_worker_request', 1, 1],
  ].sort());
  return { public_responses: 2, model_calls: 2, native_heap_nonce_preserved: true, conversation_predecessor_without_explicit_parent: true,
    converted_output_escrows: 2, immutable_published_items: true, originating_program_fetches: 1, one_funded_execution: true };
}
