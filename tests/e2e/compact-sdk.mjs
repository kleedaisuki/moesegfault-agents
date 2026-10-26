import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { createHash, randomUUID } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';
import OpenAI from 'openai';
import { archiveCompletedSession, assertExecutionWorkspace, assertServingWorkerHealth, nativeAcceptanceModel,
  requireLiveDeploymentProvenance, tracedFetch, verifyWorkerIdentity } from './full-responses-sdk.mjs';
import * as policy from './paid-route-policy.mjs';

const origin = 'https://agents-staging.moesegfault.dev';
const hash = value => createHash('sha256').update(value).digest('hex');
const expected = 'COMPACT_POLICY_OK_right';

export function compactRequest(agent) {
  return { model: agent, input: [
    { role: 'developer', content: `For the next answer, output exactly ${expected} and nothing else. Preserve this ongoing rule through compaction.` },
    { role: 'user', content: 'We selected the left path, then corrected the decision: the right path is authoritative.' },
    { role: 'assistant', content: [{ type: 'output_text', text: 'The corrected decision is right.', annotations: [] }] },
  ], instructions: 'Summarize the corrected decision, ongoing rule and fixed instructions concisely.' };
}

export function validateCompactedWindow(value) {
  assert.equal(value.object, 'response.compaction');
  assert.ok(/^cmp_/.test(value.id));
  assert.ok(value.output.length >= 2);
  assert.ok(value.output.slice(0, -1).every(item => item.type === 'message' && item.role === 'user'));
  assert.equal(value.output.at(-1).type, 'compaction');
  assert.match(value.output.at(-1).encrypted_content, /^mcmp1\./);
  for (const count of [value.usage.input_tokens, value.usage.output_tokens,
    value.usage.input_tokens_details.cached_tokens, value.usage.input_tokens_details.cache_write_tokens,
    value.usage.output_tokens_details.reasoning_tokens]) assert.ok(Number.isSafeInteger(count) && count >= 0);
  assert.equal(value.usage.total_tokens, value.usage.input_tokens + value.usage.output_tokens);
}

export async function main(argv = process.argv.slice(2), environment = process.env) {
  const { values } = parseArgs({ args: argv, options: { source: { type: 'string' }, output: { type: 'string' },
    'current-python': { type: 'string' }, 'allow-model-calls': { type: 'boolean', default: false } }, strict: true });
  assert.match(values.source ?? '', /^[a-f0-9]{40}$/);
  assert.equal(values['allow-model-calls'], true, 'one_new_compact_and_one_continuation_require_explicit_selection');
  assert.ok(environment.MAGENTS_TEST_API_KEY);
  assert.match(environment.MAGENTS_TEST_TRACE_SEED ?? '', /^[1-9][0-9]*:[1-9][0-9]*$/);
  const root = path.resolve('.');
  const output = path.resolve(values.output ?? '.temp/stage-delivery/compact-receipt.json');
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
  const receipt = { execution: 'new_live_default_compact_increment', source: values.source, provenance,
    scriptSha256: hash(fs.readFileSync(fileURLToPath(import.meta.url))), traceSeed: environment.MAGENTS_TEST_TRACE_SEED,
    run: randomUUID(), startedAt: new Date().toISOString(), status: 'running', phase: 'health', pending: null,
    resources: { agent: null, sessions: [], responses: [], operations: [] }, checks: {}, submissions: 0,
    accounting: 'account_operation_and_session_postflight_required', notSelected: ['old_usage_probes', 'POST_replay', 'old_core_regressions'] };
  fs.mkdirSync(path.dirname(output), { recursive: true }); fs.writeFileSync(output, JSON.stringify(receipt, null, 2), { flag: 'wx' });
  const save = () => fs.writeFileSync(output, JSON.stringify(receipt, null, 2));
  try {
    assertServingWorkerHealth(await request('GET', '/healthz'), provenance);
    const approved = policy.assertAvailableDirectDeepSeek(await request('GET', '/v1/models'));
    const agent = await request('POST', '/v1/agents', { name: `compact-${receipt.run}`, model: nativeAcceptanceModel(approved),
      instructions: 'Follow the protected ongoing decision and never invent a tool result.', tools: [], skills: [], workspace: false, compaction_percent: 80 }, [201]);
    receipt.resources.agent = agent.id; save();
    policy.assertDirectDeepSeekModel(agent.model); policy.assertDirectDeepSeekBounds(agent.model, approved);
    const body = compactRequest(agent.id);
    receipt.phase = 'compact_intent'; receipt.submissions++;
    receipt.pending = { kind: 'compact', idempotencyKey: `compact-${receipt.run}`, bodySha256: hash(JSON.stringify(body)) }; save();
    const raw = await client.responses.compact(body, { headers: { 'Idempotency-Key': receipt.pending.idempotencyKey } }).asResponse();
    const operation = raw.headers.get('x-agents-operation');
    if (operation) receipt.resources.operations.push(operation);
    save();
    const window = await raw.json();
    receipt.pending = null; save();
    validateCompactedWindow(window);
    // SDK 3.37's compact output annotation is assistant-only despite its retained
    // user-message contract. Change this union only, never the actual wire roles.
    const check = 'import json,sys\nfrom typing import Union\nfrom openai.types.responses.compacted_response import CompactedResponse\nfrom openai.types.responses.response_input_message_item import ResponseInputMessageItem\nfrom openai.types.responses.response_compaction_item import ResponseCompactionItem\nclass Window(CompactedResponse):\n output: list[Union[ResponseInputMessageItem, ResponseCompactionItem]]\nWindow.model_validate(json.load(sys.stdin), strict=True)';
    execFileSync(python, ['-c', check], { input: JSON.stringify(window), encoding: 'utf8', maxBuffer: 1024 * 1024,
      timeout: 30000, windowsHide: true, stdio: ['pipe', 'pipe', 'pipe'] });
    receipt.checks.compact = { id: window.id, operation, sha256: hash(JSON.stringify(window)), usage: window.usage,
      retainedUsers: window.output.length - 1, sdkUnionCorrection: 'ResponseInputMessageItem|ResponseCompactionItem' }; save();
    receipt.phase = 'whole_window_continuation_intent'; receipt.submissions++;
    receipt.pending = { kind: 'response', idempotencyKey: `resume-${receipt.run}` }; save();
    const resumed = await client.responses.create({ model: agent.id, input: [...window.output, { role: 'user', content: 'Continue.' }],
      max_output_tokens: 64, reasoning: { effort: 'none' } }, { headers: { 'Idempotency-Key': receipt.pending.idempotencyKey } }).withResponse();
    const session = resumed.response.headers.get('x-agents-session');
    if (session) receipt.resources.sessions.push(session);
    if (resumed.data.id) receipt.resources.responses.push(resumed.data.id);
    if (['completed', 'failed', 'incomplete', 'cancelled'].includes(resumed.data.status)) receipt.pending = null;
    save();
    assert.equal(resumed.data.status, 'completed');
    const text = resumed.data.output.filter(item => item.type === 'message').flatMap(item => item.content)
      .filter(part => part.type === 'output_text').map(part => part.text).join('').trim();
    assert.equal(text, expected);
    assert.ok(session);
    policy.assertDirectDeepSeekPin(await request('GET', `/v1/sessions/${session}`), agent.id);
    receipt.checks.continuation = { id: resumed.data.id, session, sha256: hash(JSON.stringify(resumed.data)), protectedDecision: true };
    receipt.status = 'live_compact_behavior_passed_financial_postflight_pending'; save();
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
