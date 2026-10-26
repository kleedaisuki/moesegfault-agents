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
export const RUNNER_CONTRACT = 'full-responses-core-prepaid-count-trace-management-v1';
const digest = value => createHash('sha256').update(JSON.stringify(value)).digest('hex');

export function nativeAcceptanceModel(approvedModel) {
  return { ...approvedModel, protocol: 'responses' };
}

export function assertExecutionWorkspace(root, source, environment) {
  const canonical = fs.realpathSync(root);
  if (environment.GITHUB_ACTIONS === 'true') {
    assert.equal(environment.GITHUB_REPOSITORY, 'kleedaisuki/moesegfault-agents', 'untrusted_actions_repository');
    assert.equal(environment.GITHUB_SHA, source, 'actions_source_mismatch');
    assert.ok(environment.GITHUB_WORKSPACE, 'actions_workspace_missing');
    assert.equal(fs.realpathSync(environment.GITHUB_WORKSPACE), canonical, 'actions_cwd_workspace_mismatch');
    return { mode: 'trusted_actions_workspace', source };
  }
  assert.equal(canonical.replaceAll('\\', '/').toLowerCase(), 'd:/code/moesegfault-agents/.cache/v2-release-worktree', 'explicit_release_worktree_required');
  return { mode: 'owned_local_release_worktree', source };
}

export function traceHeaders(seed) {
  if (!seed) return {};
  assert.match(seed, /^[1-9][0-9]*:[1-9][0-9]*$/, 'invalid_ci_trace_seed');
  return { traceparent: `00-${createHash('sha256').update(seed).digest('hex').slice(0, 32)}-${randomUUID().replaceAll('-', '').slice(0, 16)}-00` };
}

export function tracedFetch(fetcher, seed) {
  return (url, init = {}) => fetcher(url, { ...init, headers: new Headers({ ...Object.fromEntries(new Headers(init.headers)), ...traceHeaders(seed) }) });
}

export async function archiveCompletedSession(request, session) {
  const before = await request('GET', `/v1/sessions/${session}`);
  if (before.status === 'closed') return;
  assert.equal(before.run_id ?? null, null, 'never_archive_an_unobserved_active_execution');
  await request('DELETE', `/v1/sessions/${session}`, undefined, [200, 202, 404]);
  const deadline = Date.now() + 120000;
  while (Date.now() < deadline) {
    if ((await request('GET', `/v1/sessions/${session}`)).status === 'closed') return;
    await new Promise(resolve => setTimeout(resolve, 1000));
  }
  throw Error('archive_observer_timeout_no_reexecution');
}

export async function createManagedConversation(client, body, { begin, remember, complete }) {
  const key = begin('conversation-create', { bodySha256: digest(body) });
  const conversation = await client.conversations.create(body, { maxRetries: 0, headers: { 'Idempotency-Key': key } });
  remember('conversations', conversation.id);
  complete(key, conversation.id);
  return conversation;
}

export async function countRootWithoutAdmission(client, request, sessionId, body) {
  const before = await request('GET', `/v1/sessions/${sessionId}`);
  assert.equal(before.status, 'idle', 'prepaid_count_session_not_idle');
  assert.ok(Array.isArray(before.messages) && before.pinned, 'prepaid_count_canonical_source_missing');
  assert.ok(Number.isSafeInteger(before.input_revision) && Number.isSafeInteger(before.last_input_at_ms), 'prepaid_count_user_clock_missing');
  assert.equal(before.run_id ?? null, null, 'prepaid_count_session_already_running');
  const conversationPath = typeof body.conversation === 'string' ? `/v1/conversations/${body.conversation}/items?limit=100&order=asc` : null;
  const conversationBefore = conversationPath ? await request('GET', conversationPath) : null;
  if (conversationBefore) assert.equal(conversationBefore.has_more, false, 'prepaid_count_seed_snapshot_truncated');
  const count = await client.responses.inputTokens.count(body, { headers: { 'X-Agents-Session': sessionId } });
  assert.equal(count.object, 'response.input_tokens');
  assert.ok(Number.isSafeInteger(count.input_tokens) && count.input_tokens > 0, 'genuine_prepaid_count_required');
  const after = await request('GET', `/v1/sessions/${sessionId}`);
  assert.deepEqual(after.pinned, before.pinned, 'count_changed_initial_pin');
  assert.deepEqual(after.messages, before.messages, 'count_changed_initial_source');
  assert.equal(after.input_revision, before.input_revision, 'count_changed_user_clock_revision');
  assert.equal(after.last_input_at_ms, before.last_input_at_ms, 'count_changed_user_clock_time');
  assert.equal(after.run_id ?? null, before.run_id ?? null, 'count_admitted_a_run');
  if (conversationPath) assert.deepEqual(await request('GET', conversationPath), conversationBefore, 'count_changed_conversation_seed');
  return { count, evidence: { status: 'passed', inputTokens: count.input_tokens, pinSha256: digest(before.pinned), sourceSha256: digest(before.messages), userClockUnchanged: true, noRunAdmitted: true } };
}

export function compareRootCount(count, response) {
  const usageSource = response.usage?.input_tokens != null ? 'usage' : 'agents_usage';
  const input = response[usageSource]?.input_tokens;
  if (!Number.isSafeInteger(input) || input < 0) return { status: 'not_compared_provider_standard_usage_unavailable' };
  assert.equal(input, count.input_tokens, 'same_rendered_root_count_differs_from_provider_usage');
  return { status: 'equal_provider_reported_input_tokens', inputTokens: input, usageSource };
}

const COMPONENTS = ['worker', 'native', 'container', 'dashboard', 'tooling'];

function assertComponentFingerprints(fingerprints) {
  assert.ok(fingerprints && COMPONENTS.every(name => /^[a-f0-9]{64}$/.test(fingerprints[name] ?? '')), 'all_five_component_fingerprints_required');
  assert.equal(Object.keys(fingerprints).length, COMPONENTS.length, 'unknown_component_fingerprint');
}

function readProjectTempJson(file, root) {
  const target = fs.realpathSync(path.resolve(root, file));
  const owned = fs.realpathSync(path.join(root, '.temp'));
  assert.ok(target.startsWith(owned + path.sep), 'sealed_artifact_outside_project_temp');
  const bytes = fs.readFileSync(target);
  assert.ok(bytes.length <= 1024 * 1024, 'sealed_artifact_size_exceeded');
  return { target, value: JSON.parse(bytes.toString('utf8')), sha256: createHash('sha256').update(bytes).digest('hex') };
}

export function requireLiveDeploymentProvenance(environment, source, root) {
  assert.ok(typeof environment.MAGENTS_TEST_DEPLOYMENT_PROVENANCE === 'string' && environment.MAGENTS_TEST_DEPLOYMENT_PROVENANCE.length > 0, 'live_deployment_provenance_required_before_network');
  return readDeploymentProvenance(environment.MAGENTS_TEST_DEPLOYMENT_PROVENANCE, source, root);
}

export function readDeploymentProvenance(file, source, root) {
  const loaded = readProjectTempJson(file, root), receipt = loaded.value;
  assert.equal(receipt.schema_version, 1, 'deployment_provenance_schema_changed');
  assert.equal(receipt.status, 'incremental_build_and_selected_checks_verified', 'deployment_provenance_not_validated');
  assert.equal(receipt.source_sha, source, 'deployment_provenance_source_mismatch');
  assert.deepEqual(receipt.diagnostic_only_components, [], 'diagnostic_scope_cannot_seal_core');
  assert.equal(receipt.staging_acceptance, 'not_executed', 'deployment_build_provenance_scope_changed');
  assert.equal(receipt.production_authorized, false, 'deployment_provenance_cannot_authorize_production');
  assertComponentFingerprints(receipt.component_fingerprints);
  assert.match(String(receipt.source_run ?? ''), /^[1-9][0-9]*$/, 'deployment_source_run_missing');
  assert.ok(Array.isArray(receipt.passed_current_checks) && receipt.baseline_evidence && typeof receipt.baseline_evidence === 'object', 'deployment_selected_check_provenance_missing');
  for (const name of ['worker', 'dashboard', 'container']) {
    const artifact = receipt.artifacts?.[name];
    assert.ok(artifact && Number.isSafeInteger(artifact.id) && artifact.id > 0, 'deployment_artifact_provenance_missing');
    assert.match(String(artifact.run_id ?? ''), /^[1-9][0-9]*$/, 'deployment_artifact_run_missing');
    assert.match(artifact.source_sha ?? '', /^[a-f0-9]{40}$/, 'deployment_artifact_source_missing');
    assert.equal(artifact.fingerprint, receipt.component_fingerprints[name], 'deployment_artifact_component_mismatch');
    assert.ok(['built_current', 'reused_identical_artifact'].includes(artifact.status) || name === 'container' && artifact.status === 'core_compatible_disabled_runtime' && receipt.runtime_scope === 'core-compatible/disabled-runtime', 'deployment_artifact_scope_not_accepted');
  }
  const componentSources = Object.fromEntries(['worker', 'dashboard', 'container'].map(name => [name, receipt.artifacts[name].source_sha]));
  return { source, assemblySource: source, workerSource: componentSources.worker, component_sources: componentSources, receipt: path.relative(root, loaded.target), receiptSha256: loaded.sha256, component_fingerprints: receipt.component_fingerprints };
}

export function verifyWorkerIdentity(provenance, root, verifyServingSource) {
  assert.equal(provenance.workerSource, provenance.component_sources.worker, 'worker_source_map_mismatch');
  return verifyServingSource(provenance.workerSource, root);
}

export function assertServingWorkerHealth(health, provenance) {
  assert.equal(health.status, 'ok', 'serving_worker_unhealthy');
  assert.equal(health.build_id, provenance.workerSource, 'serving_worker_source_changed');
}

export function currentPythonWorkerArguments(root, provenance, handles, receipt) {
  return [path.join(root, 'tests/e2e/current_responses_sdk.py'), 'staging-inspect', '--source', provenance.workerSource, '--session', handles.session, '--response', handles.response, '--agent', handles.agent, '--receipt', receipt];
}

export function currentPythonWorkerEnvironment(environment, provenance) {
  return { ...environment, MAGENTS_TEST_EXPECTED_BUILD: provenance.workerSource, MAGENTS_TEST_DEPLOYMENT_ASSEMBLY_SHA: provenance.assemblySource };
}

export function aggregateEvidence(config, root) {
  const owned = fs.realpathSync(path.join(root, '.temp/v0.3-full-acceptance'));
  const loaded = readProjectTempJson(config['core-receipt'], root), core = loaded.value;
  assert.ok(loaded.target.startsWith(owned + path.sep), 'core_receipt_outside_owned_acceptance_directory');
  assert.match(core.source ?? '', /^[a-f0-9]{40}$/, 'sealed_core_source_invalid');
  assert.equal(core.runnerContract, RUNNER_CONTRACT, 'sealed_core_contract_version_mismatch');
  assert.ok(['core_passed_full_acceptance_not_claimed', 'core_passed_full_gate_not_ready', 'core_and_external_evidence_passed_final_release_judgment_required'].includes(core.status), 'sealed_core_not_passed');
  assert.equal(core.pending, null, 'sealed_core_has_unresolved_work');
  assert.ok(Array.isArray(core.managementIntents) && core.managementIntents.every(intent => intent.status === 'completed'), 'sealed_core_management_unresolved');
  assert.ok(core.dependencies?.nodeSdk && core.dependencies?.currentPythonSdk, 'sealed_core_dependency_baseline_missing');
  assert.deepEqual(core.cleanup?.failures, [], 'sealed_core_cleanup_incomplete');
  for (const group of [...CORE_GROUPS, 'prepaid_count', 'current_python_wire', 'current_python_sdk']) assert.ok(core.checks?.[group], `sealed_core_group_missing:${group}`);
  const external = readProjectTempJson(config['external-evidence'], root).value;
  const sameSource = core.source === config.source;
  if (!sameSource) assert.ok(external.core_baseline && external.current_deployment, 'sealed_core_source_mismatch_requires_explicit_baseline_and_current_provenance');
  if (external.core_baseline) {
    const seal = external.core_baseline;
    const sealedCore = readProjectTempJson(seal.receipt, root);
    assert.equal(sealedCore.target, loaded.target, 'core_baseline_receipt_identity_mismatch');
    assert.equal(seal.receiptSha256, loaded.sha256, 'core_baseline_receipt_hash_mismatch');
    assert.equal(seal.source, core.source, 'core_baseline_source_mismatch');
    assertComponentFingerprints(core.component_fingerprints);
    assert.deepEqual(seal.component_fingerprints, core.component_fingerprints, 'core_baseline_fingerprints_mismatch');
  }
  let current = null;
  if (external.current_deployment) {
    assert.equal(external.current_deployment.source, config.source, 'current_deployment_source_mismatch');
    current = readDeploymentProvenance(external.current_deployment.receipt, config.source, root);
    assert.equal(external.current_deployment.receiptSha256, current.receiptSha256, 'current_deployment_receipt_hash_mismatch');
  }
  assertExternalEvidence(external, config.source);
  const verified = verifyExternalArtifacts(external, root);
  return { source: config.source, assemblySource: config.source, runnerSource: config.source, workerSource: current?.workerSource ?? (sameSource ? core.workerSource ?? null : null), component_sources: current?.component_sources ?? (sameSource ? core.component_sources ?? null : null), baselineSource: core.source, baselineWorkerSource: core.workerSource ?? null, baselineComponentSources: core.component_sources ?? null, currentWorkerSource: current?.workerSource ?? null, currentComponentSources: current?.component_sources ?? null, runnerContract: RUNNER_CONTRACT, status: sameSource ? 'sealed_core_and_external_evidence_verified_final_release_judgment_required' : 'baseline_not_reexecuted_final_judgment_required', coreReceipt: path.relative(root, loaded.target), coreReceiptSha256: loaded.sha256, groups: verified.groups, newModelRuns: 0, networkRequests: 0, servingSourceRechecked: false, dependencyBaseline: { status: 'sealed_core_not_retested', ...core.dependencies }, baselineComponentFingerprints: core.component_fingerprints ?? null, currentComponentFingerprints: current?.component_fingerprints ?? null, changedComponents: current && core.component_fingerprints ? COMPONENTS.filter(name => current.component_fingerprints[name] !== core.component_fingerprints[name]) : [], currentDeployment: current };
}
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

export async function exerciseConversations(client, otherClient, remember, createConversation = body => client.conversations.create(body)) {
  const conversation = await createConversation({ metadata: { acceptance: 'full-responses' }, items: [{ role: 'user', content: 'Remember the original branch boundary.' }] });
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

export async function exerciseWebSocket(client, parentId, beforeDispatch, remember, makeSocket, observeWire = () => {}, traceSeed) {
  const socket = (makeSocket ?? (value => new ResponsesWS(value, { reconnect: null, maxPayload: 4 * 1024 * 1024, headers: traceHeaders(traceSeed) })))(client);
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

export function configuration(argv, environment) {
  const { values } = parseArgs({ args: argv, options: { source: { type: 'string' }, output: { type: 'string' }, 'max-paid-submissions': { type: 'string' }, 'allow-model-calls': { type: 'boolean', default: false }, scope: { type: 'string', default: 'full' }, 'external-evidence': { type: 'string' }, 'current-python': { type: 'string' }, 'aggregate-only': { type: 'boolean', default: false }, 'core-receipt': { type: 'string' }, 'omit-compact': { type: 'boolean', default: false } } });
  assert.match(values.source ?? '', /^[a-f0-9]{40}$/, 'exact_source_required_before_network');
  assert.ok(['core', 'full'].includes(values.scope), 'unknown_acceptance_scope');
  assert.ok(!values['omit-compact'] || values.scope === 'core', 'compact_omission_cannot_claim_full_acceptance');
  if (values['aggregate-only']) {
    assert.ok(values['core-receipt'] && values['external-evidence'] && values.output, 'aggregation_requires_sealed_core_external_and_output');
    assert.equal(values['allow-model-calls'], false, 'evidence_only_mode_must_not_authorize_generation');
    return values;
  }
  assert.equal(values['allow-model-calls'], true, 'explicit_paid_execution_gate_required');
  assertBudget(values['omit-compact'] ? 7 : 9, Number(values['max-paid-submissions']));
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
  assertExecutionWorkspace(root, config.source, environment);
  const output = path.resolve(config.output);
  assert.ok(output.startsWith(path.join(root, '.temp') + path.sep), 'receipt_must_stay_in_project_temp');
  assert.equal(fs.existsSync(output), false, 'existing_receipt_requires_reconciliation_not_new_execution');
  if (config['aggregate-only']) {
    const result = aggregateEvidence(config, root);
    fs.mkdirSync(path.dirname(output), { recursive: true }); fs.writeFileSync(output, JSON.stringify(result, null, 2), { flag: 'wx' });
    process.stdout.write(JSON.stringify(result) + '\n'); return;
  }
  const deploymentProvenance = requireLiveDeploymentProvenance(environment, config.source, root);
  const policy = await import('./paid-route-policy.mjs');
  await verifyWorkerIdentity(deploymentProvenance, root, policy.verifyServingSource);
  const transport = tracedFetch(fetch, environment.MAGENTS_TEST_TRACE_SEED);
  const headers = { Authorization: `Bearer ${environment.MAGENTS_TEST_API_KEY}` };
  const request = async (method, route, body, expected = [200]) => {
    const response = await transport(config.origin + route, { method, headers: { ...headers, ...(body === undefined ? {} : { 'Content-Type': 'application/json' }) }, ...(body === undefined ? {} : { body: JSON.stringify(body) }), signal: AbortSignal.timeout(30000) });
    assert.ok(expected.includes(response.status), `platform_request_status:${method}:${response.status}`);
    const bytes = await response.text();
    return bytes ? JSON.parse(bytes) : null;
  };
  const health = await request('GET', '/healthz');
  assertServingWorkerHealth(health, deploymentProvenance);
  const receipt = { source: config.source, assemblySource: config.source, runnerSource: config.source, workerSource: deploymentProvenance.workerSource, component_sources: deploymentProvenance.component_sources, runnerContract: RUNNER_CONTRACT, component_fingerprints: deploymentProvenance?.component_fingerprints ?? null, deployment_provenance: deploymentProvenance, dependencies: { nodeSdk: JSON.parse(fs.readFileSync(new URL('../../node_modules/openai/package.json', import.meta.url), 'utf8')).version, currentPythonSdk: null }, scope: config.scope, status: 'running', run: randomUUID(), submissions: 0, resources: { agents: [], sessions: [], responses: [], conversations: [] }, checks: {}, dispatches: [], managementIntents: [], pending: null };
  const persist = () => { fs.mkdirSync(path.dirname(output), { recursive: true }); fs.writeFileSync(output + '.partial', JSON.stringify(receipt, null, 2)); fs.renameSync(output + '.partial', output); };
  const remember = (kind, id) => { assert.ok(id); if (!receipt.resources[kind].includes(id)) receipt.resources[kind].push(id); persist(); };
  const management = { begin: (purpose, details) => { const intent = { purpose, ...details, idempotencyKey: `full-management-${receipt.run}-${receipt.managementIntents.length + 1}`, status: 'submitting_do_not_replay_if_ambiguous' }; receipt.managementIntents.push(intent); persist(); return intent.idempotencyKey; }, remember, complete: (key, id) => { const intent = receipt.managementIntents.find(value => value.idempotencyKey === key); intent.status = 'completed'; intent.resourceId = id; persist(); } };
  const beforeDispatch = (purpose, details = {}) => { assert.ok(receipt.submissions < config.limit, 'paid_submission_budget_exhausted'); receipt.submissions++; receipt.pending = { purpose, ...details, transport: purpose.startsWith('websocket:') ? 'websocket' : 'http', idempotencyKey: purpose.startsWith('websocket:') ? null : `full-${receipt.run}-${receipt.submissions}` }; receipt.dispatches.push({ submission: receipt.submissions, ...receipt.pending }); persist(); return receipt.pending.idempotencyKey; };
  persist();
  const client = new OpenAI({ apiKey: environment.MAGENTS_TEST_API_KEY, baseURL: config.origin + '/v1', maxRetries: 0, timeout: 300000, fetch: transport });
  const other = new OpenAI({ apiKey: environment.MAGENTS_TEST_OTHER_API_KEY, baseURL: config.origin + '/v1', maxRetries: 0, timeout: 30000, fetch: transport });
  let agent;
  try {
    receipt.checks.conversations = await exerciseConversations(client, other, remember, body => createManagedConversation(client, body, management)); persist();
    const approvedModel = policy.assertAvailableDirectDeepSeek(await request('GET', '/v1/models'), config.model);
    assert.ok(approvedModel.max_output_tokens >= 512, 'priced_model_output_budget_too_small');
    agent = await request('POST', '/v1/agents', { name: `full-sdk-${receipt.run}`, model: nativeAcceptanceModel(approvedModel), instructions: 'Respond briefly. Preserve the original user context on branches.', tools: [], skills: [], workspace: false, compaction_percent: 80 }, [201]);
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
    const activeConversation = await createManagedConversation(client,{ metadata: { acceptance: receipt.run }, items: [{ role: 'user', content: 'The seeded conversation context is SEED.' }] }, management);
    remember('conversations', activeConversation.id);
    const rootBody = { model: agent.id, conversation: activeConversation.id, input: 'The original branch marker is ORIGIN. Reply ready.', instructions: 'Keep the original marker available for later branch questions.', reasoning: { effort: 'none' } };
    receipt.checks.prepaid_count = { status: 'count_only_submitting', bodySha256: digest(rootBody) }; persist();
    const prepaid = await countRootWithoutAdmission(client, request, initialId, rootBody);
    const count = prepaid.count;
    receipt.checks.prepaid_count = prepaid.evidence; persist();
    const first = await create(rootBody, initialId, 'http-root');
    receipt.checks.prepaid_count.parity = compareRootCount(count, first.response); persist();
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
    let compact;
    if (!config['omit-compact']) {
      const key = beforeDispatch('standalone-compact');
      compact = await client.responses.compact({ model: agent.id, previous_response_id: first.response.id, instructions: 'Keep the original branch marker exactly.' }, { headers: { 'Idempotency-Key': key } });
      assert.equal(compact.object, 'response.compaction'); assert.ok(compact.output.some(value => value.type === 'compaction' && value.encrypted_content));
      assert.ok(compact.usage && compact.usage.total_tokens > 0, 'genuine_compact_usage_required');
      receipt.pending = null; receipt.checks.count_compact = { inputTokens: count.input_tokens, compactId: compact.id, compactObject: compact.object, outputItems: compact.output.length, outputSha256: digest(compact.output) }; persist();
      const compactContinuation = await policy.createPinnedSession(request, agent, ownedSessions, config.model);
      const compactSession = typeof compactContinuation === 'string' ? compactContinuation : compactContinuation.id; remember('sessions', compactSession);
      const reused = await create({ input: [...compact.output, { role: 'user', content: 'Report the original marker briefly.' }], previous_response_id: null }, compactSession, 'standalone-window-reuse');
      assert.equal(reused.response.previous_response_id ?? null, null, 'standalone_compact_must_start_fresh_chain');
      assert.ok(reused.response.output.filter(item => item.type === 'message').flatMap(item => item.content).filter(part => part.type === 'output_text').some(part => part.text.includes('ORIGIN')), 'standalone_compact_lost_original_marker');
    } else {
      receipt.notSelected = { compact: 'provider_cache_write_metric_unavailable' }; persist();
    }
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
    receipt.checks.websocket_lanes = await exerciseWebSocket(client, first.response.id, beforeDispatch, remember, undefined, events => { websocketWire = events; }, environment.MAGENTS_TEST_TRACE_SEED);
    const all = await request('GET', `/v1/agents/${agent.id}/sessions`);
    for (const session of all.data) { remember('sessions', session.id); policy.assertDirectDeepSeekPin(await request('GET', `/v1/sessions/${session.id}`), agent.id, config.model); }
    const python = path.resolve(config['current-python']);
    assert.ok(python.startsWith(path.join(root, '.cache') + path.sep), 'current_sdk_interpreter_must_be_project_managed');
    const wire = JSON.stringify({ response: first.response, events: first.events, websocket_events: websocketWire, compact, count, conversation: activeConversation, conversation_items: conversationWire });
    assert.ok(Buffer.byteLength(wire, 'utf8') <= 16 * 1024 * 1024, 'actual_wire_bundle_size_exceeded');
    const typedWire = JSON.parse(execFileSync(python, [path.join(root, 'tests/e2e/current_responses_sdk.py'), config['omit-compact'] ? 'api-wire' : 'wire', '--input', '-'], { cwd: root, input: wire, encoding: 'utf8', maxBuffer: 1024 * 1024, timeout: 30000, windowsHide: true, stdio: ['pipe', 'pipe', 'pipe'] }));
    assert.equal(typedWire.passed, true, 'actual_wire_current_sdk_validation_failed');
    receipt.dependencies.currentPythonSdk = typedWire.sdk;
    receipt.checks.current_python_wire = { sdk: typedWire.sdk, actualPayloads: true, newModelRuns: 0, responseEvents: typedWire.response_events, websocketEvents: typedWire.websocket_events }; persist();
    const currentReceipt = output.replace(/\.json$/, '') + '-current-python.json';
    const current = JSON.parse(execFileSync(python, currentPythonWorkerArguments(root, deploymentProvenance, { session: first.session, response: first.response.id, agent: agent.id }, currentReceipt), { cwd: root, env: currentPythonWorkerEnvironment(environment, deploymentProvenance), encoding: 'utf8', maxBuffer: 1024 * 1024, timeout: 300000, windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'] }));
    assert.equal(current.passed, true, 'current_python_sdk_gate_failed');
    assert.equal(current.new_model_runs, 0, 'current_python_sdk_must_reuse_canonical_trajectory');
    receipt.checks.current_python_sdk = { sdk: current.sdk, workerSource: deploymentProvenance.workerSource, runnerSource: config.source, receipt: currentReceipt, newModelRuns: 0 };
    receipt.pending = null;
    receipt.status = config['omit-compact'] ? 'api_increment_passed_compact_not_selected' : 'core_passed_full_acceptance_not_claimed'; persist();
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
