import assert from 'node:assert/strict';
import test from 'node:test';
import { framingExperiment } from './v3-response-framing.mjs';

test('framing experiment preserves exact UTF-8 text and immediate first fragment under all policies', () => {
  const result = framingExperiment();
  for (const { policies } of result.workloads) {
    for (const policy of Object.values(policies)) {
      assert.ok(policy.exact_text_preserved);
      assert.equal(policy.first_text_delay_ms, 0);
      assert.equal(policy.text_bytes, policies.immediate.text_bytes);
    }
  }
});

test('callback-only coalescing exposes quiet gaps while timer coalescing bounds extra fragment delay', () => {
  const result = framingExperiment();
  const quiet = result.workloads.find(workload => workload.name === 'quiet_gap').policies;
  assert.equal(quiet.callback.maximum_fragment_delay_ms, 2999);
  for (const { policies } of result.workloads) assert.ok(policies.timer.maximum_fragment_delay_ms <= 50);
  const burst = result.workloads.find(workload => workload.name === 'burst').policies;
  assert.ok(burst.timer.delta_sse_body_bytes < burst.immediate.delta_sse_body_bytes / 10);
});
