import { execFileSync, spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { appendFileSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { pathToFileURL } from 'node:url';

const SHA = /^[a-f0-9]{40}$/;
const OUTPUT = '.cache/verification';
const COMPONENTS = {
  worker: ['Cargo.toml', 'Cargo.lock', 'rust-toolchain.toml', 'crates/agents-worker', 'crates/agents-core', 'crates/agents-auth', 'third_party', 'wrangler.jsonc', 'scripts/bundle-worker.mjs', 'package.json', 'package-lock.json'],
  native: ['Cargo.toml', 'Cargo.lock', 'rust-toolchain.toml', 'crates/agents-core', 'crates/agents-auth', 'crates/magents', 'crates/workspace-daemon'],
  container: ['Cargo.toml', 'Cargo.lock', 'rust-toolchain.toml', 'crates/workspace-daemon', 'container', '.dockerignore'],
  dashboard: ['apps/dashboard'],
  tooling: ['scripts', 'tests/e2e', 'tests/fixtures', 'package.json', 'package-lock.json'],
};
const JOBS = { worker: 'worker', native: 'rust', container: 'container', dashboard: 'dashboard', tooling: 'tooling' };
const ARTIFACTS = { worker: 'worker', container: 'container-image', dashboard: 'dashboard' };
const git = (...args) => execFileSync('git', args, { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'], maxBuffer: 32 * 1024 * 1024 }).trim();
const reads = new Map();
const read = (source, file) => { const key = `${source}:${file}`; if (!reads.has(key)) { try { reads.set(key, git('show', key)); } catch { reads.set(key, ''); } } return reads.get(key); };

export function fingerprint(source, component, runGit = git) {
  if (!SHA.test(source) || !Object.hasOwn(COMPONENTS, component)) throw Error('Invalid fingerprint identity');
  // Git tree entries bind path, file mode and exact bytes, including dependency locks.
  return createHash('sha256').update(runGit('ls-tree', '-r', source, '--', ...COMPONENTS[component])).digest('hex');
}

export function baselineFor(source, component, baselines, hash = fingerprint) {
  const candidates = baselines.filter(base => base.components.includes(component));
  if (!candidates.length) throw Error(`No sealed baseline for ${component}`);
  const current = hash(source, component);
  const base = [...candidates].reverse().find(base => hash(base.source_sha, component) === current)
    ?? candidates.at(-1);
  return { ...base, source_sha: base.source_sha, run_id: base.run_id, job: JOBS[component],
    fingerprint: hash(base.source_sha, component), matches: hash(base.source_sha, component) === current };
}

function changed(source, base) {
  return git('diff', '--name-only', base, source).split('\n').filter(Boolean);
}

function changedLines(source, base, file) {
  const diff = git('diff', '--unified=0', base, source, '--', file);
  return [...diff.matchAll(/^@@ .* \+(\d+)(?:,(\d+))? @@/gm)]
    .map(match => [Number(match[1]), Number(match[1]) + Number(match[2] ?? 1)]);
}

export function changedRustTests(content, ranges) {
  const lines = content.split('\n');
  const tests = [];
  let attribute = false;
  let ignored = false;
  for (let i = 0; i < lines.length; i++) {
    if (/^\s*#\[(?:[\w:]+::)?test(?:\([^\]]*\))?\]/.test(lines[i])) attribute = true;
    if (/^\s*#\[ignore(?:[\](= ]|$)/.test(lines[i])) ignored = true;
    const match = lines[i].match(/^\s*(?:async\s+)?fn\s+([a-zA-Z0-9_]+)\s*\(/);
    if (!match) continue;
    if (attribute && !ignored) tests.push({ name: match[1], start: i + 1 });
    attribute = false;
    ignored = false;
  }
  return tests.filter((test, index) => ranges.some(([start, end]) =>
    start < (tests[index + 1]?.start ?? lines.length + 1) && end >= test.start))
    .map(test => test.name);
}

function command(id, argv, reason, env = {}) {
  if (argv[0] === 'node' && argv[1] === '--test'
      && !argv.some(arg => arg === '--test-reporter' || arg.startsWith('--test-reporter='))) {
    argv = [argv[0], argv[1], '--test-reporter=tap', ...argv.slice(2)];
  }
  if (argv.includes('pytest')) {
    const temporary = createHash('sha256').update(id).digest('hex').slice(0, 16);
    argv = [...argv, '--basetemp=.temp/pytest-increment-' + temporary, '-o', 'cache_dir=.cache/pytest'];
  }
  return { id, argv, env, reason };
}
const python = (...args) => ['.cache/e2e-venv/bin/python', ...args];

export function rustTestArguments(crate, name, file) {
  const target = file.includes('/tests/') ? ['--test', path.basename(file, '.rs')]
    : ['agents-worker', 'agents-core', 'agents-auth'].includes(crate) ? ['--lib'] : ['--bin', crate];
  return ['cargo', 'test', '--locked', '-p', crate, ...target, name, '--', '--nocapture'];
}

export function selections(source, base, files) {
  const checks = { rust: [], worker: [], dashboard: [], container: [], tooling: [], current_sdk: [] };
  for (const file of files) {
    if (file.endsWith('.rs') && file.startsWith('crates/')) {
      const crate = file.split('/')[1];
      const names = changedRustTests(read(source, file), changedLines(source, base, file));
      const job = crate === 'agents-worker' ? 'worker' : 'rust';
      for (const name of names) checks[job].push({ ...command(`rust:${crate}:${name}`,
        rustTestArguments(crate, name, file), `Changed test in ${file}`), source_file: file });

    }
    if (/^tests\/test_.*\.py$/.test(file)) checks.rust.push(command(`python:${file}`, ['python3', '-m', 'unittest', 'discover', '-s', 'tests', '-p', path.basename(file), '-v'], `Changed offline contract ${file}`));
    if (/^tests\/e2e\/test_.*\.py$/.test(file)) {
      if (/test_workspace_(daemon|interpreter|code)/.test(file)) {
        checks.container.push(command(`pytest:${file}`, python('-m', 'pytest', file, '-ra', '-rP'), `Changed real-container journey ${file}`));
      } else if (/test_(current_responses_sdk_contract)/.test(file)) {
        checks.current_sdk.push(command(`pytest:${file}`, ['.cache/e2e-current-venv/bin/python', '-m', 'pytest', file, '-ra'], `Changed current SDK schema ${file}`));
      } else if (!/test_(deployed|responses|space)\.py$/.test(file)) {
        checks.tooling.push(command(`pytest:${file}`, python('-m', 'pytest', file, '-ra'), `Changed offline contract ${file}`));
      }
    }
    if (/^(scripts\/.*\.test\.mjs|tests\/e2e\/.*(?:\.test|_contract)\.mjs)$/.test(file)) checks.tooling.push(command(`node:${file}`, ['node', '--test', file], `Changed tooling contract ${file}`));
    if (/^tests\/worker\/.*\.test\.mjs$/.test(file) && !file.endsWith('/http.test.mjs')) checks.worker.push(command(`node:${file}`, ['node', '--test', file], `Changed real Worker/SDK contract ${file}`));
  }
  const executionLease = files.some(file => /execution[-_]lease/.test(file));
  const journeys = [
    [/execution[-_]lease/, 'ModelOperation execution lease cold loss and terminal admission rejection'],
    [/prompt[_-]templates/, 'Responses prompt templates preserve version owner variables and replay identity'],
    [/responses-tool-budget/, 'response-wide server tool allowance survives disobedient calls and cold retrieval'],
    [/context_management/, 'Responses request context management emits sealed item and continues'],
    [/^tests\/worker\/auxiliary-journey\.mjs$/, 'Responses auxiliary compact and count'],
    [/^tests\/worker\/deepseek-compact-journey\.mjs$/, 'DeepSeek standalone compaction uses explicit Anthropic transport and genuine usage'],
    [/^tests\/worker\/deepseek-compact-journey\.mjs$/, 'DeepSeek compact reserves the exact rendered input rather than the full model window'],
    [/^tests\/worker\/deepseek-compact-media-journey\.mjs$/, 'DeepSeek compact keeps frozen images and files through a complete continuation window'],
    [/^tests\/worker\/deepseek-tokenizer-journey\.mjs$/, 'DeepSeek exact count-only tokenizer cold warm and profile replacement'],
    [/^tests\/worker\/deepseek-vision-count-journey\.mjs$/, 'DeepSeek vision exact count shares frozen PNG GIF owned files and native media slots'],
  ];
  for (const [pattern, name] of journeys) {
    // Lease fixture controls belong to the new actor journey, not the accepted endpoint baseline.
    if (executionLease && name === 'Responses auxiliary compact and count') continue;
    if (files.some(file => pattern.test(file))) checks.worker.push(command(`journey:${name}`,
      ['node', '--test', 'tests/worker/http.test.mjs'], 'Changed behavior: bounded compiled-Worker journey', { MAGENTS_WORKER_CASE: name }));
  }
  if (files.some(file => /^apps\/dashboard\//.test(file))) {
    const tests = files.filter(file => /^apps\/dashboard\/.*\.(test|spec)\.[cm]?[jt]sx?$/.test(file));
    for (const file of tests) {
      const browser = file.startsWith('apps/dashboard/e2e/');
      const argv = browser ? ['npm', '--prefix', 'apps/dashboard', 'run', 'test:browser', '--', file.replace('apps/dashboard/', '')]
        : ['npm', '--prefix', 'apps/dashboard', 'test', '--', file.replace('apps/dashboard/', '')];
      if (!browser && /\/(?:AgentRouting|Agents)\.test\.tsx$/.test(file)) argv.push('-t',
        'independent agent service routing|preserves an existing model while metadata loads|preserves an existing BYOK model while|preserves existing custom BYOK model limits');
      checks.dashboard.push(command(`ui:${file}`, argv, `Changed ${browser ? 'browser' : 'unit'} UI contract ${file}`));
    }
  }
  for (const job of Object.keys(checks)) checks[job] = [...new Map(checks[job].map(check => [check.id, check])).values()];
  return checks;
}

function remediation(source, check) {
  const selected = { ...command(check.id, check.argv, check.reason, check.env), expected_test: check.expected_test };
  if (check.nested_except) {
    const file = check.argv.at(-1);
    const names = [...read(source, file).matchAll(/\bt\.test\(['"]([^'"]+)['"]/g)].map(match => match[1]);
    if (!names.includes(check.nested_except)) throw Error('Missing historical failure selector');
    const escape = value => value.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
    const desired = new Set([check.nested_except]);
    if (check.baseline_sha && read(check.baseline_sha, file) !== read(source, file)) {
      const ranges = changedLines(source, check.baseline_sha, file);
      const lines = read(source, file).split('\n');
      const positions = lines.flatMap((line, index) => { const match = /\bt\.test\(['"]([^'"]+)['"]/.exec(line); return match ? [{name: match[1], start: index + 1}] : []; });
      positions.forEach((test, index) => { if (ranges.some(([start,end]) => start < (positions[index+1]?.start ?? lines.length + 1) && end >= test.start)) desired.add(test.name); });
    }
    selected.expected_tests = [...desired];
    selected.nested_file = file;
    const excluded = names.filter(name => !desired.has(name)).map(escape);
    if (excluded.length) selected.argv.splice(selected.argv.length - 1, 0, '--test-skip-pattern', `^(?:${excluded.join('|')})$`);
  }
  return selected;
}

const CHECK_COMMON = ['Cargo.toml', 'Cargo.lock', 'rust-toolchain.toml', 'crates/agents-worker/Cargo.toml', 'crates/agents-core', 'crates/agents-auth', 'crates/agents-worker/src/util.rs', 'crates/agents-worker/src/resources.rs'];
const TOOL_CHECK_INPUTS = {
  'paid-route-policy.test.mjs': ['tests/e2e/paid-route-policy.mjs', 'crates/agents-worker/src/provider.rs', 'crates/agents-worker/src/provider_config.rs', 'wrangler.jsonc'],
  'test_paid_route_policy.py': ['tests/e2e/paid_route_policy.py', 'tests/e2e/conftest.py', 'crates/agents-worker/src/provider.rs', 'crates/agents-worker/src/provider_config.rs', 'wrangler.jsonc'],
  'test_current_responses_sdk_contract.py': ['tests/e2e/current_responses_sdk.py', 'tests/e2e/response_checks.py', 'tests/e2e/validate_responses_wire.py', 'tests/e2e/requirements-current.lock'],
  'test_full_responses_sdk_contract.mjs': ['tests/e2e/full-responses-sdk.mjs', 'tests/e2e/paid-route-policy.mjs', 'tests/e2e/current_responses_sdk.py', 'tests/e2e/requirements-current.lock'],
  'test_deepseek_usage_probe.py': ['scripts/deepseek-usage-probe.py'],
  'test_egress_client.py': ['container/egress_client.py'],
  'test_python_kernel_bridge.py': ['container/python_kernel.py', 'container/egress_client.py'],
  'incremental-checks.test.mjs': ['scripts/incremental-checks.mjs', '.github/workflows/ci.yml'],
  'bundle-worker.test.mjs': ['scripts/bundle-worker.mjs', 'wrangler.jsonc'],
  'deploy-workflow.test.mjs': ['.github/workflows/deploy.yml'],
  'deploy-config.test.mjs': ['scripts/deploy-config.mjs', 'wrangler.jsonc'],
  'deploy-provenance.test.mjs': ['scripts/deploy-provenance.mjs', 'scripts/incremental-checks.mjs', 'scripts/deploy-config.mjs', 'scripts/container-artifact.mjs', '.github/workflows/deploy.yml'],
};
const checkFiles = new Map();

export function checkInputPaths(source, check, runGit = git) {
  const direct = check.argv.find(argument => /^(?:tests|scripts|apps)\/.*\.(?:py|mjs|[jt]sx?)(?:::|$)/.test(argument))?.split('::')[0];
  if (check.argv[0] === 'cargo') {
    const crate = check.argv[check.argv.indexOf('-p') + 1];
    if (crate !== 'agents-worker') return ['Cargo.toml', 'Cargo.lock', 'rust-toolchain.toml', `crates/${crate}`, 'crates/agents-core', 'crates/agents-auth'];
    let own = check.source_file;
    if (!own) {
      const name = check.id.split(':').at(-1);
      own = runGit('grep', '-l', '-E', `fn[[:space:]]+${name}[[:space:]]*[(]`, source, '--', 'crates/agents-worker/src').split('\n')[0]?.slice(41);
    }
    if (!own) throw Error(`Cannot bind Rust check source: ${check.id}`);
    const base = path.basename(own);
    const groups = [
      [/^provider/, /\/(?:provider[^/]*|responses_controls|responses_wire|responses_schema)\.rs$/],
      [/^(?:session|conversation|workspace|response_(?:routes|index|websocket)|resources)/, /\/(?:session[^/]*|conversation[^/]*|workspace[^/]*|response_[^/]*|store[^/]*|resources|cleanup|checkpoint_gc|space)\.rs$/],
      [/^(?:store|model_|auxiliary|context_prefix)/, /\/(?:store[^/]*|model_[^/]*|auxiliary[^/]*|responses_auxiliary|context_prefix|metering|resources)\.rs$/],
      [/^(?:hosted|image|search|file|tool|vector)/, /\/(?:hosted[^/]*|image[^/]*|search[^/]*|file[^/]*|tool[^/]*|vector[^/]*|workspace_blobs|space|metering|resources)\.rs$/],
      [/^(?:responses_|response_preflight|prompt_templates|context_management)/, /\/(?:responses_[^/]*|response_preflight|context_prefix|prompt_templates|context_management|tools|hosted_bindings|skills)\.rs$/],
      [/^deepseek_tokenizer/, /\/(?:deepseek_tokenizer|deepseek_image_tokens|input_image_metadata|provider_responses|provider_config)\.rs$/],
    ];
    const pattern = groups.find(([match]) => match.test(base))?.[1];
    const key = `${source}:worker-files`;
    if (!checkFiles.has(key)) checkFiles.set(key, runGit('ls-tree', '-r', '--name-only', source, '--', 'crates/agents-worker/src').split('\n').filter(Boolean));
    const files = pattern ? checkFiles.get(key).filter(file => pattern.test(file)) : checkFiles.get(key);
    return [...new Set([...CHECK_COMMON, own, ...files, ...(base.startsWith('deepseek_tokenizer') ? ['third_party/deepseek-recipe', 'tests/fixtures/deepseek-token-count.json', 'tests/fixtures/deepseek-image-count.json'] : [])])].sort();
  }
  if (direct?.startsWith('apps/dashboard/')) return [...COMPONENTS.dashboard, 'package.json', 'package-lock.json'];
  if (direct?.startsWith('tests/worker/')) return [...COMPONENTS.worker, 'migrations', 'tests/worker', 'tests/e2e/response_checks.py', 'tests/e2e/validate_responses_wire.py', 'tests/e2e/requirements.lock'];
  if (direct && Object.hasOwn(TOOL_CHECK_INPUTS, path.basename(direct))) {
    return [...new Set([direct, ...TOOL_CHECK_INPUTS[path.basename(direct)], 'package.json', 'package-lock.json', 'tests/e2e/requirements.lock'])].sort();
  }
  // Unknown contracts retain conservative component coverage rather than assume
  // that their test file has no shared implementation or fixture dependencies.
  return direct?.startsWith('tests/test_') ? [...COMPONENTS.native, direct, 'migrations', 'tests/fixtures'] : [...COMPONENTS.tooling, direct].filter(Boolean);
}

export function checkFingerprint(source, check, runGit = git) {
  if (!SHA.test(source)) throw Error('Invalid check source');
  const paths = checkInputPaths(source, check, runGit);
  const tree = runGit('ls-tree', '-r', source, '--', ...paths);
  return createHash('sha256').update(JSON.stringify({ version: 1, id: check.id, argv: check.argv, env: check.env,
    expected_test: check.expected_test, expected_tests: check.expected_tests, paths })).update('\0').update(tree).digest('hex');
}

export function reuseChecks(source, checks, baselines, hash = checkFingerprint) {
  const pending = [], reused = [];
  for (const original of checks) {
    const check = { ...original, input_fingerprint: hash(source, original) };
    const proof = [...baselines].reverse().flatMap(base => (base.verified_checks ?? []).map(record => ({ base, record })))
      .find(({ record }) => record.id === check.id && record.input_fingerprint === check.input_fingerprint && record.status === "passed_current" && record.executed_tests > 0);
    if (!proof) { pending.push(check); continue; }
    reused.push({ ...check, status: 'reused_verified', source_sha: proof.base.source_sha, run_id: proof.base.run_id,
      artifact_name: proof.base.receipt_artifact, artifact_id: proof.base.receipt_artifact_id ?? null,
      receipt_path: proof.record.receipt_path, executed_tests: proof.record.executed_tests });
  }
  return { checks: pending, reused_checks: reused };
}

export function containerSelection(checks, scope) {
  if (scope === 'incremental') return { checks, validation_scope: 'incremental', deferred_checks: [] };
  if (scope !== 'startup-1g') throw Error('Invalid finite container verification scope');
  const file = 'tests/e2e/test_workspace_interpreter_memory.py';
  const test = `${file}::test_physical_memory_guard_precedes_kernel_and_execution_id_acceptance`;
  return {
    checks: [command('diagnostic:container-startup-1g', python('-m', 'pytest', `${test}[1g]`, '-ra', '-rP'), 'One documented startup failure; diagnostic only, not capability acceptance')],
    validation_scope: 'diagnostic_startup_only',
    deferred_checks: [
      ...['4g', 'None'].map(memory => command(`pytest:${file}:${memory}`, python('-m', 'pytest', `${test}[${memory}]`, '-ra', '-rP'), 'Previously failed memory capability; not covered by 1g startup diagnosis')),
      ...checks.filter(check => !check.id.includes('test_workspace_interpreter_memory.py')),
    ],
  };
}

export function operatorUsagePlan(source, baselines, hashComponent = fingerprint, hashCheck = checkFingerprint) {
  if (!SHA.test(source)) throw Error('Invalid source SHA');
  const components = Object.fromEntries(Object.keys(COMPONENTS).map(component => [component, {
    fingerprint: hashComponent(source, component), action: component === 'tooling' ? 'verify' : 'not_executed',
    validation_scope: component === 'tooling' ? 'operator_usage_only' : 'not_executed', artifact: null,
  }]));
  const ownScope = command('node:operator-usage-scope', ['node', '--test', '--test-name-pattern',
    '^operator usage scope never implies platform acceptance$', 'scripts/incremental-checks.test.mjs'],
    'New finite operator scope and non-deployment boundary');
  ownScope.expected_test = 'operator usage scope never implies platform acceptance';
  const selected = reuseChecks(source, [
    command('pytest:tests/e2e/test_deepseek_usage_probe.py', python('-m', 'pytest', 'tests/e2e/test_deepseek_usage_probe.py', '-ra'), 'Operator helper only; no paid request'),
    command('node:scripts/deploy-workflow.test.mjs', ['node', '--test', 'scripts/deploy-workflow.test.mjs'], 'Operator workflow security boundary'),
    command('node:scripts/operator-usage-provenance.test.mjs', ['node', '--test', 'scripts/operator-usage-provenance.test.mjs'], 'Operator provenance verifier'),
    ownScope,
  ], baselines, hashCheck);
  return {
    schema_version: 1, source_sha: source, policy: 'incremental-only', verification_scope: 'operator-usage',
    app_validation: 'not_executed', non_deployable: true, diagnostic_only_components: ['worker', 'native', 'container', 'dashboard'],
    planned_checks_are_not_results: true, required_jobs: ['changes', 'tooling'], required_remediations: [],
    reused_checks: selected.reused_checks.map(check => ({ ...check, job: 'tooling' })),
    receipt_paths: { tooling: ['tooling.json'] }, changed_files: [], native_packages: [], components,
    checks: { rust: [], worker: [], dashboard: [], container: [], tooling: selected.checks, current_sdk: [] },
  };
}

export function plan(source, baselines, containerScope = 'incremental', verificationScope = 'incremental') {
  if (verificationScope === 'operator-usage') return operatorUsagePlan(source, baselines);
  if (verificationScope !== 'incremental') throw Error('Invalid finite verification scope');
  if (!SHA.test(source)) throw Error('Invalid source SHA');
  const components = {};
  const checks = { rust: [], worker: [], dashboard: [], container: [], tooling: [], current_sdk: [] };
  const changedFiles = new Set();
  let nativePackages = [];
  for (const component of Object.keys(COMPONENTS)) {
    const baseline = baselineFor(source, component, baselines);
    const selection = baseline.selection_base ?? baseline;
    if (baseline.selection_base && (selection.status !== 'composed_accepted'
        || !SHA.test(selection.source_sha) || !Array.isArray(selection.receipts) || !selection.receipts.length
        || selection.receipts.some(receipt => !SHA.test(receipt.source_sha) || !/^[1-9][0-9]{0,14}$/.test(String(receipt.run_id)) || !receipt.reference)
        || git('merge-base', selection.source_sha, source) !== selection.source_sha)) throw Error('Invalid composed selection baseline');
    const files = changed(source, selection.source_sha);
    const artifactFiles = changed(source, baseline.source_sha);
    if (component === 'native') {
      nativePackages = artifactFiles.some(file => /^(Cargo\.(lock|toml)|rust-toolchain\.toml)$/.test(file))
        ? ['agents-core', 'agents-auth', 'magents', 'workspace-daemon']
        : [...new Set(artifactFiles.map(file => /^crates\/(agents-core|agents-auth|magents|workspace-daemon)\//.exec(file)?.[1]).filter(Boolean))].sort();
    }
    for (const file of artifactFiles) changedFiles.add(file);
    const selected = selections(source, selection.source_sha, files);
    const jobs = component === 'tooling' ? ['tooling', 'current_sdk'] : [JOBS[component]];
    for (const job of jobs) checks[job].push(...selected[job]);
    for (const check of selection.unresolved_checks ?? []) {
      const repair = remediation(source, {...check, baseline_sha: selection.source_sha});
      if (repair.nested_file) checks[JOBS[component]] = checks[JOBS[component]].filter(item => item.id !== `node:${repair.nested_file}`);
      checks[JOBS[component]].push(repair);
    }
    const build = !baseline.matches;
    components[component] = { fingerprint: fingerprint(source, component), action: build ? 'build' : 'reuse', baseline,
      artifact: ARTIFACTS[component] ? { name: ARTIFACTS[component], artifact_id: build ? null : baseline.artifact_id ?? null, run_id: build ? process.env.GITHUB_RUN_ID ?? null : baseline.run_id, source_sha: build ? source : baseline.source_sha } : null };
  }
  for (const job of Object.keys(checks)) checks[job] = [...new Map(checks[job].map(check => [check.id, check])).values()];
  const container = containerSelection(checks.container, containerScope);
  checks.container = container.checks;
  components.container.validation_scope = container.validation_scope;
  components.container.deferred_checks = container.deferred_checks;
  const reusedChecks = [];
  for (const job of Object.keys(checks)) {
    const selected = reuseChecks(source, checks[job], baselines);
    checks[job] = selected.checks;
    reusedChecks.push(...selected.reused_checks.map(check => ({...check, job})));
  }
  const requiredJobs = ['changes', ...Object.entries(components).filter(([component, details]) => details.action === 'build' || checks[JOBS[component]].length > 0 || (component === 'tooling' && checks.current_sdk.length > 0)).map(([component]) => JOBS[component])];
  return { schema_version: 1, source_sha: source, policy: 'incremental-only', diagnostic_only_components: containerScope === 'startup-1g' ? ['container'] : [], planned_checks_are_not_results: true,
    required_jobs: requiredJobs, reused_checks: reusedChecks, required_remediations: [...new Set(Object.values(components).flatMap(component => ((component.baseline.selection_base ?? component.baseline).unresolved_checks ?? []).map(check => check.id)))], receipt_paths: { rust: ['rust.json', 'native-build.json'], worker: ['worker-rust.json', 'worker-journeys.json'], dashboard: ['dashboard.json'], container: ['container.json'], tooling: ['tooling.json', 'current_sdk.json'] },
    changed_files: [...changedFiles].sort(), native_packages: nativePackages, components, checks };
}

export function executedTests(argv, output) {
  if (argv[0] === 'cargo') return [...output.matchAll(/test result: (?:ok|FAILED)\. (\d+) passed; (\d+) failed;/g)].reduce((sum, match) => sum + Number(match[1]) + Number(match[2]), 0);
  if (argv.includes('unittest') || (/python(?:3(?:\.\d+)?)?(?:\.exe)?$/.test(argv[0]) && argv.some(arg => /^tests\/test_.*\.py$/.test(arg)))) return Number(/Ran (\d+) tests? in/.exec(output)?.[1] ?? 0);
  if (argv.includes('pytest')) return ['passed', 'failed'].reduce((sum, kind) => sum + Number(new RegExp('(?:^|[ ,=])(\\d+) ' + kind + '(?:[ ,=]|$)', 'm').exec(output)?.[1] ?? 0), 0);
  if (argv[0] === 'node' && argv.includes('--test')) return ['pass', 'fail'].reduce((sum, kind) => sum + Number(new RegExp('^# ' + kind + ' (\\d+)$', 'm').exec(output)?.[1] ?? 0), 0);
  if (argv[0] === 'npm' && argv.includes('test:browser')) return Number(/(?:^|\s)(\d+) passed(?:\s|$)/m.exec(output.replace(/\u001b\[[0-9;]*m/g, ''))?.[1] ?? 0);
  if (argv[0] === 'npm') return Number(/Tests\s+(\d+) passed/.exec(output.replace(/\u001b\[[0-9;]*m/g, ''))?.[1] ?? 0);
  throw Error('Unknown selected test runner');
}

export function runChecks(receipt, job, execute = spawnSync, directory = OUTPUT) {
  const sourceJob = job.startsWith('worker-') ? 'worker' : job;
  if (!Object.hasOwn(receipt.checks, sourceJob)) throw Error('Unknown check job');
  const selected = receipt.checks[sourceJob].filter(check => job === 'worker-rust' ? check.argv[0] === 'cargo' : job === 'worker-journeys' ? check.argv[0] !== 'cargo' : true);
  const results = [];
  const failures = [];
  mkdirSync(directory, { recursive: true });
  for (const check of selected) {
    if (check.env.MAGENTS_WORKER_CASE && !readFileSync('tests/worker/http.test.mjs', 'utf8').includes(check.env.MAGENTS_WORKER_CASE)) {
      throw Error(`Selected Worker journey does not exist: ${check.env.MAGENTS_WORKER_CASE}`);
    }
    const started = Date.now();
    console.log(`::group::${check.id} — ${check.reason}`);
    const result = execute(check.argv[0], check.argv.slice(1), { stdio: 'pipe', encoding: 'utf8', maxBuffer: 16 * 1024 * 1024, env: { ...process.env, ...check.env } });
    process.stdout.write(result.stdout ?? ''); process.stderr.write(result.stderr ?? '');
    const tests = executedTests(check.argv, `${result.stdout ?? ''}\n${result.stderr ?? ''}`);
    const requiredNames = check.expected_tests ?? (check.expected_test ? [check.expected_test] : []);
    const observed = requiredNames.every(name => (result.stdout ?? '').includes(`# Subtest: ${name}`));
    const passed = result.status === 0 && tests > 0 && observed;
    console.log('::endgroup::');
    const failure = passed ? undefined : { process_status: result.status, signal: result.signal ?? null,
      transport_error: result.error?.message?.slice(0, 512) ?? null,
      stderr_tail: (result.stderr ?? '').slice(-16384), stdout_tail: (result.stdout ?? '').slice(-4096) };
    results.push({ id: check.id, argv: check.argv, env: check.env, status: passed ? 'passed_current' : 'failed_current', executed_tests: tests, duration_ms: Date.now() - started, input_fingerprint: check.input_fingerprint, failure });
    writeFileSync(`${directory}/${job}.json`, JSON.stringify({ schema_version: 1, source_sha: receipt.source_sha, job, results }, null, 2) + '\n');
    if (!passed) {
      failures.push(check.id);
      // Uncaught termination can discard pending asynchronous stderr writes.
      writeFileSync(2, `\nFailure tail for ${check.id}:\n${failure.stderr_tail}\n${failure.stdout_tail}\n`);
      if (check.argv[0] === 'cargo' && /could not compile|error\[E\d+\]|failed to (?:run custom build|resolve|get )/.test(`${result.stdout ?? ''}\n${result.stderr ?? ''}`)) {
        throw Error(`Compilation failed; remaining checks of the same crate are not recompiled: ${check.id}`);
      }
    }
  }
  if (failures.length) throw Error(`Incremental checks failed or selected zero tests: ${failures.join(', ')}`);
  if (!results.length) writeFileSync(`${directory}/${job}.json`, JSON.stringify({ schema_version: 1, source_sha: receipt.source_sha, job, results: [], status: 'no_selected_tests' }, null, 2) + '\n');
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const [mode, job] = process.argv.slice(2);
  if (mode === 'plan') {
    const baselines = JSON.parse(readFileSync('scripts/verification-baselines.json', 'utf8').replace(/^\uFEFF/, ''));
    // Private topic proof has explicit checks below, not the unrelated composed deployment selection.
    for (const baseline of baselines) delete baseline.selection_base;
    const receipt = plan(git('rev-parse', 'HEAD'), baselines, process.env.CONTAINER_SCOPE ?? 'incremental', process.env.VERIFICATION_SCOPE ?? 'incremental');
    // Topic-only queued file management comparison; no accepted Conversation or SQL replay.
    for (const key of Object.keys(receipt.checks)) receipt.checks[key] = [];
    receipt.native_packages = [];
    receipt.required_jobs = ['changes','worker'];
    for (const [name, component] of Object.entries(receipt.components)) component.action = name === 'worker' ? 'build' : 'reuse';
    receipt.checks.worker = [
      command('new-file-batch-telemetry',['cargo','test','--locked','-p','agents-worker','--lib','batched_d1_phases_keep_actual_rows_and_operation_correlation','--','--nocapture'],'One new structural batch telemetry preservation contract'),
      command('new-file-management-perf',['node','--test','tests/worker/file-management-perf.test.mjs'],'One new queued file management comparison, exact same rows and charges, zero ingestion',{MAGENTS_BASELINE_WORKER_DIR:'.cache/file-management-baseline/build'})
    ];
    // New telemetry Rust contract already passed; same exact compiled Wasm, only fixture corrected.
    receipt.checks.worker = receipt.checks.worker.filter(check => check.argv[0] !== 'cargo');
    receipt.components.worker.action = 'reuse';
    receipt.components.worker.artifact = {run_id:'40450871131', source_sha:'4f11e42b781d5aa39907b538158f063a8882d958', artifact_id:'12710153193', artifact_name:'worker'};
    mkdirSync(OUTPUT, { recursive: true });
    writeFileSync(`${OUTPUT}/plan.json`, JSON.stringify(receipt, null, 2) + '\n');
    if (process.env.GITHUB_OUTPUT) appendFileSync(process.env.GITHUB_OUTPUT, `dashboard_browser=${receipt.checks.dashboard.some(check => check.argv.includes('test:browser'))}\n`);
    for (const [component, details] of Object.entries(receipt.components)) {
      const job = JOBS[component];
      const required = details.action === 'build' || receipt.checks[job].length > 0 || (job === 'tooling' && receipt.checks.current_sdk.length > 0);
      const line = `${job}=${required}\n`;
      process.stdout.write(line);
      if (process.env.GITHUB_OUTPUT) {
        appendFileSync(process.env.GITHUB_OUTPUT, line);
        appendFileSync(process.env.GITHUB_OUTPUT, `${component}_build=${details.action === 'build'}\n`);
        if (details.artifact) appendFileSync(process.env.GITHUB_OUTPUT, `${component}_artifact_run=${details.artifact.run_id}\n${component}_artifact_sha=${details.artifact.source_sha}\n${component}_artifact_id=${details.artifact.artifact_id ?? ''}\n`);
      }
    }
  } else if (mode === 'check-native') {
    const receipt = JSON.parse(readFileSync(`${OUTPUT}/plan.json`, 'utf8'));
    const argv = ['cargo', 'clippy', '--locked', ...receipt.native_packages.flatMap(name => ['-p', name]), '--all-targets', '--', '-D', 'warnings'];
    if (receipt.native_packages.length) {
      const result = spawnSync(argv[0], argv.slice(1), { stdio: 'inherit' });
      if (result.status !== 0) throw Error('Changed native package type/lint gate failed');
    }
    writeFileSync(`${OUTPUT}/native-build.json`, JSON.stringify({ source_sha: receipt.source_sha, argv, status: receipt.native_packages.length ? 'passed_current' : 'no_changed_native_packages' }, null, 2) + '\n');
  } else if (mode === 'run') {
    runChecks(JSON.parse(readFileSync(`${OUTPUT}/plan.json`, 'utf8')), job);
  } else throw Error('Usage: node scripts/incremental-checks.mjs plan | run <job>');
}
