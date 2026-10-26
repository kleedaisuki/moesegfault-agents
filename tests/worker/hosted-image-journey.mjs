import assert from 'node:assert/strict';

export const hostedImagePng = 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aOSkAAAAASUVORK5CYII=';

// Install this profile and synthetic secret only in the controlled Worker test.
export const hostedImageProfile = {
  version: 1,
  profiles: [{ api: 'openrouter', base_url: 'https://openrouter.ai/api/v1',
    model: 'bytedance-seed/seedream-5-0-flash', public_model: null,
    capabilities: { streaming: false, parameters: {
      resolution: { type: 'enum', values: ['1K', '2K'] },
      size: { type: 'boolean' }, output_format: { type: 'enum', values: ['png', 'jpeg', 'webp'] },
    }, max_references: 14, mask: false },
    router_endpoint: { provider_slug: 'seed', provider_tag: 'seed', passthrough_parameters: [] },
    asset_hosts: [], price_revision: 'seedream-catalog-2026-10-25',
    reservation_microusd: 18000, platform_funded: true }],
};

export function createHostedImageProviderFixture() {
  const state = { calls: [], mode: 'inline' };
  return { state, async handle(request) {
    const url = new URL(request.url);
    if (url.origin !== 'https://openrouter.ai' || url.pathname !== '/api/v1/images') return undefined;
    assert.equal(request.method, 'POST');
    assert.equal(request.headers.get('authorization'), 'Bearer synthetic-platform-image-key');
    assert.ok(request.headers.get('idempotency-key'));
    const body = await request.json();
    assert.equal(body.model, 'bytedance-seed/seedream-5-0-flash');
    assert.deepEqual(body.provider, { only: ['seed'], allow_fallbacks: false });
    assert.equal(body.n, 1);
    assert.equal(body.response_format, undefined);
    state.calls.push(body);
    if (state.mode === 'disconnect') throw new Error('Synthetic image result lost after dispatch');
    return Response.json({ id: 'image-provider-known', created: 123,
      data: [{ b64_json: hostedImagePng, media_type: 'image/png' }],
      usage: { cost: 0.018 } });
  } };
}

export function createHostedByokImageFixture() {
  const state = { calls: [] };
  return { state, baseUrl: 'https://owned-image-service.example.com/v1',
    model: 'unknown-selfhost-model', key: 'synthetic-owned-image-key',
    async handle(request) {
      const url = new URL(request.url);
      if (url.hostname !== 'owned-image-service.example.com') return undefined;
      assert.equal(url.pathname, '/v1/images/generations');
      assert.equal(request.method, 'POST');
      assert.equal(request.headers.get('authorization'), 'Bearer synthetic-owned-image-key',
        'BYOK image operation must never fall back to platform image credentials');
      assert.equal(request.headers.get('cookie'), null);
      const body = await request.json();
      assert.deepEqual(Object.keys(body).sort(), ['model', 'n', 'prompt']);
      assert.equal(body.model, 'unknown-selfhost-model');
      assert.equal(body.n, 1);
      state.calls.push(body);
      return Response.json({ id: 'byok-image-known', created: 123,
        data: [{ b64_json: hostedImagePng, media_type: 'image/png' }],
        usage: { cost: 1.234 } });
    },
  };
}

