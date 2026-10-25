import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdir, writeFile } from 'node:fs/promises';
import { createBrowserPrincipal } from './browser-principal.mjs';

const origin = 'https://agents.test';
const digest = bytes => createHash('sha256').update(bytes).digest('hex');

async function dispatch(mf, apiKey, method, endpoint, { body, headers = {}, expected = 200 } = {}) {
  const response = await mf.dispatchFetch(origin + endpoint, {
    method,
    headers: { Authorization: `Bearer ${apiKey}`, 'CF-Connecting-IP': '192.0.2.31', ...headers },
    ...(body === undefined ? {} : { body }),
  });
  if (response.status !== expected) await response.arrayBuffer();
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

async function classBReads(db, owner) {
  const row = await db.prepare("SELECT CAST(COALESCE(SUM(l.quantity),0) AS TEXT) AS operations,COUNT(*) AS entries FROM usage_ledger l JOIN usage_reservations r ON r.id=l.reservation_id WHERE r.account_id=? AND l.resource='r2_operation' AND l.unit='class_b'").bind(owner).first();
  return { operations: BigInt(row.operations), entries: row.entries };
}

async function readWithAccounting({ mf, db, owner, file, headers = {}, expected, bytes, contentRange, operations }) {
  const before = await classBReads(db, owner.owner);
  const response = await dispatch(mf, owner.apiKey, 'GET', `/v1/files/${file.id}/content`, { headers, expected });
  const body = Buffer.from(await response.arrayBuffer());
  const after = await classBReads(db, owner.owner);
  assert.equal(after.operations - before.operations, BigInt(operations), `${expected} must meter only the actual R2 read`);
  assert.equal(after.entries - before.entries, operations, 'A conditional or rejected request must not append a fabricated R2 read event');
  assert.equal(response.headers.get('etag'), `"${file.sha256}"`);
  assert.equal(response.headers.get('x-content-sha256'), file.sha256, 'The digest names the complete immutable object, never the byte slice');
  assert.equal(response.headers.get('cache-control'), 'no-store');
  assert.equal(response.headers.get('content-range'), contentRange ?? null);
  if (expected === 416) {
    assert.ok(body.length > 0 && body.length < 256, 'An invalid range returns a bounded standard API diagnostic, not file bytes');
    assert.equal(JSON.parse(body.toString()).error.code, 'invalid_range');
  } else {
    assert.deepEqual(body, bytes);
  }
  if (expected === 200 || expected === 206) {
    assert.equal(response.headers.get('content-length'), String(body.length));
  }
  if (expected === 206) {
    assert.equal(response.headers.get('accept-ranges'), 'bytes');
  }
  return body.length;
}

async function bandwidthJourney({ mf, db, owner, foreign, main, empty, payload }) {
  const etag = `"${main.sha256}"`;
  const context = { mf, db, owner, file: main };
  const fullBytes = await readWithAccounting({ ...context, expected: 200, bytes: payload, operations: 1 });
  const partialBytes = await readWithAccounting({ ...context, headers: { Range: 'bytes=13-1020' },
    expected: 206, bytes: payload.subarray(13, 1021), contentRange: `bytes 13-1020/${payload.length}`, operations: 1 });
  const notModifiedBytes = await readWithAccounting({ ...context, headers: { 'If-None-Match': etag },
    expected: 304, bytes: Buffer.alloc(0), operations: 0 });

  for (const value of [`W/${etag}`, '*', `"not-the-file", W/${etag}`]) {
    await readWithAccounting({ ...context, headers: { 'If-None-Match': value },
      expected: 304, bytes: Buffer.alloc(0), operations: 0 });
  }
  await readWithAccounting({ ...context, headers: { 'If-None-Match': etag, Range: 'bytes=invalid-range' },
    expected: 304, bytes: Buffer.alloc(0), operations: 0 });
  await readWithAccounting({ ...context, headers: { 'If-None-Match': '"different-file"' },
    expected: 200, bytes: payload, operations: 1 });

  const valid = [
    ['bytes=327600-', 327600, payload.length - 1],
    ['bytes=-17', payload.length - 17, payload.length - 1],
    [`bytes=0-${payload.length + 900}`, 0, payload.length - 1],
    [`bytes=${payload.length - 1}-${payload.length + 500}`, payload.length - 1, payload.length - 1],
    [`bytes=-${payload.length + 100}`, 0, payload.length - 1],
  ];
  for (const [range, start, end] of valid) {
    await readWithAccounting({ ...context, headers: { Range: range }, expected: 206,
      bytes: payload.subarray(start, end + 1), contentRange: `bytes ${start}-${end}/${payload.length}`, operations: 1 });
  }
  await readWithAccounting({ ...context, headers: { Range: 'bytes=3-7', 'If-Range': etag },
    expected: 206, bytes: payload.subarray(3, 8), contentRange: `bytes 3-7/${payload.length}`, operations: 1 });
  await readWithAccounting({ ...context, headers: { Range: 'bytes=3-7', 'If-None-Match': '"different-file"' },
    expected: 206, bytes: payload.subarray(3, 8), contentRange: `bytes 3-7/${payload.length}`, operations: 1 });

  for (const value of ['"different-file"', `W/${etag}`, 'Wed, 21 Oct 2015 07:28:00 GMT', '*']) {
    await readWithAccounting({ ...context, headers: { Range: 'bytes=3-7', 'If-Range': value },
      expected: 200, bytes: payload, operations: 1 });
  }
  await readWithAccounting({ ...context, headers: { Range: 'bytes=invalid-range', 'If-Range': '"different-file"' },
    expected: 200, bytes: payload, operations: 1 });
  for (const range of ['bytes=0-1,5-6', 'items=0-1']) {
    await readWithAccounting({ ...context, headers: { Range: range }, expected: 200, bytes: payload, operations: 1 });
  }

  const invalid = [
    `bytes=${payload.length}-`, `bytes=${payload.length}-${payload.length + 1}`,
    'bytes=8-3', 'bytes=-0', 'bytes=-', 'bytes=',
    'bytes=18446744073709551616-', 'bytes=0-18446744073709551616',
    'bytes=abc-def',
  ];
  let errorBytes = 0;
  for (const range of invalid) {
    errorBytes += await readWithAccounting({ ...context, headers: { Range: range }, expected: 416,
      contentRange: `bytes */${payload.length}`, operations: 0 });
  }
  await readWithAccounting({ ...context, headers: { Range: 'bytes=8-3', 'If-Range': etag },
    expected: 416, contentRange: `bytes */${payload.length}`, operations: 0 });
  await readWithAccounting({ ...context, file: empty, headers: { Range: 'bytes=0-0' },
    expected: 416, contentRange: 'bytes */0', operations: 0 });

  const foreignBefore = await classBReads(db, foreign.owner);
  const denied = await dispatch(mf, foreign.apiKey, 'GET', `/v1/files/${main.id}/content`, {
    headers: { 'If-None-Match': etag, Range: 'bytes=0-1' }, expected: 404,
  });
  await denied.text();
  assert.deepEqual(await classBReads(db, foreign.owner), foreignBefore, 'Conditional headers must not bypass ownership or cause a foreign read');

  const receipt = {
    fixture: 'actual compiled Rust Worker with synthetic R2 content and observed accounting',
    full_bytes: fullBytes, full_r2_reads: 1,
    selected_range_bytes: partialBytes, selected_range_r2_reads: 1,
    not_modified_bytes: notModifiedBytes, not_modified_r2_reads: 0,
    invalid_range_cases: invalid.length, invalid_range_body_bytes: errorBytes, invalid_range_r2_reads: 0,
  };
  const directory = new URL('../../.cache/perf/v3/', import.meta.url);
  await mkdir(directory, { recursive: true });
  await writeFile(new URL('files-actual.json', directory), JSON.stringify(receipt, null, 2) + '\n');
  console.log('Space content wire measurements: ' + JSON.stringify(receipt));
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
  assert.equal(main.content_type, 'application/octet-stream');
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
  assert.equal(rename.content_type, main.content_type);
  assert.equal((await db.prepare('SELECT object_key FROM space_files WHERE id=?').bind(main.id).first()).object_key, stored.object_key, 'Metadata rename must not rewrite content');
  const metadata = await request('GET', `/v1/files/${main.id}`, undefined, 200, owner.apiKey);
  assert.equal(metadata.filename, 'renamed.bin');
  assert.equal(metadata.content_type, main.content_type);

  const generated = [main];
  for (const [bytes, purpose] of [[Buffer.from('first\n'), 'user_data'], [Buffer.from('second\0\n'), 'assistants'], [Buffer.alloc(0), 'user_data']]) {
    generated.push(await multipart(mf, owner.apiKey, 'same-name.txt', bytes, purpose));
  }
  assert.equal(new Set(generated.map(file => file.id)).size, generated.length, 'A display filename is not object identity');
  assert.equal(generated.at(-1).bytes, 0);
  assert.equal(generated.at(-1).sha256, digest(Buffer.alloc(0)));
  await bandwidthJourney({ mf, db, owner, foreign, main, empty: generated.at(-1), payload });
  const descending = await filePages(request, owner.apiKey);
  assert.ok(descending.every(file => file.content_type === 'application/octet-stream'));
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
  const failedFiles = (await request('GET', '/v1/files?limit=100', undefined, 200, owner.apiKey)).data.filter(file => file.filename === 'bad-digest.bin');
  assert.equal(failedFiles.length, 1, 'Known failed uploads retain discoverable owner diagnostics until deleted');
  assert.equal(failedFiles[0].status, 'error');
  assert.equal(failedFiles[0].diagnostics.upload_status, 'failed');
  assert.equal(typeof failedFiles[0].diagnostics.operation_id, 'string');
  assert.ok(failedFiles[0].diagnostics.operation_id);
  assert.deepEqual(await request('GET', `/v1/files/${failedFiles[0].id}`, undefined, 200, owner.apiKey), failedFiles[0]);
  await dispatch(mf, owner.apiKey, 'GET', `/v1/files/${failedFiles[0].id}/content`, { expected: 404 });
  await request('GET', `/v1/files/${failedFiles[0].id}`, undefined, 404, foreign.apiKey);
  await request('DELETE', `/v1/files/${failedFiles[0].id}`, undefined, 200, owner.apiKey);
  await request('GET', `/v1/files/${failedFiles[0].id}`, undefined, 404, owner.apiKey);

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
