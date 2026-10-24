import assert from 'node:assert/strict';

const origin = 'https://agents.test';

async function exportResponse(mf, endpoint, key, expected = 200) {
  const response = await mf.dispatchFetch(`${origin}${endpoint}`, {
    headers: { Authorization: `Bearer ${key}`, 'CF-Connecting-IP': '192.0.2.1' },
  });
  assert.equal(response.status, expected, `${endpoint}: unexpected HTTP status`);
  assert.equal(response.headers.get('cache-control'), 'no-store');
  return response;
}

function assertExportHeaders(response, mime, messages) {
  assert.ok(response.headers.get('content-type')?.startsWith(mime));
  assert.ok(response.headers.get('content-disposition')?.startsWith('attachment;'));
  assert.equal(response.headers.get('x-export-message-count'), String(messages.length));
}

export async function runExportJourney({ mf, db, request, sessionId, key, createBrowserPrincipal, encryptionKey }) {
  const before = await request('GET', `/v1/sessions/${sessionId}`);
  assert.ok(before.messages.length >= 4, 'Use an actually executed source journal, not an empty transcript fixture');
  assert.ok(before.messages.some(message => message.tool_calls?.some(call => call.function.name === 'time')));
  assert.ok(before.messages.some(message => message.role === 'tool' && message.tool_call_id === 'call_time'));
  const walletBefore = await db.prepare("SELECT CAST(spent_microusd AS TEXT) AS spent, CAST(reserved_microusd AS TEXT) AS reserved FROM usage_accounts WHERE account_id='owner'").first();
  const started = Date.now();
  const jsonResponse = await exportResponse(mf, `/v1/sessions/${sessionId}/export`, key);
  assertExportHeaders(jsonResponse, 'application/json', before.messages);
  const exported = await jsonResponse.json();
  assert.equal(exported.schema_version, 1);
  assert.ok(Number.isSafeInteger(exported.exported_at_ms) && exported.exported_at_ms >= started - 1000 && exported.exported_at_ms <= Date.now() + 1000);
  assert.equal(exported.session.id, sessionId);
  assert.deepEqual(exported.session.pinned, before.pinned, 'Export pinned configuration, not a mutable agent lookup');
  assert.equal(exported.session.input_revision, before.input_revision);
  assert.equal(exported.session.last_input_at_ms, before.last_input_at_ms);
  assert.deepEqual(exported.messages, before.messages, 'Preserve complete source messages, tool arguments/results and provider continuation fields');
  assert.deepEqual(exported.pending_messages, before.pending_messages);
  assert.equal(exported.context.input_tokens, before.context_tokens);
  assert.equal(exported.context.compact_count, before.compact_count);

  const explicitJson = await exportResponse(mf, `/v1/sessions/${sessionId}/export?format=json`, key);
  assert.deepEqual((await explicitJson.json()).messages, before.messages);
  const jsonlResponse = await exportResponse(mf, `/v1/sessions/${sessionId}/export?format=jsonl`, key);
  assertExportHeaders(jsonlResponse, 'application/x-ndjson', before.messages);
  const jsonl = (await jsonlResponse.text()).trimEnd().split('\n').map(line => JSON.parse(line));
  assert.equal(jsonl[0].type, 'session');
  assert.equal(jsonl[0].schema_version, 1);
  assert.equal(jsonl[0].session.id, sessionId);
  assert.equal(jsonl.length, 1 + before.messages.length + before.pending_messages.length);
  const journal = jsonl.filter(entry => entry.type === 'message');
  assert.deepEqual(journal.map(entry => entry.sequence), before.messages.map((_, index) => index));
  assert.deepEqual(journal.map(entry => entry.message), before.messages);
  assert.deepEqual(jsonl.filter(entry => entry.type === 'pending_message').map(entry => entry.message), before.pending_messages);

  const markdownResponse = await exportResponse(mf, `/v1/sessions/${sessionId}/export?format=markdown`, key);
  assertExportHeaders(markdownResponse, 'text/markdown', before.messages);
  const markdown = await markdownResponse.text();
  assert.ok(markdown.includes(sessionId));
  assert.ok(markdown.includes('Use time, then answer.'));
  assert.ok(markdown.includes('The clock tool completed.'));
  assert.ok(markdown.includes('call_time'), 'Markdown must retain structured tool-call/result relationships');

  await exportResponse(mf, `/v1/sessions/${sessionId}/export?format=csv`, key, 400);
  await exportResponse(mf, `/v1/sessions/${sessionId}/export?format=json&format=jsonl`, key, 400);
  await exportResponse(mf, `/v1/sessions/${sessionId}/workspace/archive`, key, 404);
  const after = await request('GET', `/v1/sessions/${sessionId}`);
  assert.deepEqual(after.messages, before.messages);
  assert.deepEqual(after.pending_messages, before.pending_messages);
  assert.equal(after.input_revision, before.input_revision);
  assert.equal(after.last_input_at_ms, before.last_input_at_ms, 'Downloads are reads, not fresh user input');
  assert.equal(after.status, before.status);
  assert.equal(after.compact_count, before.compact_count);
  assert.deepEqual(await db.prepare("SELECT CAST(spent_microusd AS TEXT) AS spent, CAST(reserved_microusd AS TEXT) AS reserved FROM usage_accounts WHERE account_id='owner'").first(), walletBefore, 'DO journal exports must not initiate a model/resource reservation');

  const other = await createBrowserPrincipal({ mf, db, encryptionKey, owner: 'export-other' });
  await exportResponse(mf, `/v1/sessions/${sessionId}/export`, other.apiKey, 404);
  await exportResponse(mf, `/v1/sessions/${sessionId}/workspace/archive`, other.apiKey, 404);
  const unauthenticated = await mf.dispatchFetch(`${origin}/v1/sessions/${sessionId}/export`);
  assert.equal(unauthenticated.status, 401);
  await unauthenticated.text();
  const missing = await exportResponse(mf, '/v1/sessions/export-nonexistent/export', key, 404);
  assert.equal((await missing.json()).error.code, 'not_found');

  // This session never dispatches Bash or allocates a Container. It proves that
  // archive reads reject uncommitted live work instead of implicitly closing it.
  const agent = await request('POST', '/v1/agents', {
    name: 'Archive readiness fixture', model: { provider: 'openrouter', model: 'test-model' },
    tools: ['time', 'bash'], workspace: true,
  }, 201, other.apiKey);
  const session = await request('POST', `/v1/agents/${agent.id}/sessions`, {}, 201, other.apiKey);
  const notReady = await exportResponse(mf, `/v1/sessions/${session.id}/workspace/archive`, other.apiKey, 409);
  assert.equal((await notReady.json()).error.code, 'workspace_not_ready');
  const unchanged = await request('GET', `/v1/sessions/${session.id}`, undefined, 200, other.apiKey);
  assert.equal(unchanged.status, session.status);
  assert.equal(unchanged.last_input_at_ms, session.last_input_at_ms);
  assert.equal(unchanged.input_revision, session.input_revision);
  // The whole isolated Miniflare instance is disposed by its parent fixture.
}
