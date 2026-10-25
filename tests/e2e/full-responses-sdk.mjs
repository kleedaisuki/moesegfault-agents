import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { createHash, randomUUID } from 'node:crypto';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';
import { execFileSync } from 'node:child_process';
import OpenAI from 'openai';
import { ResponsesWS } from 'openai/resources/responses/ws';

export const CORE_GROUPS = ['conversations', 'http_historical_forks', 'store_false_tombstone', 'count_compact', 'websocket_lanes'];
export const EXTERNAL_GROUPS = ['hosted_interpreter', 'hosted_image_seedream5', 'hosted_web_search', 'hosted_space_search', 'async_local_tools', 'steering', 'workspace_physical_branches', 'urls_ssrf', 'browser_native', 'accounting_closure', 'performance', 'websocket_fifo_limits', 'rich_native_items', 'cross_agent_pin'];
const terminalTypes = new Set(['response.completed', 'response.incomplete', 'response.failed']);
const digest = value => createHash('sha256').update(JSON.stringify(value)).digest('hex');

export function assertBudget(planned, allowed) {
  assert.ok(Number.isSafeInteger(allowed) && allowed >= planned && allowed <= 32, 'explicit_bounded_paid_submission_budget_required');
}

export function assertExternalEvidence(evidence, source) {
  assert.equal(evidence?.source, source, 'external_evidence_source_mismatch');
  for (const name of EXTERNAL_GROUPS) {
    assert.equal(evidence.groups?.[name]?.status, 'passed', `required_full_group_not_passed:${name}`);
    assert.ok(typeof evidence.groups[name].receipt === 'string' && evidence.groups[name].receipt.length > 0, `required_full_group_receipt_missing:${name}`);
    assert.equal(evidence.groups[name].source, source, `required_full_group_source_mismatch:${name}`);
    assert.match(evidence.groups[name].receiptSha256 ?? '', /^[a-f0-9]{64}$/, `required_full_group_digest_missing:${name}`);
    assert.ok(Array.isArray(evidence.groups[name].resourceIds) && evidence.groups[name].resourceIds.length > 0, `required_full_group_resource_ids_missing:${name}`);
  }
}

export function verifyExternalArtifacts(evidence, root) {
  assertExternalEvidence(evidence, evidence.source);
  const owned = fs.realpathSync(path.join(root, '.temp/v0.3-full-acceptance'));
  for (const name of EXTERNAL_GROUPS) {
    const proof = evidence.groups[name];
    const artifact = fs.realpathSync(path.resolve(root, proof.receipt));
    assert.ok(artifact.startsWith(owned + path.sep), 'external_receipt_outside_owned_acceptance_directory');
    const bytes = fs.readFileSync(artifact);
    assert.ok(bytes.length <= 1024 * 1024, 'external_safe_receipt_size_exceeded');
    assert.equal(createHash('sha256').update(bytes).digest('hex'), proof.receiptSha256, 'external_receipt_digest_changed');
    const receipt = JSON.parse(bytes.toString('utf8'));
    assert.equal(receipt.source, evidence.source, 'external_receipt_source_mismatch');
    assert.equal(receipt.group, name, 'external_receipt_group_mismatch');
    assert.equal(receipt.status, 'passed', 'external_receipt_not_passed');
    assert.deepEqual(receipt.resourceIds, proof.resourceIds, 'external_receipt_resource_ids_mismatch');
    assert.equal(receipt.realm, 'staging', 'external_receipt_not_staging');
    assert.ok(Array.isArray(receipt.checks) && receipt.checks.length > 0, 'external_receipt_checks_missing');
    assert.ok(receipt.checks.every(check => typeof check.name === 'string' && check.result === 'passed' && check.observed != null), 'external_receipt_checks_not_observed');
  }
  return { groups: EXTERNAL_GROUPS.length, status: 'artifact_evidence_verified_final_release_judgment_required' };
}

export function assertCanonicalEvents(events, expectedLane) {
  assert.ok(events.length > 0, 'canonical_events_missing');
  assert.equal(events[0].type, 'response.created', 'canonical_created_event_missing');
  let sequence = -1;
  const ids = new Set();
  for (const event of events) {
    assert.ok(Number.isInteger(event.sequence_number) && event.sequence_number > sequence, 'non_monotonic_sequence');
    sequence = event.sequence_number;
    if (expectedLane === undefined) assert.equal(event.stream_id, undefined, 'default_lane_must_be_omitted');
    else assert.equal(event.stream_id, expectedLane, 'wrong_native_event_lane');
    if (event.response?.id) ids.add(event.response.id);
  }
  assert.equal(ids.size, 1, 'one_response_identity_per_test_dispatch');
  const terminals = events.filter(event => terminalTypes.has(event.type));
  assert.equal(terminals.length, 1, 'exactly_one_authentic_terminal_required');
  assert.equal(terminals[0].type, 'response.completed', 'test_generation_not_completed');
  assert.equal(events.at(-1), terminals[0], 'events_after_terminal');
  const text = events.filter(event => event.type === 'response.output_text.delta').map(event => event.delta).join('');
  const outputText = terminals[0].response.output.filter(item => item.type === 'message').flatMap(item => item.content).filter(part => part.type === 'output_text').map(part => part.text).join('');
  assert.equal(text, outputText, 'streamed_text_differs_from_canonical_terminal');
  return terminals[0].response;
}

export async function exerciseConversations(client, otherClient, remember) {
  const conversation = await client.conversations.create({ metadata: { acceptance: 'full-responses' }, items: [{ role: 'user', content: 'Remember the original branch boundary.' }] });
  remember('conversations', conversation.id);
  assert.equal(conversation.object, 'conversation');
  const updated = await client.conversations.update(conversation.id, { metadata: { acceptance: 'full-responses', revised: 'yes' } });
  assert.deepEqual(updated.metadata, { acceptance: 'full-responses', revised: 'yes' });
  const created = await client.conversations.items.create(conversation.id, { items: [{ role: 'user', content: 'An independently owned item.' }] });
  assert.equal(created.data.length, 1);
  const item = created.data[0];
  const retrieved = await client.conversations.items.retrieve(item.id, { conversation_id: conversation.id });
  assert.deepEqual(retrieved, item);
  const listed = [];
  for await (const value of client.conversations.items.list(conversation.id, { limit: 1, order: 'asc' })) listed.push(value);
  assert.equal(listed.length, 2, 'conversation_cursor_must_traverse_both_items');
  assert.equal(new Set(listed.map(value => value.id)).size, 2);
  await assert.rejects(otherClient.conversations.retrieve(conversation.id), error => error.status === 404, 'foreign_conversation_must_be_not_found');
  await assert.rejects(otherClient.conversations.items.retrieve(item.id, { conversation_id: conversation.id }), error => error.status === 404);
  await client.conversations.items.delete(item.id, { conversation_id: conversation.id });
  await assert.rejects(client.conversations.items.retrieve(item.id, { conversation_id: conversation.id }), error => error.status === 404);
  const retained = listed.find(value => value.id !== item.id);
  const deleted = await client.conversations.delete(conversation.id);
  assert.equal(deleted.deleted, true);
  await assert.rejects(client.conversations.retrieve(conversation.id), error => error.status === 404);
  assert.deepEqual(await client.conversations.items.retrieve(retained.id, { conversation_id: conversation.id }), retained, 'conversation_deletion_must_not_cascade_items');
  await client.conversations.items.delete(retained.id, { conversation_id: conversation.id });
  return { metadata: true, cursorItems: listed.length, itemCrud: true, ownerIsolated: true, deletionRetainsItems: true };
}

export async function exerciseWebSocket(client, parentId, beforeDispatch, remember, makeSocket = value => new ResponsesWS(value, { reconnect: null, maxPayload: 4 * 1024 * 1024 }), observeWire = () => {}) {
  const socket = makeSocket(client);
  const events = new Map([['fork-left', []], ['fork-right', []]]);
  const terminal = new Set();
  const wireEvents = [];
  let accepted = 0;
  try {
    await new Promise((resolve, reject) => {
      const timeout = setTimeout(() => reject(Error('websocket_open_observer_timeout_no_replay')), 30000);
      socket.on('error', () => { clearTimeout(timeout); reject(Error('websocket_open_failed')); });
      socket.socket.on('open', () => { clearTimeout(timeout); resolve(); });
    });
    const done = new Promise((resolve, reject) => {
      socket.on('error', () => reject(Error('websocket_native_error_no_paid_replay')));
      socket.on('close', () => { if (terminal.size !== events.size) reject(Error('websocket_closed_before_canonical_terminal_no_paid_replay')); });
      socket.on('event', event => {
        try {
          assert.ok(events.has(event.stream_id), 'unexpected_websocket_lane');
          events.get(event.stream_id).push(event);
          wireEvents.push(event);
          if (event.type === 'response.created') { remember('responses', event.response.id); accepted++; }
          if (terminalTypes.has(event.type)) terminal.add(event.stream_id);
          if (terminal.size === events.size) resolve();
        } catch (error) { reject(error); }
      });
    });
    for (const stream_id of events.keys()) {
      beforeDispatch(`websocket:${stream_id}`);
      socket.send({ type: 'response.create', stream_id, previous_response_id: parentId, input: 'Reply with the word ready only.', max_output_tokens: 128, reasoning: { effort: 'none' } });
    }
    await done;
    const responses = [...events].map(([lane, values]) => assertCanonicalEvents(values, lane));
    assert.notEqual(responses[0].id, responses[1].id);
    assert.ok(responses.every(response => response.output.some(item => item.type === 'message' && item.content.some(part => part.type === 'output_text' && part.text.length > 0))), 'websocket_real_answer_missing');
    assert.equal(accepted, 2, 'each_socket_dispatch_has_one_created_identity');
    observeWire(wireEvents);
    return { lanes: 2, canonicalTerminals: 2, forkIds: responses.map(value => value.id), parent: parentId };
  } finally { socket.close(); }
}

export async function exerciseNoStoreParentRejection(client, body, { beforeDispatch, remember, record }) {
  const key = beforeDispatch('negative-store-false-parent', { parentResponse: body.previous_response_id, expectedStatus: 404 });
  let result;
  try {
    result = await client.responses.create({ ...body, max_output_tokens: 128, reasoning: { effort: 'none' }, stream: true }, { headers: { 'Idempotency-Key': key } }).withResponse();
  } catch (error) {
    if (error.status !== 404) throw error;
    record({ status: 'rejected', expectedStatus: 404, idempotencyKey: key });
    return { rejected: true };
  }
  const responseId = result.response.headers.get('x-agents-response');
  const sessionId = result.response.headers.get('x-agents-session');
  if (responseId) remember('responses', responseId);
  if (sessionId) remember('sessions', sessionId);
  record({ status: 'unexpected_accepted_observing_same_work', responseId, sessionId, idempotencyKey: key });
  for await (const event of result.data) {
    if (event.response?.id) remember('responses', event.response.id);
    if (terminalTypes.has(event.type)) record({ status: 'unexpected_terminal', terminal: event.type, responseId: event.response.id, sessionId, idempotencyKey: key });
  }
  throw Error('store_false_parent_unexpectedly_admitted_no_replay');
}

function configuration(argv, environment) {
  const { values } = parseArgs({ args: argv, options: { source: { type: 'string' }, output: { type: 'string' }, 'max-paid-submissions': { type: 'string' }, 'allow-model-calls': { type: 'boolean', default: false }, scope: { type: 'string', default: 'full' }, 'external-evidence': { type: 'string' }, 'current-python': { type: 'string' } } });
  assert.match(values.source ?? '', /^[a-f0-9]{40}$/, 'exact_source_required_before_network');
  assert.ok(['core', 'full'].includes(values.scope), 'unknown_acceptance_scope');
  assert.equal(values['allow-model-calls'], true, 'explicit_paid_execution_gate_required');
  assertBudget(9, Number(values['max-paid-submissions']));
  const origin = environment.MAGENTS_TEST_API_ORIGIN;
  assert.equal(origin, 'https://agents-staging.moesegfault.dev', 'staging_only_exact_origin_required');
  assert.ok(environment.MAGENTS_TEST_API_KEY && environment.MAGENTS_TEST_OTHER_API_KEY, 'both_existing_owned_staging_keys_required');
  assert.ok(values.output, 'durable_safe_receipt_path_required');
  assert.ok(values['current-python'], 'explicit_current_python_sdk_gate_required');

  return { ...values, origin, model: 'deepseek-flash', limit: Number(values['max-paid-submissions']) };
}

export async function main(argv = process.argv.slice(2), environment = process.env) {
  const config = configuration(argv, environment);
  const root = path.resolve('.');
  assert.ok(root.replaceAll('\\', '/').endsWith('/.cache/v2-release-worktree'), 'explicit_release_worktree_required');
  const output = path.resolve(config.output);
  assert.ok(output.startsWith(path.join(root, '.temp') + path.sep), 'receipt_must_stay_in_project_temp');
  assert.equal(fs.existsSync(output), false, 'existing_receipt_requires_reconciliation_not_new_execution');
  const policy = await import('./paid-route-policy.mjs');
  await policy.verifyServingSource(config.source, root);
  const headers = { Authorization: `Bearer ${environment.MAGENTS_TEST_API_KEY}` };
  const request = async (method, route, body, expected = [200]) => {
    const response = await fetch(config.origin + route, { method, headers: { ...headers, ...(body === undefined ? {} : { 'Content-Type': 'application/json' }) }, ...(body === undefined ? {} : { body: JSON.stringify(body) }), signal: AbortSignal.timeout(30000) });
    assert.ok(expected.includes(response.status), `platform_request_status:${method}:${response.status}`);
    const bytes = await response.text();
    return bytes ? JSON.parse(bytes) : null;
  };
  const health = await request('GET', '/healthz');
  assert.equal(health.build_id, config.source, 'serving_source_changed');
  const receipt = { source: config.source, scope: config.scope, status: 'running', run: randomUUID(), submissions: 0, resources: { agents: [], sessions: [], responses: [], conversations: [] }, checks: {}, dispatches: [], pending: null };
  const persist = () => { fs.mkdirSync(path.dirname(output), { recursive: true }); fs.writeFileSync(output + '.partial', JSON.stringify(receipt, null, 2)); fs.renameSync(output + '.partial', output); };
  const remember = (kind, id) => { assert.ok(id); if (!receipt.resources[kind].includes(id)) receipt.resources[kind].push(id); persist(); };
  const beforeDispatch = (purpose, details = {}) => { assert.ok(receipt.submissions < config.limit, 'paid_submission_budget_exhausted'); receipt.submissions++; receipt.pending = { purpose, ...details, transport: purpose.startsWith('websocket:') ? 'websocket' : 'http', idempotencyKey: purpose.startsWith('websocket:') ? null : `full-${receipt.run}-${receipt.submissions}` }; receipt.dispatches.push({ submission: receipt.submissions, ...receipt.pending }); persist(); return receipt.pending.idempotencyKey; };
  persist();
  const client = new OpenAI({ apiKey: environment.MAGENTS_TEST_API_KEY, baseURL: config.origin + '/v1', maxRetries: 0, timeout: 300000 });
  const other = new OpenAI({ apiKey: environment.MAGENTS_TEST_OTHER_API_KEY, baseURL: config.origin + '/v1', maxRetries: 0, timeout: 30000 });
  let agent;
  try {
    receipt.checks.conversations = await exerciseConversations(client, other, remember); persist();
    const approvedModel = policy.assertAvailableDirectDeepSeek(await request('GET', '/v1/models'), config.model);
    assert.ok(approvedModel.max_output_tokens >= 512, 'priced_model_output_budget_too_small');
    agent = await request('POST', '/v1/agents', { name: `full-sdk-${receipt.run}`, model: { ...approvedModel, protocol: 'responses', max_output_tokens: 512 }, instructions: 'Respond briefly. Preserve the original user context on branches.', tools: [], skills: [], workspace: false, compaction_percent: 80 }, [201]);
    remember('agents', agent.id); policy.assertDirectDeepSeekModel(agent.model, config.model);
    const ownedSessions = { add: id => remember('sessions', id) };
    const initial = await policy.createPinnedSession(request, agent, ownedSessions, config.model);
    const initialId = typeof initial === 'string' ? initial : initial.id;
    remember('sessions', initialId);
    policy.assertDirectDeepSeekPin(await request('GET', `/v1/sessions/${initialId}`), agent.id, config.model);
    const responseSessions = new Map();
    const create = async (body, sessionId, purpose) => {
      const sourceSession = sessionId ?? responseSessions.get(body.previous_response_id);
      assert.ok(sourceSession, 'prepaid_source_session_not_verified');
      policy.assertDirectDeepSeekPin(await request('GET', `/v1/sessions/${sourceSession}`), agent.id, config.model);
      const key = beforeDispatch(purpose);
      const result = await client.responses.create({ model: agent.id, max_output_tokens: 128, reasoning: { effort: 'none' }, ...body, stream: true }, { headers: { 'Idempotency-Key': key, ...(sessionId ? { 'X-Agents-Session': sessionId } : {}) } }).withResponse();
      const id = result.response.headers.get('x-agents-response');
      const session = result.response.headers.get('x-agents-session');
      if (id) remember('responses', id);
      if (session) remember('sessions', session);
      const events = [];
      for await (const event of result.data) { events.push(event); if (event.type === 'response.created') remember('responses', event.response.id); }
      const response = assertCanonicalEvents(events);
      assert.equal(response.id, id, 'canonical_header_identity');
      assert.ok(response.output.some(item => item.type === 'message' && item.content.some(part => part.type === 'output_text' && part.text.length > 0)), 'real_answer_missing');
      policy.assertDirectDeepSeekPin(await request('GET', `/v1/sessions/${session}`), agent.id, config.model);
      responseSessions.set(response.id, session);
      receipt.pending = null; persist();
      return { response, events, session };
    };
    const activeConversation = await client.conversations.create({ metadata: { acceptance: receipt.run }, items: [{ role: 'user', content: 'The seeded conversation context is SEED.' }] });
    remember('conversations', activeConversation.id);
    const first = await create({ conversation: activeConversation.id, input: 'The original branch marker is ORIGIN. Reply ready.', instructions: 'Keep the original marker available for later branch questions.' }, initialId, 'http-root');
    const conversationItems = [];
    for await (const item of client.conversations.items.list(activeConversation.id, { limit: 1, order: 'asc' })) conversationItems.push(item);
    assert.equal(new Set(conversationItems.map(item => item.id)).size, conversationItems.length, 'conversation_response_appended_duplicates');
    assert.ok(conversationItems.length >= 3, 'conversation_response_missing_new_input_or_output');
    assert.equal(conversationItems.filter(item => item.role === 'user' && JSON.stringify(item.content).includes('The original branch marker is ORIGIN.')).length, 1, 'conversation_root_input_not_appended_once');
    for (const output of first.response.output) assert.equal(conversationItems.filter(item => item.id === output.id).length, 1, 'conversation_output_item_not_appended_once');
    const conversationWire = await (await client.conversations.items.list(activeConversation.id, { limit: 100, order: 'asc' }).asResponse()).json();
    const immutable = await client.responses.retrieve(first.response.id);
    const next = await create({ previous_response_id: first.response.id, input: 'This later-head-only marker is LATER. Reply ready.' }, null, 'http-current-head');
    const fork = await create({ previous_response_id: first.response.id, input: 'Which marker was present before this branch? Reply briefly without inventing later history.' }, null, 'http-historical-fork');
    assert.notEqual(fork.session, next.session, 'historical_fork_must_not_rewind_source_session');
    assert.deepEqual(await client.responses.retrieve(first.response.id), immutable, 'old_response_mutated');
    const source = await request('GET', `/v1/sessions/${next.session}`);
    const child = await request('GET', `/v1/sessions/${fork.session}`);
    assert.deepEqual(child.pinned, source.pinned, 'same_agent_fork_pin_changed');
    assert.ok(source.messages.some(value => JSON.stringify(value).includes('LATER')));
    assert.ok(!child.messages.some(value => JSON.stringify(value).includes('LATER')), 'historical_fork_inherited_later_history');
    receipt.checks.http_historical_forks = { original: first.response.id, source: next.response.id, fork: fork.response.id, immutableSha256: digest(immutable), sourcePinSha256: digest(source.pinned), sourcePrefixIsolated: true };
    receipt.reusable = { responseId: first.response.id, sessionId: first.session, agentId: agent.id };
    const cursor = first.events.find(value => value.type === 'response.output_text.delta')?.sequence_number;
    assert.ok(Number.isInteger(cursor), 'root_text_delta_missing');
    const replay = [];
    for await (const event of await client.responses.retrieve(first.response.id, { stream: true, starting_after: cursor })) replay.push(event);
    assert.deepEqual(replay, first.events.filter(value => value.sequence_number > cursor), 'canonical_get_replay_changed');
    const count = await client.responses.inputTokens.count({ model: agent.id, previous_response_id: first.response.id, input: 'Count this exact continuation.', instructions: 'Use the original branch marker.' });
    assert.equal(count.object, 'response.input_tokens'); assert.ok(Number.isSafeInteger(count.input_tokens) && count.input_tokens > 0);
    const key = beforeDispatch('standalone-compact');
    const compact = await client.responses.compact({ model: agent.id, previous_response_id: first.response.id, instructions: 'Keep the original branch marker exactly.' }, { headers: { 'Idempotency-Key': key } });
    assert.equal(compact.object, 'response.compaction'); assert.ok(compact.output.some(value => value.type === 'compaction' && value.encrypted_content));
    assert.ok(compact.usage && compact.usage.total_tokens > 0, 'genuine_compact_usage_required');
    receipt.pending = null; receipt.checks.count_compact = { inputTokens: count.input_tokens, compactId: compact.id, compactObject: compact.object, outputItems: compact.output.length, outputSha256: digest(compact.output) }; persist();
    const compactContinuation = await policy.createPinnedSession(request, agent, ownedSessions, config.model);
    const compactSession = typeof compactContinuation === 'string' ? compactContinuation : compactContinuation.id; remember('sessions', compactSession);
    const reused = await create({ input: [...compact.output, { role: 'user', content: 'Report the original marker briefly.' }], previous_response_id: null }, compactSession, 'standalone-window-reuse');
    assert.equal(reused.response.previous_response_id ?? null, null, 'standalone_compact_must_start_fresh_chain');
    assert.ok(reused.response.output.filter(item => item.type === 'message').flatMap(item => item.content).filter(part => part.type === 'output_text').some(part => part.text.includes('ORIGIN')), 'standalone_compact_lost_original_marker');
    const noStoreSession = await policy.createPinnedSession(request, agent, ownedSessions, config.model);
    const noStoreId = typeof noStoreSession === 'string' ? noStoreSession : noStoreSession.id; remember('sessions', noStoreId);
    const noStore = await create({ input: 'Reply ready.', store: false }, noStoreId, 'store-false');
    await assert.rejects(client.responses.retrieve(noStore.response.id), error => error.status === 404);
    policy.assertDirectDeepSeekPin(await request('GET', `/v1/sessions/${noStoreId}`), agent.id, config.model);
    await exerciseNoStoreParentRejection(client, { model: agent.id, previous_response_id: noStore.response.id, input: 'This unavailable parent must not create a model run.' }, { beforeDispatch, remember, record: result => { receipt.checks.store_false_parent_negative = result; if (result.status === 'rejected') receipt.pending = null; persist(); } });
    await client.responses.delete(fork.response.id);
    await assert.rejects(client.responses.retrieve(fork.response.id), error => error.status === 404);
    assert.deepEqual(await client.responses.retrieve(first.response.id), immutable, 'child_tombstone_deleted_parent');
    receipt.checks.store_false_tombstone = { noStoreUnavailable: true, childDeletedParentIntact: true }; persist();
    policy.assertDirectDeepSeekPin(await request('GET', `/v1/sessions/${first.session}`), agent.id, config.model);
    let websocketWire;
    receipt.checks.websocket_lanes = await exerciseWebSocket(client, first.response.id, beforeDispatch, remember, undefined, events => { websocketWire = events; });
    const all = await request('GET', `/v1/agents/${agent.id}/sessions`);
    for (const session of all.data) { remember('sessions', session.id); policy.assertDirectDeepSeekPin(await request('GET', `/v1/sessions/${session.id}`), agent.id, config.model); }
    const python = path.resolve(config['current-python']);
    assert.ok(python.startsWith(path.join(root, '.cache') + path.sep), 'current_sdk_interpreter_must_be_project_managed');
    const wire = JSON.stringify({ response: first.response, events: first.events, websocket_events: websocketWire, compact, count, conversation: activeConversation, conversation_items: conversationWire });
    assert.ok(Buffer.byteLength(wire, 'utf8') <= 16 * 1024 * 1024, 'actual_wire_bundle_size_exceeded');
    const typedWire = JSON.parse(execFileSync(python, [path.join(root, 'tests/e2e/current_responses_sdk.py'), 'wire', '--input', '-'], { cwd: root, input: wire, encoding: 'utf8', maxBuffer: 1024 * 1024, timeout: 30000, windowsHide: true, stdio: ['pipe', 'pipe', 'pipe'] }));
    assert.equal(typedWire.passed, true, 'actual_wire_current_sdk_validation_failed');
    receipt.checks.current_python_wire = { sdk: typedWire.sdk, actualPayloads: true, newModelRuns: 0, responseEvents: typedWire.response_events, websocketEvents: typedWire.websocket_events }; persist();
    const currentReceipt = output.replace(/\.json$/, '') + '-current-python.json';
    const current = JSON.parse(execFileSync(python, [path.join(root, 'tests/e2e/current_responses_sdk.py'), 'staging-inspect', '--source', config.source, '--session', first.session, '--response', first.response.id, '--agent', agent.id, '--receipt', currentReceipt], { cwd: root, env: { ...environment, MAGENTS_TEST_EXPECTED_BUILD: config.source }, encoding: 'utf8', maxBuffer: 1024 * 1024, timeout: 300000, windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'] }));
    assert.equal(current.passed, true, 'current_python_sdk_gate_failed');
    assert.equal(current.new_model_runs, 0, 'current_python_sdk_must_reuse_canonical_trajectory');
    receipt.checks.current_python_sdk = { sdk: current.sdk, receipt: currentReceipt, newModelRuns: 0 };
    receipt.pending = null;
    receipt.status = 'core_passed_full_acceptance_not_claimed'; persist();
    if (config.scope === 'full') {
      receipt.status = 'core_passed_full_gate_not_ready'; persist();
      assert.ok(config['external-evidence'], 'full_acceptance_requires_external_evidence');
      const evidence = JSON.parse(fs.readFileSync(config['external-evidence'], 'utf8'));
      assertExternalEvidence(evidence, config.source);
      verifyExternalArtifacts(evidence, root);
      receipt.status = 'core_and_external_evidence_passed_final_release_judgment_required'; persist();
    }
  } catch (error) {
    if (receipt.status !== 'core_passed_full_gate_not_ready') receipt.status = 'failed'; receipt.failure = { name: error.name, ...(Number.isInteger(error.status) ? { status: error.status } : {}) }; persist();
    throw error;
  } finally {
    const failures = [];
    if (agent) {
      try { const list = await request('GET', `/v1/agents/${agent.id}/sessions`); for (const session of list.data) remember('sessions', session.id); } catch { failures.push({ kind: 'session_discovery', id: agent.id }); }
    }
    for (const id of receipt.resources.conversations) {
      try {
        const items = [];
        for await (const item of client.conversations.items.list(id, { limit: 100 })) items.push(item);
        for (const item of items) await client.conversations.items.delete(item.id, { conversation_id: id });
        try { await client.conversations.delete(id); } catch (error) { if (error.status !== 404) throw error; }
      } catch { failures.push({ kind: 'conversation', id }); }
    }
    for (const id of receipt.resources.sessions) {
      try { await request('DELETE', `/v1/sessions/${id}`, undefined, [200, 404]); } catch { failures.push({ kind: 'session', id }); }
    }
    if (agent) {
      try { await request('DELETE', `/v1/agents/${agent.id}`, undefined, [200, 404]); } catch { failures.push({ kind: 'agent', id: agent.id }); }
    }
    receipt.cleanup = { failures, responsesRemainCanonical: true }; persist();
    assert.equal(failures.length, 0, 'owned_fixture_cleanup_incomplete');
  }
  process.stdout.write(JSON.stringify({ source: receipt.source, status: receipt.status, groups: Object.keys(receipt.checks), submissions: receipt.submissions }) + '\n');
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) main().catch(error => { process.stderr.write(JSON.stringify({ result: 'failed', error: error.name, ...(Number.isInteger(error.status) ? { status: error.status } : {}) }) + '\n'); process.exitCode = 1; });
