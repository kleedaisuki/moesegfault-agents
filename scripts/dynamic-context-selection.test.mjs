import assert from 'node:assert/strict';
import fs from 'node:fs';
import test from 'node:test';
import { dynamicContextChangedLines, selections } from './incremental-checks.mjs';

test('dynamic context increments select only the three new user journeys', () => {
  const selected = selections('a'.repeat(40), 'b'.repeat(40), ['tests/worker/dynamic-context-journey.mjs']);
  assert.deepEqual(selected.worker.map(check => check.env.MAGENTS_WORKER_CASE), [
    'Dynamic Session catalogs preserve provider prefixes across callable tool and instruction revisions',
    'Dynamic Session catalog revisions close stale calls and preserve caller tool ownership',
    'Dynamic Session catalogs retain concrete tool memory through compaction cold recovery and historical forks',
  ]);
  assert.ok(selected.worker.every(check => check.argv.at(-1) === 'tests/worker/http.test.mjs'));
  assert.equal(selected.worker.some(check => /Responses cache|Agent revisions|Issued workspace/.test(check.env.MAGENTS_WORKER_CASE)), false);
  for (const job of ['rust', 'dashboard', 'container', 'tooling', 'current_sdk']) assert.deepEqual(selected[job], []);
});

test('dynamic schema verification uses auto and actual concrete compaction memory', () => {
  const source = fs.readFileSync(new URL('../tests/worker/dynamic-context-journey.mjs', import.meta.url), 'utf8');
  assert.ok(source.includes("tool_choice: 'auto'"));
  assert.ok(!source.includes("tool_choice: 'none'"), 'Disabled callable declarations cannot establish the requested schema-prefix property');
  assert.ok(source.includes('/workspace/dynamic-context-proof.txt'));
  assert.ok(source.includes('original_clock_unix_ms=${firstClock}'));
  assert.ok(source.includes("assert.equal(declaration.length, 1"), 'An empty active catalog must retain the same genuine callable dispatcher');
});

test('only exact optional context-layout fixture additions are compilation-only', () => {
  const file = 'crates/agents-worker/src/telemetry.rs';
  const fixture = '@@ -10,0 +11 @@\n+            context_layout: None,\n';
  assert.deepEqual(dynamicContextChangedLines(file, fixture), []);
  assert.deepEqual(dynamicContextChangedLines('crates/agents-core/src/session.rs', fixture), []);
  assert.deepEqual(dynamicContextChangedLines('scripts/not-a-rust-fixture.mjs', fixture), [[11, 12]]);
  assert.deepEqual(dynamicContextChangedLines(file, fixture.replace('None', 'Some(new)')), [[11, 12]]);
  assert.deepEqual(dynamicContextChangedLines(file, fixture + '-            context_layout: Some(old),\n'), [[11, 12]]);
  assert.deepEqual(dynamicContextChangedLines(file, fixture + '+            assert!(false);\n'), [[11, 12]]);
});

test('mechanical fixture fields do not hide actual changed behavior in another hunk', () => {
  const diff = '@@ -10,0 +11 @@\n+            context_layout: None,\n'
    + '@@ -30 +31 @@\n-    assert_eq!(before, old);\n+    assert_eq!(before, new);\n';
  assert.deepEqual(dynamicContextChangedLines('crates/agents-worker/src/session.rs', diff), [[31, 32]]);
});
