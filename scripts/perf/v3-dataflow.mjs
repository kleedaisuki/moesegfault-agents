// Read-only staging probe: count response bodies, never persist source content.
import assert from 'node:assert/strict';
import { mkdir, writeFile } from 'node:fs/promises';
import { dirname, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import { isDeepStrictEqual } from 'node:util';

async function body(response) {
  const chunks = [];
  let length = 0;
  const reader = response.body?.getReader();
  if (reader) {
    while (true) {
      const next = await reader.read();
      if (next.done) break;
      length += next.value.byteLength;
      if (length > 16 * 1024 * 1024) {
        await reader.cancel();
        throw new Error('Read-flow response exceeds the 16 MiB probe budget');
      }
      chunks.push(next.value);
    }
  }
  const bytes = new Uint8Array(length);
  let offset = 0;
  for (const chunk of chunks) {
    bytes.set(chunk, offset);
    offset += chunk.byteLength;
  }
  try {
    return { bytes: bytes.byteLength, value: bytes.length ? JSON.parse(new TextDecoder().decode(bytes)) : null };
  } catch {
    throw new Error('Read-flow response is not valid JSON');
  }
}

export async function measureReadFlow({ origin, session, token, polls = 10, fetcher = fetch }) {
  const base = new URL(origin);
  if (base.hostname !== 'agents-staging.moesegfault.dev' && !['localhost', '127.0.0.1', '[::1]'].includes(base.hostname)) {
    throw new Error('Use staging or a local fixture; production is not an implicit benchmark target');
  }
  if (!token || !session || !Number.isInteger(polls) || polls < 1 || polls > 30) {
    throw new Error('Supply a session, token and 1–30 unchanged polls');
  }
  const prefix = `/v1/sessions/${encodeURIComponent(session)}`;
  const observations = [];
  const read = async (path, extra = {}) => {
    const response = await fetcher(new URL(path, base), {
      headers: { Authorization: `Bearer ${token}`, ...extra }, signal: AbortSignal.timeout(30_000), redirect: 'error',
    });
    const decoded = await body(response);
    observations.push({ path: path.split('?')[0].replace(prefix, '<session>'), status: response.status, bytes: decoded.bytes });
    return { ...decoded, status: response.status, etag: response.headers.get('etag') };
  };
  const health = await read('/healthz');
  assert.equal(health.status, 200);
  assert.equal(health.value.status, 'ok');
  const legacy = await read(prefix);
  assert.equal(legacy.status, 200);
  assert.ok(Array.isArray(legacy.value.messages));
  assert.notEqual(legacy.value.status, 'running', 'Use a stable idle/closed session, not active work');
  const view = await read(`${prefix}/view`);
  assert.equal(view.status, 200);
  assert.equal(view.value.message_count, legacy.value.messages.length);
  assert.ok(view.etag, 'Metadata must advertise a validator');
  assert.ok(!Object.hasOwn(view.value, 'messages') && !Object.hasOwn(view.value, 'pinned'));
  const recent = await read(`${prefix}/messages`);
  assert.equal(recent.status, 200);
  assert.ok(isDeepStrictEqual(recent.value.data.map(entry => entry.message), legacy.value.messages.slice(recent.value.start, recent.value.end)), 'Recent page differs from original source');
  assert.equal(recent.value.end, legacy.value.messages.length);
  let conditionalBytes = 0;
  for (let poll = 0; poll < polls; poll++) {
    const unchanged = await read(`${prefix}/view`, { 'If-None-Match': view.etag });
    assert.equal(unchanged.status, 304, 'State changed during the probe; do not treat a changed view as zero-byte polling');
    assert.equal(unchanged.bytes, 0);
    conditionalBytes += unchanged.bytes;
  }
  const reconstructed = [...recent.value.data];
  let page = recent.value;
  let olderRequests = 0;
  while (page.has_older) {
    assert.ok(page.before_cursor);
    assert.ok(olderRequests++ < 1000, 'History did not make bounded cursor progress');
    const older = await read(`${prefix}/messages?before=${encodeURIComponent(page.before_cursor)}`);
    assert.equal(older.status, 200);
    assert.equal(older.value.end, page.start);
    assert.ok(older.value.start < page.start);
    reconstructed.unshift(...older.value.data);
    page = older.value;
  }
  assert.ok(isDeepStrictEqual(reconstructed.map(entry => entry.sequence), legacy.value.messages.map((_, index) => index)), 'Reconstructed source positions are missing or duplicated');
  assert.ok(isDeepStrictEqual(reconstructed.map(entry => entry.message), legacy.value.messages), 'Reconstructed source differs from original messages');
  const final = await read(`${prefix}/view`, { 'If-None-Match': view.etag });
  assert.equal(final.status, 304, 'Source changed during reconstruction');
  const baseline = legacy.bytes * (polls + 1);
  const optimized = view.bytes + recent.bytes + conditionalBytes;
  return {
    schema: 1, build_id: health.value.build_id, node: process.version,
    message_count: legacy.value.messages.length, recent_messages: recent.value.data.length,
    unchanged_polls: polls, legacy_body_bytes_per_read: legacy.bytes,
    view_body_bytes: view.bytes, recent_page_body_bytes: recent.bytes,
    legacy_initial_plus_polls_body_bytes: baseline,
    optimized_initial_plus_polls_body_bytes: optimized,
    body_reduction_percent: 100 * (1 - optimized / baseline),
    complete_source_reconstruction: true, older_history_requests: olderRequests,
    observation_count: observations.length,
    notes: [
      'Real response body byte counts after client decompression, not HTTP headers or on-wire compression.',
      'The baseline repeated-body total is calculated from one stable full read; the probe does not retransmit full history repeatedly.',
      'Older-history reconstruction is a correctness check, excluded from the progressive initial-render/polling comparison.',
      'No model/tool execution, mutation or user activity clock update is requested; no source content or session identity is persisted.',
    ],
  };
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  const report = await measureReadFlow({
    origin: process.env.MAGENTS_TEST_API_ORIGIN ?? 'https://agents-staging.moesegfault.dev',
    token: process.env.MAGENTS_TEST_API_KEY, session: process.env.MAGENTS_PERF_SESSION_ID,
    polls: Number(process.env.MAGENTS_PERF_POLLS ?? 10),
  });
  const destination = resolve(process.env.MAGENTS_PERF_OUTPUT ?? '.cache/perf/v3/deployed-read.json');
  await mkdir(dirname(destination), { recursive: true });
  await writeFile(destination, `${JSON.stringify(report, null, 2)}\n`);
  process.stdout.write(`${JSON.stringify(report, null, 2)}\n`);
}
