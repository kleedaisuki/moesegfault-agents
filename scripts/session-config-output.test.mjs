import assert from 'node:assert/strict';
import test from 'node:test';
import { toolResult } from '../tests/worker/session-config-journey.mjs';

const body = output => ({ input: [{ type: 'function_call_output', call_id: 'clock', output }] });

test('configuration tool assertions decode genuine native text parts rather than object coercion', () => {
  const clock = { unix_ms: 123456, utc: 'synthetic clock metadata' };
  assert.deepEqual(toolResult(body([{ type: 'input_text', text: JSON.stringify(clock) }]), 'clock'), clock);
  const denied = { error: { code: 'tool_revoked' } };
  assert.deepEqual(toolResult(body([{ type: 'input_text', text: JSON.stringify(denied) }]), 'clock'), denied);
});

test('configuration tool assertions reject nontext, missing and malformed results without fallback', () => {
  assert.throws(() => toolResult(body({ unix_ms: 123456 }), 'clock'), /content-part array/);
  assert.throws(() => toolResult(body([{ type: 'input_image', image_url: 'not-used' }]), 'clock'), /actual text parts/);
  assert.throws(() => toolResult(body([{ type: 'input_text', text: 'not JSON' }]), 'clock'), SyntaxError);
  assert.throws(() => toolResult({ input: [] }, 'clock'), /genuine clock/);
});
