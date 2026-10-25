import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';

const models = new Set(['deepseek-flash', 'deepseek-v4-pro']);
const modelFields = new Set(['provider', 'model', 'context_window', 'max_output_tokens', 'credential_id', 'protocol']);

export function assertDirectDeepSeekModel(model, expectedModel = 'deepseek-flash') {
  assert.ok(models.has(expectedModel), 'paid_test_model_not_approved');
  assert.ok(model && typeof model === 'object' && !Array.isArray(model), 'paid_test_model_missing');
  assert.equal(model.provider, 'deepseek', 'paid_llm_test_must_use_direct_deepseek');
  assert.equal(model.model, expectedModel, 'paid_test_model_changed');
  assert.ok(model.credential_id == null, 'paid_test_must_not_override_platform_credential');
  assert.ok(model.protocol == null || ['openai', 'responses'].includes(model.protocol), 'paid_test_transport_changed');
  assert.ok(Object.keys(model).every(key => modelFields.has(key)), 'paid_test_model_contains_unverified_route_override');
  return { provider: model.provider, model: model.model, protocol: model.protocol ?? 'openai' };
}

export function assertDirectDeepSeekPin(session, agentId, expectedModel = 'deepseek-flash') {
  assert.ok(session && typeof session.id === 'string' && session.id.length > 0, 'paid_test_session_missing');
  assert.equal(session.agent_id, agentId, 'paid_test_session_wrong_agent');
  assert.ok(session.pinned && typeof session.pinned === 'object', 'paid_test_immutable_pin_missing');
  return { session: session.id, ...assertDirectDeepSeekModel(session.pinned.model, expectedModel) };
}

export function verifyDeepSeekResolver(sourceText) {
  assert.equal(typeof sourceText, 'string');
  const exactStaticRoute = /Provider::DeepSeek\s*=>\s*\(\s*ProviderProtocol::Openai\s*,\s*"https:\/\/api\.deepseek\.com"\.into\(\)\s*,\s*env\.secret\("DEEPSEEK_API_KEY"\)\?\.to_string\(\)\s*,?\s*\)/g;
  assert.equal([...sourceText.matchAll(exactStaticRoute)].length, 1, 'exact_source_direct_deepseek_route_not_verified');
  assert.ok(/match\s+model\.provider/.test(sourceText), 'provider_route_not_selected_from_model_pin');
  return { origin: 'https://api.deepseek.com', resolverSha256: createHash('sha256').update(sourceText).digest('hex'), evidence: 'exact_source_static_mapping_not_live_upstream_observation' };
}

export function verifyServingSource(build, root = process.cwd()) {
  assert.match(build ?? '', /^[a-f0-9]{40}$/, 'exact_serving_source_required');
  const source = execFileSync('git', ['show', `${build}:crates/agents-worker/src/provider_config.rs`], { cwd: root, encoding: 'utf8', maxBuffer: 128 * 1024, timeout: 10000, windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'] });
  return { source: build, ...verifyDeepSeekResolver(source) };
}

export function assertAvailableDirectDeepSeek(catalog, expectedModel = 'deepseek-flash') {
  assert.ok(Array.isArray(catalog?.available_platform_models), 'priced_platform_registry_missing');
  const matching = catalog.available_platform_models.filter(model => model.provider === 'deepseek' && model.model === expectedModel);
  assert.equal(matching.length, 1, 'direct_deepseek_model_not_uniquely_priced_and_available');
  assertDirectDeepSeekModel(matching[0], expectedModel);
  return matching[0];
}
export function assertDirectDeepSeekBounds(model, registered) {
  for (const field of ['context_window', 'max_output_tokens']) {
    assert.ok(Number.isSafeInteger(model[field]) && Number.isSafeInteger(registered[field]) && model[field] > 0 && model[field] <= registered[field], 'paid_test_model_bounds_escape_registry');
  }
}

export async function createPinnedSession(request, agent, sessions, expectedModel = 'deepseek-flash') {
  assertDirectDeepSeekModel(agent.model, expectedModel);
  const registered = assertAvailableDirectDeepSeek(await request('GET', '/v1/models'), expectedModel);
  assertDirectDeepSeekBounds(agent.model, registered);
  const session = await request('POST', `/v1/agents/${agent.id}/sessions`, {}, [201]);
  assert.ok(typeof session?.id === 'string', 'created_session_identity_missing');
  sessions.add(session.id);
  const canonical = await request('GET', `/v1/sessions/${session.id}`);
  assertDirectDeepSeekPin(canonical, agent.id, expectedModel);
  assertDirectDeepSeekBounds(canonical.pinned.model, registered);
  assert.equal(canonical.status, 'idle', 'preflight_session_not_idle');
  assert.ok(!canonical.run_id, 'preflight_session_already_running');
  return canonical;
}
