import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { createHash, randomUUID } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';
import OpenAI from 'openai';
import { archiveCompletedSession, assertCanonicalEvents, assertExecutionWorkspace, assertServingWorkerHealth,
  nativeAcceptanceModel, requireLiveDeploymentProvenance, tracedFetch, verifyWorkerIdentity } from './full-responses-sdk.mjs';
import * as policy from './paid-route-policy.mjs';

const origin = 'https://agents-staging.moesegfault.dev';
const expected = 'SERVER_KEEP|CUSTOM_KEEP|DEVELOPER_KEEP|right';
const hash = value => createHash('sha256').update(value).digest('hex');

export function contextCapacityRequest(agent, conversation) {
  return { model: agent, conversation, stream: true, max_output_tokens: 512, reasoning: { effort: 'none' },
    context_management: [{ type: 'compaction', compact_threshold: 1 }],
    instructions: 'The current response marker is CUSTOM_KEEP. It is the second final-answer field. Preserve the server marker and historical developer policy through compaction.',
    input: [{ role: 'developer', content: 'The developer marker is DEVELOPER_KEEP. After compaction, answer with exactly four pipe-separated fields: fixed server marker, current response marker, developer marker, authoritative decision. No explanation.' },
      { role: 'user', content: 'We initially selected left. Correction: right is the authoritative decision. Continue after compacting the old context.' }] };
}

export async function main(argv = process.argv.slice(2), environment = process.env) {
  const { values } = parseArgs({ args: argv, options: { source: { type: 'string' }, output: { type: 'string' },
    'current-python': { type: 'string' }, 'allow-model-calls': { type: 'boolean', default: false } }, strict: true });
  assert.match(values.source ?? '', /^[a-f0-9]{40}$/);
  assert.equal(values['allow-model-calls'], true, 'one_new_request_with_summary_and_answer_requires_explicit_selection');
  assert.ok(environment.MAGENTS_TEST_API_KEY);
  assert.match(environment.MAGENTS_TEST_TRACE_SEED ?? '', /^[1-9][0-9]*:[1-9][0-9]*$/);
  const root = path.resolve('.');
  const output = path.resolve(values.output ?? '.temp/stage-delivery/context-capacity-receipt.json');
  assert.ok(output.startsWith(path.join(root, '.temp') + path.sep));
  assert.equal(fs.existsSync(output), false, 'existing_intent_requires_original_operation_reconciliation');
  const python = path.resolve(values['current-python'] ?? '.cache/current-sdk-venv/Scripts/python.exe');
  assert.ok(python.startsWith(path.join(root, '.cache') + path.sep) && fs.existsSync(python));
  assertExecutionWorkspace(root, values.source, environment);
  const provenance = requireLiveDeploymentProvenance(environment, values.source, root);
  verifyWorkerIdentity(provenance, root, policy.verifyServingSource);
  const transport = tracedFetch(fetch, environment.MAGENTS_TEST_TRACE_SEED);
  const request = async (method, route, body, expectedStatuses = [200]) => {
    const response = await transport(origin + route, { method, headers: { Authorization: `Bearer ${environment.MAGENTS_TEST_API_KEY}`,
      ...(body === undefined ? {} : { 'Content-Type': 'application/json' }) },
    ...(body === undefined ? {} : { body: JSON.stringify(body) }), signal: AbortSignal.timeout(30000) });
    assert.ok(expectedStatuses.includes(response.status), `platform_status:${method}:${response.status}`);
    const bytes = await response.text(); return bytes ? JSON.parse(bytes) : null;
  };
  const client = new OpenAI({ apiKey: environment.MAGENTS_TEST_API_KEY, baseURL: origin + '/v1', maxRetries: 0, timeout: 300000, fetch: transport });
  const receipt = { execution: 'new_live_context_capacity_increment', source: values.source, provenance,
    scriptSha256: hash(fs.readFileSync(fileURLToPath(import.meta.url))), traceSeed: environment.MAGENTS_TEST_TRACE_SEED,
    run: randomUUID(), startedAt: new Date().toISOString(), status: 'running', phase: 'health', pending: null,
    resources: { agent: null, conversation: null, sessions: [], responses: [] }, checks: {}, submissions: 0,
    accounting: 'owned_D1_model_resource_and_capacity_postflight_required',
    notSelected: ['old_auto_window_generations', 'old_SQL_properties', 'old_compact_vision_or_Code_journeys', 'POST_replay'] };
  fs.mkdirSync(path.dirname(output), { recursive: true }); fs.writeFileSync(output, JSON.stringify(receipt, null, 2), { flag: 'wx' });
  const save = () => fs.writeFileSync(output, JSON.stringify(receipt, null, 2));
  try {
    assertServingWorkerHealth(await request('GET', '/healthz'), provenance);
    const approved = policy.assertAvailableDirectDeepSeek(await request('GET', '/v1/models'));
    const agent = await request('POST', '/v1/agents', { name: `context-capacity-${receipt.run}`, model: nativeAcceptanceModel(approved),
      instructions: 'The fixed server marker is SERVER_KEEP. It is the first final-answer field. Follow the protected developer policy and authoritative corrected decision.',
      tools: [], skills: [], workspace: false, compaction_percent: 80 }, [201]);
    receipt.resources.agent = agent.id; save();
    receipt.phase = 'conversation_create_intent'; receipt.pending = { kind: 'conversation', idempotencyKey: `conversation-${receipt.run}` }; save();
    const conversation = await client.conversations.create({}, { headers: { 'Idempotency-Key': receipt.pending.idempotencyKey } });
    receipt.resources.conversation = conversation.id; receipt.pending = null; save();
    const body = contextCapacityRequest(agent.id, conversation.id);
    const { model, input, instructions, reasoning } = body;
    const count = await client.responses.inputTokens.count({ model, input, instructions, reasoning });
    assert.ok(Number.isSafeInteger(count.input_tokens) && count.input_tokens > 1);
    receipt.checks.exactReadOnlyInputCount = count.input_tokens;
    receipt.phase = 'response_intent'; receipt.submissions++;
    receipt.pending = { kind: 'response', idempotencyKey: `response-${receipt.run}` }; save();
    const streamed = await client.responses.create(body, { headers: { 'Idempotency-Key': receipt.pending.idempotencyKey } }).withResponse();
    const session = streamed.response.headers.get('x-agents-session');
    const responseId = streamed.response.headers.get('x-agents-response');
    if (session) receipt.resources.sessions.push(session);
    if (responseId) receipt.resources.responses.push(responseId);
    save();
    const events = [];
    for await (const event of streamed.data) {
      events.push(event);
      if (event.response?.id && !receipt.resources.responses.includes(event.response.id)) {
        receipt.resources.responses.push(event.response.id); save();
      }
    }
    const response = events.find(event => event.type === 'response.completed')?.response;
    const terminal = events.find(event => ['response.completed', 'response.failed', 'response.incomplete'].includes(event.type));
    if (terminal) receipt.pending = null;
    save();
    assert.ok(response, 'new_context_response_not_completed');
    assertCanonicalEvents(events);
    const items = response.output.filter(item => item.type === 'compaction');
    assert.equal(items.length, 1, 'one_new_compaction_item_required');
    assert.match(items[0].encrypted_content, /^mcmp1\./);
    const text = response.output.filter(item => item.type === 'message').flatMap(item => item.content)
      .filter(part => part.type === 'output_text').map(part => part.text).join('').trim();
    assert.equal(text, expected, 'protected_server_custom_developer_policy_and_corrected_decision');
    assert.ok(session);
    policy.assertDirectDeepSeekPin(await request('GET', `/v1/sessions/${session}`), agent.id);
    const typed = JSON.parse(execFileSync(python, [path.join(root, 'tests/e2e/current_responses_sdk.py'), 'response-wire', '--input', '-'], {
      cwd: root, input: JSON.stringify({ response, events }), encoding: 'utf8', maxBuffer: 1024 * 1024,
      timeout: 30000, windowsHide: true, stdio: ['pipe', 'pipe', 'pipe'] }));
    assert.equal(typed.passed, true);
    receipt.checks.response = { id: response.id, session, sha256: hash(JSON.stringify(response)), sdk: typed.sdk, events: events.length };
    const retained = (await client.conversations.items.list(conversation.id, { order: 'asc' })).data;
    const prior = await client.responses.retrieve(response.id);
    await client.conversations.delete(conversation.id);
    assert.deepEqual((await client.conversations.items.list(conversation.id, { order: 'asc' })).data, retained);
    receipt.checks.metadataDeletePreservesItems = true; save();
    receipt.phase = 'explicit_purge_intent'; receipt.pending = { kind: 'purge', conversation: conversation.id }; save();
    const purged = await request('POST', `/v1/conversations/${conversation.id}/purge`);
    assert.equal(purged.purged, true); receipt.pending = null; save();
    await assert.rejects(client.conversations.items.list(conversation.id), error => error.status === 404);
    assert.deepEqual(await client.responses.retrieve(response.id), prior);
    const continuedCount = await client.responses.inputTokens.count({ model: agent.id, previous_response_id: response.id, input: 'Count only; do not generate.' });
    assert.ok(Number.isSafeInteger(continuedCount.input_tokens) && continuedCount.input_tokens > 0);
    receipt.checks.explicitPurgeAndIndependentResponse = true;
    receipt.checks.capacityPostflight = { conversation: conversation.id, response: response.id,
      expectedEscrow: 'converted', expectedRemainingBytes: 0, removedBodyAllocations: 'released/content_removed',
      retainedHeaderAllocations: true, observedFromClient: false };
    receipt.status = 'live_context_capacity_behavior_passed_financial_postflight_pending'; save();
  } catch (error) {
    receipt.status = 'failed'; receipt.failure = { status: error.status ?? null, type: error.name ?? 'Error', code: error.code ?? null };
    save(); throw error;
  } finally {
    const failures = [];
    if (!receipt.pending) {
      for (const session of receipt.resources.sessions) {
        try { await archiveCompletedSession(request, session); } catch { failures.push('session_archive'); }
      }
      if (!failures.length && receipt.resources.agent) {
        try { await request('DELETE', `/v1/agents/${receipt.resources.agent}`, undefined, [200, 204, 404]); } catch { failures.push('agent_delete'); }
      }
    } else failures.push('original_operation_reconciliation_required');
    receipt.cleanup = { failures, automaticReplay: false }; receipt.endedAt = new Date().toISOString(); save();
  }
  process.stdout.write(JSON.stringify({ status: receipt.status, receipt: path.relative(root, output), submissions: receipt.submissions }) + '\n');
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) await main();
