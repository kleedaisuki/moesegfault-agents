import assert from 'node:assert/strict';
import test from 'node:test';
import { readFileSync } from 'node:fs';
import { main, imageRequest, summarizeImage } from './hosted-image-sdk.mjs';

const png = 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aOSkAAAAASUVORK5CYII=';
const item = { id: 'ig-owned', type: 'image_generation_call', status: 'completed', result: png };
const response = { status: 'completed', output: [item] };
const events = [
  { type: 'response.image_generation_call.in_progress', item_id: item.id },
  { type: 'response.image_generation_call.completed', item_id: item.id },
  { type: 'response.output_item.done', item },
];

test('one real image request binds only one hosted call and native explicit 1K size', () => {
  const body = imageRequest('owned-agent');
  assert.equal(body.max_tool_calls, 1); assert.equal(body.max_output_tokens, 256);
  assert.equal(body.tool_choice, undefined);
  assert.deepEqual(body.tools, [{ type: 'image_generation', size: '1024x1024' }]);
  assert.equal(body.response_format, undefined);
});

test('image evidence checks actual raster, canonical phases and SSE bytes without retaining images', () => {
  const summary = summarizeImage(response, events);
  assert.equal(summary.format, 'png'); assert.equal(summary.bytes, Buffer.from(png, 'base64').length);
  assert.match(summary.sha256, /^[a-f0-9]{64}$/);
  assert.equal(JSON.stringify(summary).includes(png), false);
  assert.throws(() => summarizeImage({ ...response, output: [item, item] }, events), /exactly_one/);
  assert.throws(() => summarizeImage({ ...response, output: [{ ...item, result: 'https://cdn.example/image.png' }] }, events));
  assert.throws(() => summarizeImage(response, events.slice(1)), /deep-equal/);
  assert.throws(() => summarizeImage(response, events.map(event => event.item ? { ...event, item: { ...item, result: 'changed' } } : event)), /SSE_done_image_differs/);
});

test('real image journey rejects ungated paid work before network or key access', async () => {
  await assert.rejects(main([], {}), /exact_image_assembly_source_required/);
  await assert.rejects(main(['--source', 'a'.repeat(40)], {}), /single_new_paid_image_gate_required/);
  await assert.rejects(main(['--source', 'a'.repeat(40), '--allow-paid-image'], {}), /owned_platform_key_missing/);
});

test('image actual runner reuses direct routing and never sends a direct provider request or an accepted POST replay', () => {
  const source = readFileSync(new URL('./hosted-image-sdk.mjs', import.meta.url), 'utf8');
  assert.equal((source.match(/client\.responses\.create\(/g) ?? []).length, 1);
  assert.equal((source.match(/new OpenAI\(/g) ?? []).length, 1);
  assert.ok(source.includes('maxRetries: 0'));
  assert.ok(source.includes('policy.assertDirectDeepSeekPin'));
  assert.ok(source.includes('canonicalImageRetained: true'));
  assert.ok(source.includes('existing_image_intent_requires_reconciliation_not_replay'));
  assert.ok(source.includes('actual_deployment_must_enable_images'));
  assert.equal(source.includes('writeFileSync(config.output, JSON.stringify({ response, events'), false);
});
