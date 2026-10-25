import assert from 'node:assert/strict';
import test from 'node:test';
import { assertResponseUsage } from './responses-sdk.mjs';

const counts = {
  input_tokens: 12,
  input_tokens_details: { cached_tokens: 3, cache_write_tokens: 0 },
  output_tokens: 4,
  output_tokens_details: { reasoning_tokens: 0 },
  total_tokens: 16,
};

test('Node SDK journey checks known and explicitly unknown reasoning and cache-write usage', () => {
  assertResponseUsage({ usage: counts });
  assertResponseUsage({ usage: null, agents_usage: { ...counts, output_tokens_details: { reasoning_tokens: null } } });
  assertResponseUsage({ usage: null, agents_usage: {
    ...counts, input_tokens_details: { cached_tokens: 3, cache_write_tokens: null },
    output_tokens_details: { reasoning_tokens: 2 },
  } });
});

test('Node SDK journey rejects missing totals, invalid bounds and malformed standard usage', () => {
  for (const response of [
    { usage: null },
    { usage: { ...counts, total_tokens: 17 } },
    { usage: { ...counts, input_tokens_details: { cached_tokens: 13 } } },
    { usage: { ...counts, output_tokens_details: { reasoning_tokens: null } } },
    { usage: { ...counts, output_tokens_details: { reasoning_tokens: 5 } } },
    { usage: { ...counts, output_tokens_details: { reasoning_tokens: -1 } } },
    { usage: { ...counts, input_tokens_details: { cached_tokens: 3, cache_write_tokens: null } } },
    { usage: { ...counts, input_tokens_details: { cached_tokens: 3, cache_write_tokens: -1 } } },
    { usage: null, agents_usage: counts },
    { usage: null, agents_usage: { ...counts, output_tokens_details: {} } },
  ]) assert.throws(() => assertResponseUsage(response));
});
