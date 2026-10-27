import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

test('native release planning can see an existing draft without broadening build permissions or bypassing promotion', () => {
  const workflow = readFileSync(new URL('../.github/workflows/release-magents.yml', import.meta.url), 'utf8').replaceAll('\r\n', '\n');
  const prepare = workflow.split('\n  prepare:\n')[1].split('\n  package:\n')[0];
  const defaults = workflow.split('\npermissions:\n')[1].split('\nconcurrency:\n')[0];
  const packages = workflow.split('\n  package:\n')[1].split('\n  attach:\n')[0];
  assert.match(prepare, /\n    permissions:\n      contents: write\n      actions: read\n/);
  assert.match(defaults, /contents: read/);
  assert.ok(!defaults.includes('contents: write'));
  assert.ok(!packages.includes('contents: write'));
  assert.ok(prepare.includes('run: python scripts/magents-release-promotion.py plan'));
  assert.ok(prepare.includes("format('refs/tags/{0}', github.event.release.tag_name || inputs.release_tag)"));
  assert.ok(packages.includes("if: needs.prepare.outputs.mode == 'build'"));
  assert.ok(workflow.includes("if: needs.prepare.outputs.mode == 'reuse' || needs.prepare.outputs.mode == 'existing'"));
  assert.ok(!prepare.includes('release publish') && !prepare.includes('release create') && !prepare.includes('release edit'));
});
