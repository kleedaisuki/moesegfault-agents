import assert from 'node:assert/strict';

export const publicImageOrigin = 'https://public-image-fixture.example.com';
export const publicImagePng = Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+ip1sAAAAASUVORK5CYII=', 'base64');

// Only Miniflare's controlled outbound boundary imports this fixture. The
// production Worker uses native public-Internet Fetch, not this test service.
export function createPublicImageInputFixture() {
  const calls = [];
  return {
    calls,
    async handle(request) {
      const url = new URL(request.url);
      if (url.hostname !== new URL(publicImageOrigin).hostname) return undefined;
      assert.equal(request.method, 'GET');
      for (const header of ['authorization', 'cookie', 'proxy-authorization', 'traceparent', 'tracestate', 'baggage', 'x-api-key']) {
        assert.equal(request.headers.get(header), null, `Public image fetch leaked ${header}`);
      }
      // The outbound service receives a new Request; client redirect mode is
      // verified by the redirect-target call counter, not this Request's default.
      calls.push({ path: url.pathname, query: url.search });
      if (url.pathname === '/redirect-public') return new Response(null, { status: 302, headers: { Location: publicImageOrigin + '/redirect-target' } });
      if (url.pathname === '/redirect-private') return new Response(null, { status: 307, headers: { Location: 'https://127.0.0.1/metadata' } });
      if (url.pathname === '/oversize') return new Response(publicImagePng, { headers: { 'Content-Type': 'image/png', 'Content-Length': String(17 * 1024 * 1024) } });
      if (url.pathname === '/gif') return new Response(Buffer.from('GIF89a'), { headers: { 'Content-Type': 'image/gif' } });
      if (url.pathname === '/wrong-mime') return new Response(publicImagePng, { headers: { 'Content-Type': 'image/jpeg' } });
      if (url.pathname === '/not-an-image') return new Response('not an image', { headers: { 'Content-Type': 'image/png' } });
      if (url.pathname === '/chunked-oversize') {
        let sent = 0;
        return new Response(new ReadableStream({ pull(controller) {
          if (sent++ === 17) { controller.close(); return; }
          controller.enqueue(new Uint8Array(1024 * 1024));
        } }), { headers: { 'Content-Type': 'image/png' } });
      }
      assert.ok(['/image.png', '/redirect-target'].includes(url.pathname), `Unknown image fixture ${url.pathname}`);
      return new Response(publicImagePng, { headers: { 'Content-Type': 'image/png', 'Content-Length': String(publicImagePng.length) } });
    },
  };
}

/**
 * Drive the actual compiled Rust Worker through its normal Responses ingress.
 * submit(url) must dispatch a fresh request to that Worker and return Response;
 * modelCalls() observes the controlled provider, not a reimplemented validator.
 * The caller supplies its ordinary image or computer-result request template.
 */
export async function runPublicImageInputJourney({ fixture, submit, modelCalls }) {
  const before = modelCalls();
  const signed = publicImageOrigin + '/image.png?signature=opaque';
  const good = await submit(signed);
  assert.equal(good.status, 200, await good.clone().text());
  const goodBody = await good.json();
  assert.equal(modelCalls(), before + 1, 'Verified public image reaches one model action');
  assert.deepEqual(fixture.calls.at(-1), { path: '/image.png', query: '?signature=opaque' });
  const receipts = [{ kind: 'public-image', response_id: goodBody.id }];
  for (const path of ['/redirect-public', '/redirect-private', '/oversize', '/chunked-oversize', '/gif', '/wrong-mime', '/not-an-image']) {
    const callsBefore = modelCalls();
    const rejected = await submit(publicImageOrigin + path);
    assert.ok(rejected.status >= 400 && rejected.status < 500, `${path}: ${rejected.status} ${await rejected.text()}`);
    assert.equal(modelCalls(), callsBefore, `${path}: rejected before paid inference`);
    receipts.push({ kind: path.slice(1), status: rejected.status });
  }
  assert.equal(fixture.calls.filter(call => call.path === '/redirect-target').length, 0, 'Redirects are never followed, including public-to-public');
  for (const url of ['https://127.0.0.1/a.png', 'https://[::1]/a.png', 'https://169.254.169.254/a.png',
    'https://localhost/a.png', 'https://host.internal/a.png', 'https://user:secret@public-image-fixture.example.com/a.png']) {
    const fetchesBefore = fixture.calls.length;
    const callsBefore = modelCalls();
    const rejected = await submit(url);
    assert.ok(rejected.status >= 400 && rejected.status < 500, `Private input accepted: ${url}`);
    assert.equal(fixture.calls.length, fetchesBefore, 'Invalid URLs never reach controlled outbound');
    assert.equal(modelCalls(), callsBefore, 'Private URL rejected before model');
    receipts.push({ kind: 'private-origin', status: rejected.status });
  }
  return receipts;
}
