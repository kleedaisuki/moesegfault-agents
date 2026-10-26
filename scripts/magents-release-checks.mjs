import { execFileSync, spawnSync } from 'node:child_process';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { pathToFileURL } from 'node:url';
import { changedRustTests, runChecks, rustTestArguments } from './incremental-checks.mjs';

const FILE = 'crates/magents/tests/space_responses.rs';
const PLATFORMS = { 'x86_64-unknown-linux-gnu': 'linux', 'x86_64-pc-windows-msvc': 'win32' };
const V030_CASES = [
  'auxiliary_commands_preserve_native_requests_and_complete_compacted_windows',
  'native_conversation_crud_items_and_container_routes_preserve_json',
  'native_context_help_and_local_rejections_work_without_login',
  'failed_native_operations_report_http_failure_without_replay_or_body_leaks',
  'progressive_help_and_local_rejections_require_no_login',
];

export function releaseChecks(version, target, source, content, platform = process.platform) {
  if (version !== '0.3.0') throw Error('Native release increment is defined only for magents 0.3.0');
  if (!Object.hasOwn(PLATFORMS, target)) throw Error('Unsupported native release target');
  if (!/^[a-f0-9]{40}$/.test(source)) throw Error('Invalid native release source');
  if (PLATFORMS[target] !== platform) throw Error('Native release target must run on its actual platform');
  const declared = changedRustTests(content, [[1, content.split('\n').length + 1]]);
  const checks = V030_CASES.map(name => {
    if (!declared.includes(name)) throw Error(`Missing native release CLI case: ${name}`);
    const [cargo, test, ...args] = rustTestArguments('magents', name, FILE);
    return { id: `rust:magents:${name}`,
      argv: [cargo, test, '--release', '--target', target, ...args, '--exact'],
      env: {}, source_file: FILE, reason: 'v0.3.0 native release increment on this platform' };
  });
  return { schema_version: 1, source_sha: source, version, target, platform, profile: 'release',
    validation_scope: 'v0.3.0-native-context', checks: { rust: checks } };
}

export function runReleaseChecks(plan, execute = spawnSync, directory = `.temp/magents-release-checks/${plan.target}`) {
  mkdirSync(directory, { recursive: true });
  writeFileSync(`${directory}/plan.json`, JSON.stringify(plan, null, 2) + '\n');
  runChecks(plan, 'rust', execute, directory);
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const source = execFileSync('git', ['rev-parse', 'HEAD'], { encoding: 'utf8' }).trim();
  runReleaseChecks(releaseChecks(process.env.PACKAGE_VERSION, process.env.TARGET, source, readFileSync(FILE, 'utf8')));
}
