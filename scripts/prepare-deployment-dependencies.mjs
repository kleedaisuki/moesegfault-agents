import { createHash } from 'node:crypto';
import { spawnSync } from 'node:child_process';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import { configuredDeploymentRuntime, renderDeploymentConfig, requireDistinctEnvironmentDatabases } from './deploy-config.mjs';

const apiOrigin = 'https://api.cloudflare.com/client/v4';
const digest = text => createHash('sha256').update(text).digest('hex');

class DependencyError extends Error {
  constructor(code, details = {}) { super(code); this.code = code; this.details = details; }
}

function fail(code, details) { throw new DependencyError(code, details); }
function safeText(value, env) {
  let text = String(value ?? '');
  for (const key of ['CLOUDFLARE_API_TOKEN', 'DEEPSEEK_API_KEY', 'GH_TOKEN', 'GITHUB_TOKEN']) {
    if (env[key]) text = text.replaceAll(env[key], '[redacted]');
  }
  return text.slice(0, 8192);
}

export function requireProtectedDeployment(env) {
  if (env.GITHUB_ACTIONS !== 'true' || env.GITHUB_WORKFLOW !== 'Deploy' || env.GITHUB_JOB !== 'deploy'
    || env.DEPLOY_ENVIRONMENT !== 'production' || env.ACCEPTANCE_PROFILE !== 'none') {
    fail('dependency_preparation_requires_protected_deploy');
  }
}

export function putDeepseekSecret(environment, secret, env, run = spawnSync) {
  if (!['staging', 'production'].includes(environment) || typeof secret !== 'string' || !secret) fail('invalid_dependency_secret_input');
  const args = [resolve('node_modules/wrangler/bin/wrangler.js'), 'secret', 'put', 'DEEPSEEK_API_KEY',
    '--env', environment, '--config', 'wrangler.jsonc'];
  const childEnv = { ...env, WRANGLER_WRITE_LOGS: 'false', WRANGLER_SEND_METRICS: 'false', WRANGLER_LOG: 'error' };
  for (const key of ['DEEPSEEK_API_KEY', 'IDENTITY_CLIENT_PRIVATE_JWK', 'DATA_ENCRYPTION_KEY', 'OPENROUTER_API_KEY']) delete childEnv[key];
  const result = run(process.execPath, args, { input: `${secret}\n`, encoding: 'utf8', timeout: 60_000,
    windowsHide: true, env: childEnv });
  if (result.status !== 0) fail('dependency_deepseek_secret_preparation_failed', {
    phase: 'put_deepseek_secret', exit_code: result.status, process_error: result.error?.code ?? null,
    diagnostic: safeText(result.stderr || result.stdout, env),
  });
}

export async function prepareDeploymentDependencies(input, env, { fetch: request = globalThis.fetch,
  putSecret = putDeepseekSecret, report = () => {} } = {}) {
  const environment = env.DEPLOY_ENVIRONMENT;
  if (!['staging', 'production'].includes(environment)) fail('invalid_dependency_environment');
  const target = input.env?.[environment], runtime = configuredDeploymentRuntime(input, env);
  const needsDeepseek = runtime.webSearch || target?.vars?.DEFAULT_PROVIDER === 'deepseek';
  const receipt = { schema_version: 1, environment, worker_name: target?.name,
    runtime_features: runtime.workerFeatures, vectorize: 'not_selected', deepseek_secret: 'not_selected', observations: [] };
  report(receipt);
  if (!runtime.fileSearch && !needsDeepseek) return receipt;
  if (target?.name !== `moesegfault-agents-${environment}` || target.vars?.ENVIRONMENT !== environment
    || !/^[a-f0-9]{32}$/.test(env.CLOUDFLARE_ACCOUNT_ID ?? '') || !env.CLOUDFLARE_API_TOKEN) fail('invalid_dependency_target');
  if (needsDeepseek && !env.DEEPSEEK_API_KEY) fail('missing_selected_deepseek_dependency_secret');
  try {
    requireDistinctEnvironmentDatabases(input);
    renderDeploymentConfig(input, env);
  } catch (error) {
    fail('invalid_reviewed_dependency_configuration', { phase:'validate_configuration',
      error_type:error?.name ?? 'Error',diagnostic:safeText(error?.message,env) });
  }

  const account = `/accounts/${env.CLOUDFLARE_ACCOUNT_ID}`;
  async function cloudflare(path, phase, { method = 'GET', body, missing = false } = {}) {
    const started = Date.now();
    const observation = { phase, method, path, state: 'pending' };
    receipt.observations.push(observation); report(receipt);
    let response;
    try {
      response = await request(`${apiOrigin}${path}`, { method, redirect: 'manual', signal: AbortSignal.timeout(30_000),
        headers: { Authorization: `Bearer ${env.CLOUDFLARE_API_TOKEN}`, 'Content-Type': 'application/json' },
        ...(body ? { body: JSON.stringify(body) } : {}) });
    } catch (error) {
      Object.assign(observation, { state:'outcome_unknown',duration_ms:Date.now()-started,error_type:error?.name ?? 'Error' }); report(receipt);
      fail('dependency_cloudflare_transport_unknown', observation);
    }
    Object.assign(observation, { state:'headers_received',http_status:response.status,
      request_id:response.headers.get('cf-ray'),content_type:response.headers.get('content-type') }); report(receipt);
    let text;
    try { text = await response.text(); }
    catch (error) {
      Object.assign(observation, { state:'body_read_failed',http_status:response.status,duration_ms:Date.now()-started,error_type:error?.name ?? 'Error' }); report(receipt);
      fail('dependency_cloudflare_body_read_failed', observation);
    }
    Object.assign(observation, { http_status:response.status,duration_ms:Date.now()-started,
      response_bytes:Buffer.byteLength(text),response_sha256:digest(text),request_id:response.headers.get('cf-ray') });
    let value;
    try { value = JSON.parse(text); }
    catch { observation.state='invalid_json'; report(receipt); fail('dependency_cloudflare_response_invalid', observation); }
    observation.error_codes = (Array.isArray(value.errors) ? value.errors : []).map(error=>error.code).filter(Number.isSafeInteger).slice(0,8);
    observation.error_messages = (Array.isArray(value.errors) ? value.errors : []).map(error=>safeText(error.message,env).slice(0,512)).slice(0,8);
    if (missing && response.status === 404 && value.success === false && Array.isArray(value.errors)) { observation.state='not_found'; report(receipt); return null; }
    if (!response.ok || value.success !== true) { observation.state='rejected'; report(receipt); fail('dependency_cloudflare_request_failed', observation); }
    observation.state='observed'; report(receipt); return value.result;
  }

  // Secret put must address an existing serving script, never create an empty one.
  if (needsDeepseek) await cloudflare(`${account}/workers/scripts/${target.name}/settings`, 'verify_serving_worker');
  if (runtime.fileSearch) {
    const profile = JSON.parse(target.vars.FILE_SEARCH_PROFILE_JSON ?? 'null');
    const bindings = target.vectorize?.filter(binding=>binding.binding === 'VECTORIZE') ?? [];
    const name = `moesegfault-agents-file-search-${environment}-v1`;
    if (bindings.length !== 1 || bindings[0].index_name !== name || profile?.namespace !== `agents-${environment}`
      || profile.embedding?.dimensions !== 1024 || profile.embedding.model !== '@cf/baai/bge-m3') fail('invalid_reviewed_vectorize_dependency');
    const path = `${account}/vectorize/v2/indexes/${name}`;
    const matches = index => index?.name === name && index.config?.dimensions === 1024 && index.config.metric === 'cosine';
    let index = await cloudflare(path, 'get_vectorize_index', { missing:true });
    if (index === null) {
      receipt.vectorize='create_pending'; report(receipt);
      await cloudflare(`${account}/vectorize/v2/indexes`, 'create_vectorize_index', {
        method:'POST',body:{ name,config:{ dimensions:1024,metric:'cosine' } },
      });
      index = await cloudflare(path, 'readback_created_vectorize_index');
      receipt.vectorize='created';
    } else receipt.vectorize='existing';
    if (!matches(index)) fail('dependency_vectorize_configuration_mismatch', {
      phase:'verify_vectorize_index',name,actual_dimensions:index?.config?.dimensions ?? null,actual_metric:index?.config?.metric ?? null,
    });
    report(receipt);
  }
  if (needsDeepseek) {
    receipt.deepseek_secret='put_pending'; report(receipt);
    await putSecret(environment, env.DEEPSEEK_API_KEY, env);
    receipt.deepseek_secret='prepared'; report(receipt);
  }
  return receipt;
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  const path = '.temp/deploy-verification/dependency-preparation.json';
  let current;
  const report = receipt => { current=receipt; mkdirSync('.temp/deploy-verification',{recursive:true}); writeFileSync(path,`${JSON.stringify(receipt,null,2)}\n`); };
  try {
    requireProtectedDeployment(process.env);
    const receipt = await prepareDeploymentDependencies(JSON.parse(readFileSync('wrangler.jsonc','utf8')),process.env,{report});
    console.log(JSON.stringify({event:'deployment.dependencies_prepared',...receipt}));
  } catch (error) {
    const failure = { event:'deployment.dependencies_failed',error_code:error.code ?? 'dependency_preparation_failed',
      diagnostics:error.details ?? {error_type:error.name,diagnostic:safeText(error.message,process.env)} };
    report({...current,status:'failed',failure});
    console.error(JSON.stringify(failure));
    process.exitCode=1;
  }
}
