import assert from 'node:assert/strict';
import { createNativeSemanticsScenario, collectNativeEvents } from './native-semantics-journey.mjs';

export const mcpTool = authorization => ({ type: 'mcp', server_label: 'owned', server_url: 'https://mcp.example.com/mcp', authorization, require_approval: 'always' });
export const mcpSchema = { additionalProperties: false, properties: { value: { type: 'string', 'x-mcp-header': 'Value' } }, required: ['value'], type: 'object' };

export function createMcpTransport(native) {
  const routes = new Map();
  const state = { expectedAuth: 'MCP-ORIGINAL-SECRET', listFailure: false, list: 0, discover: 0, calls: 0, seenAuth: [] };
  return {
    state,
    register(name, actions) {
      const scenario = native.register(name, actions);
      const endpoint = `${scenario.baseUrl}/responses`;
      scenario.baseUrl = 'https://api.deepseek.com';
      scenario.model = 'deepseek-flash';
      routes.set(`Bearer ${scenario.key}`, endpoint);
      return scenario;
    },
    async handle(request) {
      const url = new URL(request.url);
      if (url.origin === 'https://api.deepseek.com') {
        const target = routes.get(request.headers.get('authorization'));
        if (!target) return undefined;
        return native.handle(new Request(target, { method: request.method, headers: request.headers, body: await request.text() }));
      }
      if (url.origin !== 'https://mcp.example.com') return undefined;
      assert.equal(url.pathname, '/mcp');
      assert.equal(request.method, 'POST');
      assert.equal(request.headers.get('authorization'), `Bearer ${state.expectedAuth}`);
      state.seenAuth.push(state.expectedAuth);
      const body = await request.json();
      assert.equal(body.jsonrpc, '2.0');
      assert.equal(request.headers.get('mcp-protocol-version'), '2026-07-28');
      assert.equal(request.headers.get('mcp-method'), body.method);
      assert.equal(body.params._meta['io.modelcontextprotocol/protocolVersion'], '2026-07-28');
      const response = result => Response.json({ jsonrpc: '2.0', id: body.id, result });
      if (body.method === 'server/discover') {
        state.discover++;
        return response({ resultType: 'complete', supportedVersions: ['2026-07-28'], capabilities: { tools: {} }, ttlMs: 60000, cacheScope: 'private' });
      }
      if (body.method === 'tools/list') {
        state.list++;
        if (state.listFailure) return new Response('Unavailable', { status: 503 });
        return response({ resultType: 'complete', tools: [{ name: 'echo', description: 'Echo one value.', inputSchema: mcpSchema }] });
      }
      assert.equal(body.method, 'tools/call');
      state.calls++;
      assert.equal(body.params.name, 'echo');
      assert.deepEqual(body.params.arguments, { value: '雪' });
      assert.equal(request.headers.get('mcp-name'), 'echo');
      assert.equal(request.headers.get('mcp-param-value'), '=?base64?6Zuq?=');
      if (state.invalidCallReply) return new Response('{truncated', { headers: { 'Content-Type': 'application/json' } });
      const content = [{ type: 'text', text: 'echo:雪' }];
      if (state.richCallResult) content.push(
        { type: 'image', mimeType: 'image/png', data: 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aOSkAAAAASUVORK5CYII=' },
        { type: 'text', text: 'USER-ONLY-MCP-CONTENT', annotations: { audience: ['user'] } },
        { type: 'resource', resource: { uri: 'memo:owned', text: 'Controlled MCP memo' } });
      const result = { resultType: 'complete', content, structuredContent: { echo: '雪' } };
      // Use the real native SSE parser, including a request-scoped progress event.
      const events = [{ jsonrpc: '2.0', method: 'notifications/progress', params: { progressToken: body.id, progress: 1 } }, { jsonrpc: '2.0', id: body.id, result }];
      return new Response(': heartbeat\n\n' + events.map(value => `event: message\ndata: ${JSON.stringify(value)}\n\n`).join(''), { headers: { 'Content-Type': 'text/event-stream' } });
    },
  };
}

function text(item) {
  return typeof item.content === 'string' ? item.content : item.content.map(part => part.text ?? '').join('');
}

async function assertCredentialCleanup(scenario, session, response) {
  assert.ok(session, 'The accepted response identifies its actual owner Session');
  const namespace = await scenario.mf.getDurableObjectNamespace('SESSIONS');
  const stub = namespace.get(namespace.idFromName(`${scenario.principal.owner}:${session}`));
  for (let attempt = 0; attempt < 60; attempt++) {
    const state = await (await stub.fetch(`https://session.test/__test/mcp-credentials?response=${response}`)).json();
    if (state.exists && state.canonical_cleared && state.cache_cleared && state.operations_cleared) return;
    await new Promise(resolve => setTimeout(resolve, 50));
  }
  assert.fail('Canonical terminal and private cache/receipt mirrors must clear MCP credentials');
}

export async function runMcpApprovalJourney(context, reference) {
  const call = { type: 'function_call', id: 'private-mcp-call', call_id: 'mcp-echo-call', name: 'assigned-at-discovery', arguments: '{"value":"雪"}', status: 'completed' };
  const end = { type: 'message', id: 'private-mcp-answer', role: 'assistant', status: 'completed', content: [{ type: 'output_text', text: 'MCP executed exactly once.', annotations: [], logprobs: [] }] };
  const scenario = await createNativeSemanticsScenario(context, 'mcp-primary', [
    { output: [call], check(body) {
      assert.deepEqual(body.tools, [reference.native_tool], 'Generation must use the exact resolved MCP schema counted by the API');
      assert.equal(text(body.input[0]), reference.prefix);
      assert.equal(text(body.input[1]), reference.input);
      assert.equal(body.input.length, 2);
      assert.ok(!JSON.stringify(body).includes('MCP-ORIGINAL-SECRET'));
      call.name = reference.native_tool.name;
    } },
    { output: [end], check(body) {
      assert.ok(!JSON.stringify(body).includes('MCP-REFRESHED-SECRET'));
      assert.ok(body.input.every(item => item.type !== 'mcp_approval_response'), 'Public consent is not forwarded to an upstream hosted harness');
      const output = body.input.find(item => item.type === 'function_call_output' && item.call_id === 'mcp-echo-call');
      assert.ok(output && Array.isArray(output.output));
      assert.ok(output.output.some(part => part.type === 'input_text' && part.text === 'echo:雪'));
    } },
  ]);
  await scenario.principal.browser('GET', '/v1/account');
  const tools = [mcpTool('MCP-ORIGINAL-SECRET')];
  const request = { model: scenario.agent.id, input: reference.input, tools, reasoning: { effort: 'none' } };
  const before = new Set((await context.db.prepare('SELECT id FROM usage_reservations WHERE account_id=?').bind(scenario.principal.owner).all()).results.map(row => row.id));
  const counted = await context.request('POST', '/v1/responses/input_tokens', request, 200, scenario.principal.apiKey);
  assert.equal(counted.input_tokens, reference.input_tokens, 'Pinned official offline reference includes the private MCP function schema');
  assert.equal(scenario.state.calls, 0);
  assert.equal(context.providers.state.calls, 0, 'Counting may discover schemas but cannot execute the remote tool');
  const created = (await context.db.prepare('SELECT id,state FROM usage_reservations WHERE account_id=?').bind(scenario.principal.owner).all()).results.filter(row => !before.has(row.id));
  assert.equal(created.length, 2, 'Discovery and tokenization own disjoint funded Worker intervals');
  assert.ok(created.every(row => row.state !== 'reserved'), 'Both observed preparation intervals must settle');
  assert.equal((await context.db.prepare('SELECT COUNT(*) AS n FROM sessions WHERE account_id=?').bind(scenario.principal.owner).first()).n, 0);

  const first = await scenario.client.responses.create(request, { headers: { 'Idempotency-Key': 'mcp-plan' } }).withResponse();
  assert.equal(first.data.status, 'completed');
  const approval = first.data.output.find(item => item.type === 'mcp_approval_request');
  assert.ok(approval);
  assert.equal(approval.server_label, 'owned');
  assert.equal(approval.name, 'echo');
  assert.equal(context.providers.state.calls, 0, 'An approval request is not permission to send its arguments');
  assert.equal(scenario.state.calls, 1);
  await assertCredentialCleanup(scenario, first.response.headers.get('x-agents-session'), first.data.id);
  const parent = await scenario.read(first.data.id);
  assert.ok(!JSON.stringify(parent).includes('MCP-ORIGINAL-SECRET'));

  const missing = await scenario.invoke({ previous_response_id: first.data.id, input: [{ type: 'mcp_approval_response', approval_request_id: approval.id, approve: true }], reasoning: { effort: 'none' } }, 400);
  assert.equal(missing.value.error.code, 'mcp_authorization_required');
  assert.equal(context.providers.state.calls, 0);
  assert.equal(scenario.state.calls, 1, 'Missing fresh credentials is rejected before another model request');

  context.providers.state.expectedAuth = 'MCP-REFRESHED-SECRET';
  const acceptedBody = { model: scenario.agent.id, previous_response_id: first.data.id, input: [{ type: 'mcp_approval_response', approval_request_id: approval.id, approve: true, id: null, reason: null }], tools: [mcpTool('MCP-REFRESHED-SECRET')], reasoning: { effort: 'none' } };
  const accepted = await scenario.client.responses.create(acceptedBody, { headers: { 'Idempotency-Key': 'mcp-approved-once' } }).withResponse();
  const approved = accepted.data;
  await assertCredentialCleanup(scenario, accepted.response.headers.get('x-agents-session'), approved.id);
  assert.equal(approved.status, 'completed');
  const executed = approved.output.find(item => item.type === 'mcp_call');
  assert.ok(executed);
  assert.equal(executed.approval_request_id, approval.id);
  assert.equal(executed.status, 'completed');
  assert.ok(executed.output.includes('echo:雪'));
  assert.equal(context.providers.state.calls, 1);
  assert.equal(scenario.state.calls, 2);
  assert.deepEqual(await scenario.read(first.data.id), parent, 'Approving a call cannot mutate the immutable source Response');
  const retry = await scenario.client.responses.create(acceptedBody, { headers: { 'Idempotency-Key': 'mcp-approved-once' } });
  assert.equal(retry.id, approved.id);
  assert.equal(context.providers.state.calls, 1);
  assert.equal(scenario.state.calls, 2, 'An idempotent retry cannot replay inference or the remote side effect');
  assert.ok(!JSON.stringify(approved).includes('MCP-REFRESHED-SECRET'));

  context.providers.state.listFailure = true;
  const failedCount = await context.request('POST', '/v1/responses/input_tokens', { ...request, tools: [mcpTool('MCP-REFRESHED-SECRET')] }, 503, scenario.principal.apiKey);
  assert.equal(failedCount.error.code, 'mcp_discovery_failed');
  assert.equal(context.providers.state.calls, 1);
  assert.equal(scenario.state.calls, 2);
  return { model_calls: 2, remote_tool_calls: 1, approval_requires_fresh_credentials: true, count_before_model: counted.input_tokens,
    count_schema_reference: reference.reference_sha256, failed_discovery_is_not_zero_schema_count: true,
    idempotent_side_effect: true, parent_immutable: true, private_terminal_credentials_cleared: true };
}
export async function runMcpDeferredJourney(context) {
  const state = context.providers.state;
  state.listFailure = false;
  state.expectedAuth = 'MCP-DEFERRED-SECRET';
  state.richCallResult = true;
  const before = state.calls;
  const search = '__agents_tool_search';
  const invoke = '__agents_invoke_discovered';
  const call = (id, name, args) => ({ type: 'function_call', id: `private-${id}`, call_id: id, name, arguments: JSON.stringify(args), status: 'completed' });
  const final = { type: 'message', id: 'private-deferred-final', role: 'assistant', status: 'completed', content: [{ type: 'output_text', text: 'Deferred MCP completed.', annotations: [] }] };
  const scenario = await createNativeSemanticsScenario(context, 'mcp-deferred', [
    { output: [call('mcp-search', search, { paths: ['owned'] })], check(body) {
      assert.ok(body.tools.some(tool => tool.name === search));
      assert.ok(body.tools.some(tool => tool.name === invoke));
      assert.ok(!JSON.stringify(body.tools).includes('"required":["value"]'));
      assert.ok(!JSON.stringify(body).includes(state.expectedAuth));
    } },
    { output: [call('mcp-deferred-call', invoke, { tool: 'owned.echo', arguments: { value: '雪' } })], check(body) {
      const output = body.input.find(item => item.type === 'function_call_output' && item.call_id === 'mcp-search');
      assert.ok(output);
      assert.ok(JSON.stringify(output.output).includes('allowed_callers'));
      assert.ok(JSON.stringify(output.output).includes('parameters'));
      assert.ok(!JSON.stringify(body).includes(state.expectedAuth));
    } },
    { output: [final], check(body) {
      assert.ok(body.input.some(item => item.type === 'function_call_output' && item.call_id === 'mcp-deferred-call' && JSON.stringify(item.output).includes('echo:雪')));
      assert.ok(!body.input.some(item => item.type === 'mcp_approval_response'));
      const parts = body.input.find(item => item.type === 'function_call_output' && item.call_id === 'mcp-deferred-call').output;
      assert.ok(parts.some(part => part.type === 'input_image' && part.image_url.startsWith('data:image/png;base64,')));
      assert.ok(parts.filter(part => part.type === 'input_text').every(part => !part.text.includes('iVBOR') && !part.text.includes('USER-ONLY-MCP-CONTENT')));
    } },
  ]);
  await scenario.principal.browser('GET', '/v1/account');
  const tools = [{ ...mcpTool(state.expectedAuth), defer_loading: true, server_description: 'Echo tools' }, { type: 'tool_search' }];
  const stream = await context.mf.dispatchFetch('https://agents.test/v1/responses', { method: 'POST',
    headers: { Authorization: `Bearer ${scenario.principal.apiKey}`, 'Content-Type': 'application/json' },
    body: JSON.stringify({ model: scenario.agent.id, input: 'Discover and use owned echo.', tools, stream: true, reasoning: { effort: 'none' } }) });
  assert.equal(stream.status, 200, stream.status === 200 ? '' : await stream.text());
  const events = await collectNativeEvents(stream);
  const pending = events.findLast(event => event.type === 'response.completed')?.response;
  assert.ok(pending, 'Deferred discovery must finish with a canonical streamed handoff');
  const wire = context.validateWire(pending, events);
  assert.equal(pending.status, 'completed');
  assert.deepEqual(pending.output.map(item => item.type), ['mcp_list_tools', 'tool_search_call', 'tool_search_output', 'mcp_approval_request']);
  assert.equal(state.calls, before, 'Loading schemas and requesting approval cannot execute MCP');
  const approval = pending.output.at(-1);
  const approved = await scenario.client.responses.create({ model: scenario.agent.id, previous_response_id: pending.id,
    tools, input: [{ type: 'mcp_approval_response', approval_request_id: approval.id, approve: true }], reasoning: { effort: 'none' } },
  { headers: { 'Idempotency-Key': 'deferred-mcp-once' } });
  assert.equal(approved.status, 'completed');
  assert.equal(approved.output.find(item => item.type === 'mcp_call')?.name, 'echo');
  assert.equal(state.calls, before + 1);
  assert.equal(scenario.state.calls, 3);
  assert.deepEqual(await scenario.read(approved.id), approved);
  context.validateWire(approved, []);
  assert.ok(!JSON.stringify(approved).includes('__agents_'));
  assert.ok(!JSON.stringify(approved).includes(state.expectedAuth));
  state.richCallResult = false;
  return { strict_wire: wire, typed_image_native_input: true, user_only_content_filtered: true, model_calls: 3, remote_tool_calls: 1, actual_deferred_schema_loaded: true, qualified_permissions_loaded: true, raw_get_matches_sdk: true };
}

export async function runMcpDeniedAndUnknown(context) {
  const state = context.providers.state;
  state.listFailure = false;
  state.expectedAuth = 'MCP-NEGATIVE-SECRET';
  const before = state.calls;
  const action = { type: 'function_call', id: 'private-negative-call', call_id: 'mcp-negative-call', name: 'assigned', arguments: '{"value":"雪"}', status: 'completed' };
  const final = { type: 'message', id: 'private-negative-final', role: 'assistant', status: 'completed', content: [{ type: 'output_text', text: 'MCP failure handled without retry.', annotations: [] }] };
  const denial = await createNativeSemanticsScenario(context, 'mcp-denial', [
    { output: [action], check(body) { action.name = body.tools.find(tool => tool.name?.startsWith('__agents_mcp_')).name; } },
    { output: [final], check(body) { assert.ok(body.input.some(item => item.type === 'function_call_output' && JSON.stringify(item.output).includes('mcp_approval_denied'))); } },
  ]);
  await denial.principal.browser('GET', '/v1/account');
  const pending = (await denial.invoke({ input: 'Plan echo.', tools: [mcpTool(state.expectedAuth)], reasoning: { effort: 'none' } })).value;
  const approval = pending.output.find(item => item.type === 'mcp_approval_request');
  assert.ok(approval);
  const denied = (await denial.invoke({ previous_response_id: pending.id,
    input: [{ type: 'mcp_approval_response', approval_request_id: approval.id, approve: false }], reasoning: { effort: 'none' } })).value;
  assert.equal(denied.status, 'completed');
  context.validateWire(denied, []);
  assert.equal(denied.output.find(item => item.type === 'mcp_call')?.status, 'failed');
  assert.equal(state.calls, before, 'Deny never obtains permission for a remote call');
  assert.equal(denial.state.calls, 2);

  state.invalidCallReply = true;
  const unknownCall = { ...action, name: 'assigned' };
  const unknown = await createNativeSemanticsScenario(context, 'mcp-unknown', [
    { output: [unknownCall], check(body) { unknownCall.name = body.tools.find(tool => tool.name?.startsWith('__agents_mcp_')).name; } },
    { output: [final], check(body) { assert.ok(body.input.some(item => item.type === 'function_call_output' && JSON.stringify(item.output).includes('outcome_unknown'))); } },
  ]);
  await unknown.principal.browser('GET', '/v1/account');
  const body = { input: 'Execute without approval only because this owner explicitly opts out.', tools: [{ ...mcpTool(state.expectedAuth), require_approval: 'never' }], reasoning: { effort: 'none' } };
  const result = (await unknown.invoke(body, 200, 'unknown-mcp-once')).value;
  assert.equal(result.status, 'completed');
  context.validateWire(result, []);
  assert.equal(result.output.find(item => item.type === 'mcp_call')?.status, 'failed');
  assert.equal(state.calls, before + 1);
  const replay = (await unknown.invoke(body, 200, 'unknown-mcp-once')).value;
  assert.equal(replay.id, result.id);
  assert.equal(state.calls, before + 1, 'Unknown HTTP outcomes never grant permission to replay a side effect');
  assert.equal(unknown.state.calls, 2);
  state.invalidCallReply = false;
  return { model_calls: 4, denied_remote_calls: 0, unknown_remote_calls: 1, unknown_http_not_replayed: true };
}
export async function runMcpApprovalReuse(context) {
  const state = context.providers.state;
  state.listFailure = false;
  state.invalidCallReply = false;
  state.expectedAuth = 'MCP-CLAIM-SECRET';
  const before = state.calls;
  const action = { type: 'function_call', id: 'private-claim-call', call_id: 'mcp-claim-call', name: 'assigned', arguments: '{"value":"雪"}', status: 'completed' };
  const final = { type: 'message', id: 'private-claim-final', role: 'assistant', status: 'completed', content: [{ type: 'output_text', text: 'The approved effect has one destination.', annotations: [] }] };
  const scenario = await createNativeSemanticsScenario(context, 'mcp-claim', [
    { output: [action], check(body) { action.name = body.tools.find(tool => tool.name?.startsWith('__agents_mcp_')).name; } },
    { output: [final] },
  ]);
  await scenario.principal.browser('GET', '/v1/account');
  const tools = [{ ...mcpTool(state.expectedAuth), allowed_callers: ['direct', 'programmatic'] }];
  const pending = (await scenario.invoke({ input: 'Plan one owned effect.', tools, reasoning: { effort: 'none' } })).value;
  const approval = pending.output.find(item => item.type === 'mcp_approval_request');
  assert.ok(approval);
  const body = { previous_response_id: pending.id, tools, input: [{ type: 'mcp_approval_response', approval_request_id: approval.id, approve: true }], reasoning: { effort: 'none' } };
  const accepted = (await scenario.invoke(body, 200, 'claim-first')).value;
  assert.equal(accepted.status, 'completed');
  assert.equal(state.calls, before + 1);
  const second = (await scenario.invoke(body, 409, 'claim-second')).value;
  assert.equal(second.error.code, 'mcp_approval_consumed');
  assert.equal(state.calls, before + 1, 'A second Response cannot consume the same approval');
  assert.equal(scenario.state.calls, 2, 'A rejected second destination cannot buy another model action');
  assert.deepEqual(await scenario.read(pending.id), pending, 'Consent consumption cannot mutate the public parent');
  context.validateWire(accepted, []);
  return { model_calls: 2, remote_tool_calls: 1, consumed_approval_rejected: true, parent_immutable: true, mixed_caller_direct_supported: true };
}
