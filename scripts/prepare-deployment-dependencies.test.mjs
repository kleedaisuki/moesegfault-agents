import assert from 'node:assert/strict';
import { mkdirSync, mkdtempSync, readFileSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
import { resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { prepareDeploymentDependencies as prepareWithCredential, putDeepseekSecret, requireProtectedDeployment } from './prepare-deployment-dependencies.mjs';

// These previously accepted CF contracts inject the new independent credential gate.
const prepareDeploymentDependencies=(input,env,options)=>prepareWithCredential(input,env,{verifyCredential:async()=>({http_status:200,error_code:null}),...options});

const configuration = () => JSON.parse(readFileSync(new URL('../wrangler.jsonc', import.meta.url),'utf8'));
const environment = () => ({ DEPLOY_ENVIRONMENT:'production',RUNTIME_FEATURES:'configured',CLOUDFLARE_ACCOUNT_ID:'a'.repeat(32),
  CLOUDFLARE_API_TOKEN:'cf-api-synthetic-secret',DEEPSEEK_API_KEY:'ds-approved-synthetic-secret',GITHUB_SHA:'b'.repeat(40),
  GITHUB_ACTIONS:'true',GITHUB_WORKFLOW:'Deploy',GITHUB_JOB:'deploy',ACCEPTANCE_PROFILE:'none' });
const name = 'moesegfault-agents-file-search-production-v1';
const index = () => ({ name,config:{dimensions:1024,metric:'cosine'} });
const reply = (result,status=200) => Response.json({ success:status<300,result,errors:status>=300?[{code:10000,message:'Controlled metadata rejection'}]:[] },{status,headers:{'cf-ray':'request-control'}});

test('dependency preparation reuses exact resources and sends only selected DeepSeek through secret stdin', async () => {
  const calls=[], secrets=[], reports=[];
  const receipt = await prepareDeploymentDependencies(configuration(),environment(),{
    fetch:async(url,options)=>{calls.push({url,options});return reply(url.endsWith('/settings')?{bindings:[]}:index());},
    putSecret:(...args)=>secrets.push(args),report:value=>reports.push(structuredClone(value)),
  });
  assert.equal(receipt.vectorize,'existing');assert.equal(receipt.deepseek_secret,'prepared');
  assert.equal(calls.length,2);assert.ok(calls.every(call=>call.options.method==='GET'&&call.options.redirect==='manual'));
  assert.ok(calls[0].url.endsWith('/workers/scripts/moesegfault-agents-production/settings'));
  assert.ok(calls[1].url.endsWith(`/vectorize/v2/indexes/${name}`));
  assert.equal(secrets.length,1);assert.deepEqual(secrets[0].slice(0,2),['production','ds-approved-synthetic-secret']);
  assert.equal(reports[0].deepseek_secret,'not_selected');assert.equal(reports.at(-2).deepseek_secret,'put_pending');
  assert.ok(!JSON.stringify(reports).includes('synthetic-secret'));
  let invocation;
  putDeepseekSecret('production','ds-approved-synthetic-secret',environment(),(...args)=>{invocation=args;return{status:0};});
  assert.equal(invocation[2].input,'ds-approved-synthetic-secret\n');
  assert.ok(invocation[1].includes('DEEPSEEK_API_KEY'));
  assert.ok(!invocation[1].some(arg=>arg.includes('synthetic-secret')||arg.includes('IDENTITY')||arg.includes('ENCRYPTION')||arg.includes('OPENROUTER')));
  assert.equal(invocation[2].env.WRANGLER_WRITE_LOGS,'false');
  assert.equal(invocation[2].env.DEEPSEEK_API_KEY,undefined);
  assert.equal(invocation[2].env.CLOUDFLARE_API_TOKEN,environment().CLOUDFLARE_API_TOKEN);
  assert.throws(()=>putDeepseekSecret('production',undefined,environment(),()=>{throw Error('Must not run');}),/invalid_dependency_secret_input/);
});

test('dependency preparation creates only an authenticated missing index and verifies readback before secret write', async () => {
  const calls=[];let exists=false;
  const options={fetch:async(url,request)=>{
    calls.push(request.method);
    if(url.endsWith('/settings'))return reply({bindings:[]});
    if(request.method==='POST'){assert.deepEqual(JSON.parse(request.body),{name,config:{dimensions:1024,metric:'cosine'}});exists=true;return reply(index(),201);}
    return exists?reply(index()):reply(null,404);
  },putSecret:()=>calls.push('SECRET')};
  const first=await prepareDeploymentDependencies(configuration(),environment(),options);
  assert.equal(first.vectorize,'created');assert.deepEqual(calls,['GET','GET','POST','GET','SECRET']);
  calls.length=0;
  const second=await prepareDeploymentDependencies(configuration(),environment(),options);
  assert.equal(second.vectorize,'existing');assert.deepEqual(calls,['GET','GET','SECRET']);
  let lost=true, writes=0;
  const observeLostCreate={fetch:async(url,request)=>{
    if(url.endsWith('/settings'))return reply({bindings:[]});
    if(request.method==='POST'){writes++;lost=false;throw Error('Lost create acknowledgement');}
    return lost?reply(null,404):reply(index());
  },putSecret:()=>{}};
  await assert.rejects(prepareDeploymentDependencies(configuration(),environment(),observeLostCreate),error=>{
    assert.equal(error.code,'dependency_cloudflare_transport_unknown');assert.equal(error.details.phase,'create_vectorize_index');return true;
  });
  await prepareDeploymentDependencies(configuration(),environment(),observeLostCreate);
  assert.equal(writes,1,'Only original authenticated 404 permits create; retry observes an existing index');
});

test('dependency preparation fails closed on denied, ambiguous or mismatched metadata without replacing resources', async () => {
  for(const state of ['denied','unavailable','malformed','dimensions','metric','missing_worker']) {
    const calls=[];let secretWrites=0;
    await assert.rejects(prepareDeploymentDependencies(configuration(),environment(),{
      fetch:async(url,request)=>{
        calls.push(request.method);
        if(url.endsWith('/settings'))return reply({bindings:[]},state==='missing_worker'?404:200);
        if(state==='denied')return Response.json({success:false,errors:[{code:10000,message:'cf-api-synthetic-secret ds-approved-synthetic-secret'}]},{status:403});
        if(state==='unavailable')return reply(null,500);
        if(state==='malformed')return new Response('untrusted proxy body',{status:404});
        return reply({...index(),config:{dimensions:state==='dimensions'?768:1024,metric:state==='metric'?'dot-product':'cosine'}});
      },putSecret:()=>secretWrites++,
    }),error=>{
      assert.ok(error.code.startsWith('dependency_'));
      if(state==='denied'){assert.equal(error.details.http_status,403);assert.deepEqual(error.details.error_codes,[10000]);assert.ok(!JSON.stringify(error.details).includes('synthetic-secret'));}
      return true;
    });
    assert.ok(calls.every(method=>method==='GET'));assert.equal(secretWrites,0);
  }
  let calls=0;
  await assert.rejects(prepareDeploymentDependencies(configuration(),{...environment(),DEEPSEEK_API_KEY:undefined},{fetch:()=>{calls++;}}),/missing_selected_deepseek/);
  assert.equal(calls,0);
});

test('dependency preparation keeps disabled legacy configurations read-only and requires the existing protected deploy context', async () => {
  const input=configuration();delete input.env.production.vars.HOSTED_TOOL_RUNTIMES;
  let calls=0;
  const result=await prepareDeploymentDependencies(input,{DEPLOY_ENVIRONMENT:'production'}, {fetch:()=>calls++,putSecret:()=>calls++});
  assert.equal(calls,0);assert.equal(result.vectorize,'not_selected');assert.equal(result.deepseek_secret,'not_selected');
  requireProtectedDeployment(environment());
  for(const override of [{GITHUB_ACTIONS:'false'},{GITHUB_WORKFLOW:'CI'},{GITHUB_JOB:'operator_usage'},{DEPLOY_ENVIRONMENT:'staging'},{ACCEPTANCE_PROFILE:'responses-increment'}])assert.throws(()=>requireProtectedDeployment({...environment(),...override}),/protected_deploy/);
  assert.throws(()=>putDeepseekSecret('production','ds-approved-synthetic-secret',environment(),()=>({status:1,stderr:'cf-api-synthetic-secret ds-approved-synthetic-secret'})),error=>{
    assert.equal(error.code,'dependency_deepseek_secret_preparation_failed');assert.ok(!JSON.stringify(error.details).includes('synthetic-secret'));return true;
  });
  mkdirSync('.temp',{recursive:true});
  const directory=mkdtempSync(resolve('.temp','dependency-cli-'));
  const childResult=spawnSync(process.execPath,[fileURLToPath(new URL('./prepare-deployment-dependencies.mjs',import.meta.url))],
    {cwd:directory,encoding:'utf8',windowsHide:true,env:{...process.env,GITHUB_ACTIONS:'false'}});
  assert.equal(childResult.status,1);
  const failure=JSON.parse(readFileSync(resolve(directory,'.temp/deploy-verification/dependency-preparation.json'),'utf8'));
  assert.equal(failure.status,'failed');assert.equal(failure.failure.error_code,'dependency_preparation_requires_protected_deploy');
});

test('dependency preparation is inside the protected proven deployment and precedes every full Worker or migration change', () => {
  const workflow=readFileSync(new URL('../.github/workflows/deploy.yml',import.meta.url),'utf8');
  const preparation=workflow.indexOf('run: node scripts/prepare-deployment-dependencies.mjs');
  assert.ok(preparation>workflow.indexOf('run: node scripts/deploy-provenance.mjs'));
  assert.ok(preparation>workflow.indexOf('Never replay paid stage work through an Actions rerun'));
  for(const change of ['npx wrangler containers push','npx wrangler d1 migrations apply','npx wrangler deploy --env'])assert.ok(preparation<workflow.indexOf(change));
  const block=workflow.slice(workflow.lastIndexOf('      - name:',preparation),preparation);
  assert.ok(block.includes('secrets.DEEPSEEK_API_KEY'));
  assert.ok(block.includes("if: inputs.environment == 'production'"));
  assert.ok(!block.includes('IDENTITY_CLIENT_PRIVATE_JWK')&&!block.includes('DATA_ENCRYPTION_KEY')&&!block.includes('OPENROUTER_API_KEY'));
  assert.ok(workflow.includes('.temp/deploy-verification/dependency-preparation.json'));
  assert.ok(workflow.indexOf('run: node scripts/worker-secrets.mjs')>workflow.indexOf('npx wrangler deploy --env'));
});
