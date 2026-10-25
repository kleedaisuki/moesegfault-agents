import assert from 'node:assert/strict';
import test from 'node:test';
import { assertDirectDeepSeekModel, assertDirectDeepSeekPin, verifyDeepSeekResolver, createPinnedSession, assertAvailableDirectDeepSeek } from './paid-route-policy.mjs';

const direct = () => ({ provider: 'deepseek', model: 'deepseek-flash', protocol: 'responses' });
const source = 'match model.provider { Provider::DeepSeek => (ProviderProtocol::Openai, "https://api.deepseek.com".into(), env.secret("DEEPSEEK_API_KEY")?.to_string(),), }';
test('explicit pin plus static source proof keeps origin claim precise', () => {
  assert.equal(assertDirectDeepSeekPin({ id: 'session', agent_id: 'agent', pinned: { model: direct() } }, 'agent').provider, 'deepseek');
  assert.equal(verifyDeepSeekResolver(source).origin, 'https://api.deepseek.com');
  assert.equal(verifyDeepSeekResolver(source).evidence, 'exact_source_static_mapping_not_live_upstream_observation');
});
for (const [name, model] of [
  ['OpenRouter language route', { ...direct(), provider: 'openrouter' }],
  ['credential override', { ...direct(), credential_id: 'owned-but-unapproved' }],
  ['URL override', { ...direct(), base_url: 'https://other.invalid' }],
  ['unexpected model', { ...direct(), model: 'fallback' }],
  ['transport override', { ...direct(), protocol: 'anthropic' }],
]) test(`reject ${name} before model submission`, () => assert.throws(() => assertDirectDeepSeekModel(model)));
test('policy is not a product-wide prohibition on image service routes', () => {
  const pin = { id: 'session', agent_id: 'agent', pinned: { model: direct(), tool_services: { image: { provider: 'openrouter', model: 'bytedance-seed/seedream-5-0-flash' } } } };
  assertDirectDeepSeekPin(pin, 'agent');
});
test('wrong pin identity and missing immutable pin reject', () => {
  assert.throws(() => assertDirectDeepSeekPin({ id: 's', agent_id: 'other', pinned: { model: direct() } }, 'agent'));
  assert.throws(() => assertDirectDeepSeekPin({ id: 's', agent_id: 'agent', model: direct() }, 'agent'));
});
test('changed static origin, key or selection rejects', () => {
  for (const wrong of [source.replace('api.deepseek.com', 'openrouter.ai'), source.replace('DEEPSEEK_API_KEY', 'OTHER_KEY'), source.replace('model.provider', 'default.provider')]) assert.throws(() => verifyDeepSeekResolver(wrong));
});
test('pin-only creation is checked before caller can dispatch inference', async () => {
  const calls = [], sessions = new Set();
  const request = async (method, endpoint) => {
    calls.push([method, endpoint]);
    if (endpoint === '/v1/models') return { available_platform_models: [direct()] };
    return method === 'POST' ? { id: 's' } : { id: 's', agent_id: 'a', status: 'idle', pinned: { model: direct() } };
  };
  assert.equal((await createPinnedSession(request, { id: 'a', model: direct() }, sessions)).id, 's');
  assert.deepEqual(calls, [['GET', '/v1/models'], ['POST', '/v1/agents/a/sessions'], ['GET', '/v1/sessions/s']]);
  assert.deepEqual([...sessions], ['s']);
});
test('wrong resolved pin leaves zero model calls and retains cleanup identity', async () => {
  const calls = [], sessions = new Set();
  const request = async (method, endpoint) => { calls.push(endpoint); if (endpoint === '/v1/models') return { available_platform_models: [direct()] }; return method === 'POST' ? { id: 's' } : { id: 's', agent_id: 'a', status: 'idle', pinned: { model: { provider: 'openrouter', model: 'deepseek-flash' } } }; };
  await assert.rejects(createPinnedSession(request, { id: 'a', model: direct() }, sessions));
  assert.equal(calls.some(endpoint => endpoint.includes('/responses') || endpoint.includes('/chat/completions')), false);
  assert.equal(sessions.has('s'), true);
});

test('missing, unpriced or duplicate registry model prevents admission', () => {
  assertAvailableDirectDeepSeek({ available_platform_models: [direct()] });
  for (const catalog of [{ available_models: [direct()] }, { available_platform_models: [] }, { available_platform_models: [direct(), direct()] }]) assert.throws(() => assertAvailableDirectDeepSeek(catalog));
});