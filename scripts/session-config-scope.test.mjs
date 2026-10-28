import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import test from 'node:test';
import { selections, sessionConfigurationFixtureOnly } from './incremental-checks.mjs';

test('only the exact optional-None provenance fixture insertion is compilation-only', () => {
  const file = 'crates/agents-worker/src/telemetry.rs';
  const diff = ['--- a/' + file, '+++ b/' + file, '@@ -690,0 +691 @@', '+            configuration: None,'].join('\n');
  assert.equal(sessionConfigurationFixtureOnly(file, diff), true);
  assert.equal(sessionConfigurationFixtureOnly('crates/agents-worker/src/session.rs', diff), false);
  assert.equal(sessionConfigurationFixtureOnly(file, diff + '\n+            assert!(false);'), false);
  assert.equal(sessionConfigurationFixtureOnly(file, diff + '\n-            configuration: Some(old),'), false);
  assert.equal(sessionConfigurationFixtureOnly(file, diff.replace('None', 'Some(new)')), false);
});

test('new configuration compaction coverage does not replay the accepted context-management journey', () => {
  const revision = execFileSync('git', ['rev-parse', 'HEAD'], { encoding: 'utf8' }).trim();
  const selected = selections(revision, revision, ['crates/agents-worker/src/session_context_management.rs',
    'tests/worker/session-config-journey.mjs']);
  assert.ok(selected.worker.some(check => check.env.MAGENTS_WORKER_CASE ===
    'Agent revisions append to an existing Session and survive compaction and cold recovery'));
  assert.equal(selected.worker.some(check => check.env.MAGENTS_WORKER_CASE ===
    'Responses request context management emits sealed item and continues'), false);
});
