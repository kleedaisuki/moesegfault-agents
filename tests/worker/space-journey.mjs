import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { createBrowserPrincipal } from './browser-principal.mjs';

const origin = 'https://agents.test';
const digest = bytes => createHash('sha256').update(bytes).digest('hex');

async function dispatch(mf, apiKey, method, endpoint, { body, headers = {}, expected = 200 } = {}) {
  const response = await mf.dispatchFetch(origin + endpoint, {
    method,
    headers: { Authorization: `Bearer ${apiKey}`, 'CF-Connecting-IP': '192.0.2.31', ...headers },
    ...(body === undefined ? {} : { body }),
  });
  assert.equal(response.status, expected, `${method} ${endpoint}: unexpected HTTP status`);
  return response;
}

async function multipart(mf, apiKey, filename, bytes, purpose = 'user_data') {
  const form = new FormData();
  form.set('purpose', purpose);
  form.set('file', new Blob([bytes], { type: 'application/octet-stream' }), filename);
  const native = new Request(origin + '/v1/files', { method: 'POST', body: form });
  const body = Buffer.from(await native.arrayBuffer());
  const response = await dispatch(mf, apiKey, 'POST', '/v1/files', {
    body,
    headers: { 'Content-Type': native.headers.get('content-type'), 'Content-Length': String(body.length) },
  });
  return response.json();
}

async function filePages(request, key, order = 'desc', purpose = '') {
  let after;
  const found = [];
  const seen = new Set();
  for (let page = 0; page < 20; page++) {
    const query = new URLSearchParams({ limit: '1', order });
    if (purpose) query.set('purpose', purpose);
    if (after) query.set('after', after);
    const result = await request('GET', `/v1/files?${query}`, undefined, 200, key);
    assert.equal(result.object, 'list');
    assert.ok(result.data.length <= 1, 'The server must honor a bounded page size');
    for (const file of result.data) {
      assert.ok(!seen.has(file.id), 'Cursor pagination must not repeat files');
      seen.add(file.id);
      found.push(file);
    }
    if (!result.has_more) return found;
    assert.ok(result.data.length && result.last_id === result.data.at(-1).id);
    after = result.last_id;
  }
  assert.fail('Space file cursor did not terminate');
}

export async function runSpaceJourney({ mf, db, encryptionKey, request }) {
  const owner = await createBrowserPrincipal({ mf, db, encryptionKey, owner: 'space-worker-owner' });
  const foreign = await createBrowserPrincipal({ mf, db, encryptionKey, owner: 'space-worker-foreign' });
  const bucket = await mf.getR2Bucket('WORKSPACES');
  const firstSpace = await request('GET', '/v1/space', undefined, 200, owner.apiKey);
  const otherSpace = await request('GET', '/v1/space', undefined, 200, foreign.apiKey);
  assert.equal(firstSpace.object, 'space');
  assert.notEqual(firstSpace.id, otherSpace.id);
  assert.equal((await request('GET', '/v1/space', undefined, 200, owner.apiKey)).id, firstSpace.id);
  const renamedSpace = await request('PATCH', '/v1/space', { name: 'Space integration 雪' }, 200, owner.apiKey);
  assert.equal(renamedSpace.name, 'Space integration 雪');
  assert.equal(renamedSpace.id, firstSpace.id);

  const binary = Buffer.alloc(256 * 1280);
  for (let n = 0; n < binary.length; n++) binary[n] = n & 255;
  const payload = Buffer.concat([binary, Buffer.from('\0\r\nSpace 雪🧪\n')]);
  const main = await multipart(mf, owner.apiKey, 'evidence-雪.bin', payload);
  assert.equal(main.object, 'file');
  assert.equal(main.bytes, payload.length);
  assert.equal(main.filename, 'evidence-雪.bin');
  assert.equal(main.purpose, 'user_data');
  assert.equal(main.status, 'processed');
  assert.equal(main.sha256, digest(payload));
  assert.equal(main.object_key, undefined);
  assert.ok(Number.isSafeInteger(main.created_at) && main.created_at > 0);
  const stored = await db.prepare('SELECT account_id,object_key,status,bytes FROM space_files WHERE id=?').bind(main.id).first();
  assert.equal(stored.account_id, owner.owner);
  assert.equal(stored.status, 'processed');
  assert.equal(stored.bytes, payload.length);
  assert.deepEqual(Buffer.from(await (await bucket.get(stored.object_key)).arrayBuffer()), payload, 'Actual R2 bytes must match the multipart source');

  const downloaded = await dispatch(mf, owner.apiKey, 'GET', `/v1/files/${main.id}/content`);
  assert.equal(downloaded.headers.get('content-length'), String(payload.length));
  assert.equal(downloaded.headers.get('x-content-sha256'), digest(payload));
  assert.equal(downloaded.headers.get('etag'), `"${digest(payload)}"`);
  assert.equal(downloaded.headers.get('cache-control'), 'no-store');
  assert.ok(downloaded.headers.get('content-disposition').includes('filename*=UTF-8'));
  assert.deepEqual(Buffer.from(await downloaded.arrayBuffer()), payload);

  const rename = await request('PATCH', `/v1/space/files/${main.id}`, { filename: 'renamed.bin' }, 200, owner.apiKey);
  assert.equal(rename.filename, 'renamed.bin');
  assert.equal(rename.sha256, main.sha256);
  assert.equal(rename.bytes, main.bytes);
  assert.equal((await db.prepare('SELECT object_key FROM space_files WHERE id=?').bind(main.id).first()).object_key, stored.object_key, 'Metadata rename must not rewrite content');
  assert.equal((await request('GET', `/v1/files/${main.id}`, undefined, 200, owner.apiKey)).filename, 'renamed.bin');

  const generated = [main];
  for (const [bytes, purpose] of [[Buffer.from('first\n'), 'user_data'], [Buffer.from('second\0\n'), 'assistants'], [Buffer.alloc(0), 'user_data']]) {
    generated.push(await multipart(mf, owner.apiKey, 'same-name.txt', bytes, purpose));
  }
  assert.equal(new Set(generated.map(file => file.id)).size, generated.length, 'A display filename is not object identity');
  assert.equal(generated.at(-1).bytes, 0);
  assert.equal(generated.at(-1).sha256, digest(Buffer.alloc(0)));
  const descending = await filePages(request, owner.apiKey);
  const ascending = await filePages(request, owner.apiKey, 'asc');
  assert.deepEqual(ascending.map(file => file.id), descending.map(file => file.id).reverse(), 'Order and cursor predicates must agree, including timestamp ties');
  assert.deepEqual(new Set(descending.map(file => file.id)), new Set(generated.map(file => file.id)));
  const filtered = await filePages(request, owner.apiKey, 'desc', 'assistants');
  assert.deepEqual(filtered.map(file => file.id), [generated[2].id]);
  assert.equal((await request('GET', '/v1/space/files?limit=100', undefined, 200, owner.apiKey)).data.length, generated.length);

  for (const suffix of ['', '/content']) {
    await dispatch(mf, foreign.apiKey, 'GET', `/v1/files/${main.id}${suffix}`, { expected: 404 });
    const anonymous = await mf.dispatchFetch(`${origin}/v1/files/${main.id}${suffix}`);
    assert.equal(anonymous.status, 401);
    await anonymous.text();
  }
  await request('PATCH', `/v1/space/files/${main.id}`, { filename: 'hijacked.bin' }, 404, foreign.apiKey);
  await request('DELETE', `/v1/files/${main.id}`, undefined, 404, foreign.apiKey);
  assert.deepEqual((await request('GET', '/v1/files?limit=100', undefined, 200, foreign.apiKey)).data, []);
  assert.deepEqual(Buffer.from(await (await bucket.get(stored.object_key)).arrayBuffer()), payload);

  const beforeRejected = await request('GET', '/v1/space', undefined, 200, owner.apiKey);
  const bad = Buffer.from('bad digest is deterministic, not an uncertain PUT');
  const rejected = await mf.dispatchFetch(origin + '/v1/files?filename=bad-digest.bin', {
    method: 'POST', headers: { Authorization: `Bearer ${owner.apiKey}`, 'Content-Type': 'application/octet-stream',
      'Content-Length': String(bad.length), 'X-Content-Sha256': '0'.repeat(64) }, body: bad,
  });
  assert.ok([400, 422].includes(rejected.status), 'A rejected checksum must be reported as invalid client input');
  await rejected.text();
  const afterRejected = await request('GET', '/v1/space', undefined, 200, owner.apiKey);
  assert.equal(afterRejected.usage.bytes, beforeRejected.usage.bytes);
  assert.equal(afterRejected.usage.files, beforeRejected.usage.files);
  assert.equal(await db.prepare("SELECT COUNT(*) AS count FROM space_files WHERE account_id=? AND status='uploading'").bind(owner.owner).first('count'), 0, 'Rejected bytes cannot strand quota in an upload intent');
  assert.equal(await db.prepare("SELECT COUNT(*) AS count FROM usage_reservations WHERE account_id=? AND state='reserved'").bind(owner.owner).first('count'), 0, 'Rejected bytes cannot strand a new financial hold');

  for (const file of generated) {
    const deleted = await request('DELETE', `/v1/files/${file.id}`, undefined, 200, owner.apiKey);
    assert.deepEqual(deleted, { id: file.id, object: 'file', deleted: true });
    await request('GET', `/v1/files/${file.id}`, undefined, 404, owner.apiKey);
    await dispatch(mf, owner.apiKey, 'GET', `/v1/files/${file.id}/content`, { expected: 404 });
    const row = await db.prepare('SELECT object_key,status FROM space_files WHERE id=?').bind(file.id).first();
    assert.equal(row.status, 'deleted');
    assert.equal(await bucket.get(row.object_key), null);
    assert.equal(await db.prepare('SELECT status FROM resource_storage_objects WHERE object_key=?').bind(row.object_key).first('status'), 'removed');
  }
  const emptySpace = await request('GET', '/v1/space', undefined, 200, owner.apiKey);
  assert.equal(emptySpace.usage.bytes, 0);
  assert.equal(emptySpace.usage.files, 0);
  const charges = await db.prepare('SELECT DISTINCT l.resource FROM usage_ledger l JOIN usage_reservations r ON r.id=l.reservation_id WHERE r.account_id=?').bind(owner.owner).all();
  assert.ok(charges.results.some(row => row.resource === 'r2_operation'), 'Actual storage operations must be metered');
  assert.ok(charges.results.every(row => !['model', 'container'].includes(row.resource)), 'Files must not allocate inference or workspace execution');
  assert.equal(await db.prepare("SELECT COUNT(*) AS count FROM usage_reservations WHERE account_id=? AND state='reserved'").bind(owner.owner).first('count'), 0);
}
