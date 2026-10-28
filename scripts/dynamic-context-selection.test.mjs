import assert from 'node:assert/strict';
import fs from 'node:fs';
import test from 'node:test';
import { changedRustTests, checkInputPaths, dynamicContextChangedLines, rustTestArguments, selections } from './incremental-checks.mjs';

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

test('registered dispatcher increment binds exactly its three new Rust guards', () => {
  const source = fs.readFileSync(new URL('../crates/agents-worker/src/registered_tools.rs', import.meta.url), 'utf8');
  const names = changedRustTests(source, [[1, source.split('\n').length + 1]]);
  assert.deepEqual(names, [
    'dynamic_context_dispatcher_schema_and_catalog_revision_do_not_depend_on_instructions',
    'dynamic_context_envelope_preserves_native_identity_and_checks_current_schema',
    'dynamic_context_layout_keeps_origin_provenance_out_of_live_guidance',
  ]);
  for (const name of names) assert.deepEqual(rustTestArguments('agents-worker', name, 'crates/agents-worker/src/registered_tools.rs'),
    ['cargo', 'test', '--locked', '-p', 'agents-worker', '--lib', name, '--', '--nocapture']);
});

test('pure registered dispatcher receipts exclude unrelated runtime fixes without losing their dependencies', () => {
  const file = 'crates/agents-worker/src/registered_tools.rs';
  const check = { id: 'rust:agents-worker:dynamic_context_layout_keeps_origin_provenance_out_of_live_guidance',
    argv: ['cargo', 'test', '-p', 'agents-worker', '--lib', 'dynamic_context_layout_keeps_origin_provenance_out_of_live_guidance'], source_file: file };
  const files = [file, 'crates/agents-worker/src/configuration.rs', 'crates/agents-worker/src/context_prefix.rs',
    'crates/agents-worker/src/prompt_context.rs', 'crates/agents-worker/src/responses_schema.rs',
    'crates/agents-worker/src/session.rs', 'crates/agents-worker/src/workspace.rs'];
  const paths = checkInputPaths('a'.repeat(40), check, () => files.join('\n'));
  for (const file of files.slice(0, 5)) assert.ok(paths.includes(file));
  for (const file of files.slice(5)) assert.ok(!paths.includes(file), `Unrelated ${file} must not invalidate an accepted pure guard`);
});
