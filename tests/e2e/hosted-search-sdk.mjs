import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { createHash, randomUUID } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';
import OpenAI from 'openai';
import { archiveCompletedSession, assertCanonicalEvents, assertExecutionWorkspace, assertServingWorkerHealth, countRootWithoutAdmission, nativeAcceptanceModel, requireLiveDeploymentProvenance, tracedFetch, verifyWorkerIdentity } from './full-responses-sdk.mjs';
import * as policy from './paid-route-policy.mjs';

const origin = 'https://agents-staging.moesegfault.dev';
const hash = value => createHash('sha256').update(value).digest('hex');
const digest = value => hash(JSON.stringify(value));
const include = ['web_search_call.action.sources'];

export function searchRequest(agent) {
  return { model: agent, input: 'Use web_search exactly once to find the official Rust Book page about ownership. Do not open or find any page. Then answer in one short sentence with the returned official source URL.',
    tools: [{ type: 'web_search', search_context_size: 'low', filters: { allowed_domains: ['doc.rust-lang.org'] } }],
    include, max_tool_calls: 1, max_output_tokens: 256, reasoning: { effort: 'none' } };
}

export function verifySearchPriceSource(source) {
  assert.match(source, /pub const PRICE_REVISION: &str = "deepseek-direct-search-fetch-2026-10-26";/, 'actual_direct_search_price_revision_required');
  assert.match(source, /pub const PAID_UNIT_MICRODOLLARS: u64 = 750;/, 'direct_search_must_reserve_750_not_harness_promotion');
  return { successfulRequestMicroUSD: 750, priceRevision: 'deepseek-direct-search-fetch-2026-10-26', sourceSha256: hash(source) };
}

export function summarizeSearch(response, events) {
  assert.equal(response.status, 'completed');
  const calls = response.output.filter(item => item.type === 'web_search_call');
  assert.equal(calls.length, 1, 'exactly_one_real_search_call_required');
  const item = calls[0];
  assert.equal(item.status, 'completed', 'search_service_not_completed');
  assert.equal(item.action.type, 'search', 'live_increment_does_not_authorize_open_or_find');
  assert.ok(typeof item.action.query === 'string' && item.action.query.length > 0);
  assert.ok(item.action.sources?.length > 0, 'actual_native_sources_required');
  const sources = item.action.sources.map(source => {
    assert.equal(source.type, 'url');
    const url = new URL(source.url);
    assert.equal(url.protocol, 'https:'); assert.equal(url.hostname, 'doc.rust-lang.org');
    assert.equal(url.username, ''); assert.equal(url.password, '');
    return { hostname: url.hostname, pathname: url.pathname, urlSha256: hash(source.url) };
  });
  const phases = events.filter(event => event.item_id === item.id).map(event => event.type);
  assert.deepEqual(phases, ['response.web_search_call.in_progress', 'response.web_search_call.searching', 'response.web_search_call.completed']);
  const text = response.output.filter(item => item.type === 'message').flatMap(item => item.content)
    .filter(part => part.type === 'output_text').map(part => part.text).join('');
  assert.ok(text.length > 0, 'outer_answer_missing');
  assert.ok(!JSON.stringify(response).includes('__agents_web_search'), 'private_tool_alias_leaked');
  return { itemId: item.id, action: item.action.type, querySha256: hash(item.action.query), sources,
    phases, answerSha256: hash(text), answerMentionsKnownSource: item.action.sources.some(source => text.includes(source.url)) };
}

function configuration(argv, environment, root) {
  const { values } = parseArgs({ args: argv, options: { source: { type: 'string' }, output: { type: 'string' },
    'current-python': { type: 'string' }, 'allow-paid-search': { type: 'boolean', default: false } }, strict: true });
  assert.match(values.source ?? '', /^[a-f0-9]{40}$/, 'exact_search_assembly_source_required');
  assert.equal(values['allow-paid-search'], true, 'single_new_paid_search_gate_required');
  assert.ok(environment.MAGENTS_TEST_API_KEY, 'owned_platform_key_missing');
  assert.match(environment.MAGENTS_TEST_TRACE_SEED ?? '', /^[1-9][0-9]*:[1-9][0-9]*$/);
  const output = path.resolve(values.output ?? '.temp/stage-delivery/hosted-search-receipt.json');
  assert.ok(output.startsWith(path.join(root, '.temp') + path.sep), 'receipt_must_stay_in_project_temp');
  assert.equal(fs.existsSync(output), false, 'existing_search_intent_requires_reconciliation_not_replay');
  const python = path.resolve(values['current-python'] ?? '.cache/current-sdk-venv/Scripts/python.exe');
  assert.ok(python.startsWith(path.join(root, '.cache') + path.sep) && fs.existsSync(python), 'project_current_python_required');
  return { source: values.source, output, python };
}

async function cleanup(request, receipt, save) {
  const failures = [];
  if (receipt.pending) {
    receipt.cleanup = { failures: [{ kind: 'ambiguous_response_reconcile_original_id_only' }], automaticReplay: false }; save();
    return;
  }
  for (const session of receipt.resources.sessions) {
    try { await archiveCompletedSession(request, session); }
    catch (error) { failures.push({ kind: 'session_archive', id: session, status: error.status ?? null }); }
  }
  if (!failures.length && receipt.resources.agent) {
    try { await request('DELETE', `/v1/agents/${receipt.resources.agent}`, undefined, [200, 204, 404]); }
    catch (error) { failures.push({ kind: 'agent_delete', status: error.status ?? null }); }
  }
  receipt.cleanup = { failures, canonicalResponsesRetained: true }; save();
}

export async function main(argv = process.argv.slice(2), environment = process.env) {
  const root = path.resolve('.'), config = configuration(argv, environment, root);
  assertExecutionWorkspace(root, config.source, environment);
  const provenance = requireLiveDeploymentProvenance(environment, config.source, root);
  verifyWorkerIdentity(provenance, root, policy.verifyServingSource);
  const price = verifySearchPriceSource(execFileSync('git', ['show', `${provenance.workerSource}:crates/agents-worker/src/search_deepseek.rs`],
    { cwd: root, encoding: 'utf8', maxBuffer: 256 * 1024, windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'] }));
  const transport = tracedFetch(fetch, environment.MAGENTS_TEST_TRACE_SEED);
  const request = async (method, route, body, expected = [200]) => {
    const response = await transport(origin + route, { method, headers: { Authorization: `Bearer ${environment.MAGENTS_TEST_API_KEY}`,
      ...(body === undefined ? {} : { 'Content-Type': 'application/json' }) },
    ...(body === undefined ? {} : { body: JSON.stringify(body) }), signal: AbortSignal.timeout(30000) });
    assert.ok(expected.includes(response.status), `platform_status:${method}:${response.status}`);
    const bytes = await response.text(); return bytes ? JSON.parse(bytes) : null;
  };
  const client = new OpenAI({ apiKey: environment.MAGENTS_TEST_API_KEY, baseURL: origin + '/v1', maxRetries: 0, timeout: 300000, fetch: transport });
  const receipt = { execution: 'new_live_platform_search_increment', source: config.source, provenance, price,
    scriptSha256: hash(fs.readFileSync(fileURLToPath(import.meta.url))), traceSeed: environment.MAGENTS_TEST_TRACE_SEED,
    startedAt: new Date().toISOString(), run: randomUUID(), phase: 'health', status: 'running', submissions: 0, pending: null,
    resources: { agent: null, sessions: [], responses: [] }, checks: {}, cleanup: {},
    accounting: 'actual_one_successful_search_request_750microusd_and_session_postflight_required',
    notSelected: ['Code', 'image_generation', '4g', 'accepted_POST_replay', 'old_core_eight_calls', 'operator_usage_probe'] };
  fs.mkdirSync(path.dirname(config.output), { recursive: true });
  fs.writeFileSync(config.output, JSON.stringify(receipt, null, 2), { flag: 'wx' });
  const save = () => fs.writeFileSync(config.output, JSON.stringify(receipt, null, 2));
  try {
    assertServingWorkerHealth(await request('GET', '/healthz'), provenance);
    const approved = policy.assertAvailableDirectDeepSeek(await request('GET', '/v1/models'));
    receipt.phase = 'agent_create_intent'; save();
    const agent = await request('POST', '/v1/agents', { name: `hosted-search-${receipt.run}`, model: nativeAcceptanceModel(approved),
      instructions: 'Use the declared web search exactly once when asked, then answer briefly using the returned official source. Never claim a tool executed without its result.',
      workspace: false, tools: [], skills: [], tool_services: { web_search: { provider: 'platform_deepseek' } }, compaction_percent: 80 }, [201]);
    receipt.resources.agent = agent.id; save();
    const initial = await policy.createPinnedSession(request, agent, { add: id => { receipt.resources.sessions.push(id); save(); } });
    const body = searchRequest(agent.id);
    receipt.phase = 'search_admission_without_inference'; save();
    receipt.checks.prepaidSearchAdmission = (await countRootWithoutAdmission(client, request, initial.id, body)).evidence;
    receipt.submissions = 1; receipt.phase = 'single_search_response_intent';
    receipt.pending = { idempotencyKey: `hosted-search-${receipt.run}`, session: initial.id, bodySha256: digest(body) }; save();
    const result = await client.responses.create({ ...body, stream: true }, { headers: {
      'X-Agents-Session': initial.id, 'Idempotency-Key': receipt.pending.idempotencyKey } }).withResponse();
    const responseId = result.response.headers.get('x-agents-response');
    if (responseId) receipt.resources.responses.push(responseId);
    assert.equal(result.response.headers.get('x-agents-session'), initial.id); save();
    const events = [];
    for await (const event of result.data) {
      events.push(event);
      if (event.type === 'response.created' && !receipt.resources.responses.includes(event.response.id)) { receipt.resources.responses.push(event.response.id); save(); }
      if (['response.completed', 'response.failed', 'response.incomplete'].includes(event.type) && receipt.resources.responses.includes(event.response?.id)) { receipt.pending = null; save(); }
    }
    const response = assertCanonicalEvents(events);
    assert.equal(response.id, responseId);
    receipt.phase = 'canonical_search_terminal'; receipt.checks.search = summarizeSearch(response, events); save();
    policy.assertDirectDeepSeekPin(await request('GET', `/v1/sessions/${initial.id}`), agent.id);
    const typed = JSON.parse(execFileSync(config.python, [path.join(root, 'tests/e2e/current_responses_sdk.py'), 'response-wire', '--input', '-'], {
      cwd: root, input: JSON.stringify({ response, events }), encoding: 'utf8', maxBuffer: 1024 * 1024,
      timeout: 30000, windowsHide: true, stdio: ['pipe', 'pipe', 'pipe'] }));
    assert.equal(typed.passed, true);
    receipt.checks.currentPythonWire = { sdk: typed.sdk, actualPayloads: true, newModelRuns: 0 };
    const retrieved = await client.responses.retrieve(response.id, { include });
    assert.deepEqual(retrieved.output, response.output);
    const replay = [];
    for await (const event of await client.responses.retrieve(response.id, { stream: true, include })) replay.push(event);
    assert.deepEqual(replay, events, 'GET_event_replay_changed_search_canonical_wire');
    receipt.checks.canonicalRetrieval = { jsonOutputSha256: digest(retrieved.output), getEventsSha256: digest(replay), originalEvents: events.length, acceptedPostReplay: false };
    receipt.status = 'live_search_behavior_passed_financial_postflight_pending'; save();
  } catch (error) {
    receipt.status = 'failed'; receipt.failure = { name: error.name, status: error.status ?? null }; save(); throw error;
  } finally {
    await cleanup(request, receipt, save); receipt.endedAt = new Date().toISOString(); save();
    if (receipt.cleanup.failures.length) process.exitCode = 1;
  }
  process.stdout.write(JSON.stringify({ status: receipt.status, submissions: receipt.submissions, receipt: config.output }) + '\n');
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main().catch(error => { process.stderr.write(JSON.stringify({ result: 'failed', name: error.name, status: error.status ?? null }) + '\n'); process.exitCode = 1; });
}
