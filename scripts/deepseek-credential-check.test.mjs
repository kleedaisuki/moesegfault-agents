import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync} from 'node:fs';
import test from 'node:test';
import {verifyDeepseekCredential} from './deepseek-credential-check.mjs';
import {prepareDeploymentDependencies} from './prepare-deployment-dependencies.mjs';

const key='synthetic-private-api-key';
const catalog=JSON.stringify({object:'list',data:[{id:'canonical-model-not-an-agent-alias',object:'model'}]});
const hash=body=>createHash('sha256').update(body).digest('hex');
const headers={'Content-Type':'application/json','x-request-id':'request-metadata-original'};

test('production credential metadata accepts authenticated nonempty models without depending on an alias catalog', async()=>{
  const reports=[];let calls=0;
  const result=await verifyDeepseekCredential(key,{fetch:async(url,options)=>{
    calls++;assert.equal(url,'https://api.deepseek.com/models');assert.equal(options.method,'GET');
    assert.equal(options.redirect,'manual');assert.equal(options.headers.Authorization,`Bearer ${key}`);
    assert.equal(options.body,undefined);assert.ok(options.signal instanceof AbortSignal);
    return new Response(catalog,{status:200,headers});
  },report:value=>reports.push(value)});
  assert.equal(calls,1);assert.equal(result.http_status,200);assert.equal(result.request_id,headers['x-request-id']);
  assert.equal(result.response_bytes,Buffer.byteLength(catalog));assert.equal(result.response_sha256,hash(catalog));
  assert.equal(result.error_code,null);
  assert.deepEqual(Object.keys(result).sort(),['duration_ms','error_code','http_status','request_id','response_bytes','response_sha256']);
  const recorded=JSON.stringify(reports);
  assert.ok(!recorded.includes(key)&&!recorded.includes('canonical-model-not-an-agent-alias'));
});

test('production credential metadata rejects 401 redirects malformed UTF8 and empty lists without logging response contents', async()=>{
  const cases=[
    [401,JSON.stringify({error:{message:key}}),'deepseek_models_auth_rejected'],
    [302,'redirect-body','deepseek_models_http_rejected'],
    [200,'malformed '+key,'deepseek_models_invalid_json'],
    [200,new Uint8Array([255,254]),'deepseek_models_invalid_json'],
    [200,JSON.stringify({object:'list',data:[]}),'deepseek_models_invalid_list'],
    [200,JSON.stringify({object:'list',data:[{id:''}]}),'deepseek_models_invalid_list'],
  ];
  for(const [status,body,code]of cases){
    let calls=0;const reports=[];
    await assert.rejects(verifyDeepseekCredential(key,{fetch:async()=>{
      calls++;return new Response(body,{status,headers:{...headers,Location:'https://unrelated.test/collect'}});
    },report:value=>reports.push(value)}),error=>{
      assert.equal(error.code,code);assert.equal(error.details.http_status,status);
      assert.equal(error.details.request_id,headers['x-request-id']);assert.ok(error.details.response_sha256);
      assert.ok(!JSON.stringify(error.details).includes(key));return true;
    });
    assert.equal(calls,1);assert.ok(!JSON.stringify(reports).includes(key));
  }
});

test('production credential metadata bounds response memory and time while preserving known HTTP evidence', async()=>{
  let cancelled=false;
  await assert.rejects(verifyDeepseekCredential(key,{maxBytes:64,fetch:async()=>new Response(new ReadableStream({
    start(controller){controller.enqueue(new Uint8Array(65));},cancel(){cancelled=true;},
  }),{status:200,headers})}),error=>{
    assert.equal(error.code,'deepseek_models_body_too_large');assert.equal(error.details.http_status,200);
    assert.equal(error.details.response_bytes,65);assert.equal(error.details.response_sha256,null);return true;
  });
  assert.equal(cancelled,true);
  await assert.rejects(verifyDeepseekCredential(key,{timeoutMs:5,fetch:async(url,{signal})=>new Promise((resolve,reject)=>{
    const timer=setTimeout(()=>reject(signal.reason),20);
    signal.addEventListener('abort',()=>{clearTimeout(timer);reject(signal.reason);},{once:true});
  })}),error=>{assert.equal(error.code,'deepseek_models_timeout');assert.equal(error.details.http_status,null);return true;});
  await assert.rejects(verifyDeepseekCredential(key,{fetch:async()=>new Response(new ReadableStream({
    start(controller){controller.error(new Error(key));},
  }),{status:200,headers})}),error=>{
    assert.equal(error.code,'deepseek_models_transport_failed');assert.equal(error.details.http_status,200);
    assert.equal(error.details.request_id,headers['x-request-id']);assert.ok(!JSON.stringify(error.details).includes(key));return true;
  });
});

test('production credential gate runs before any Cloudflare read or mutation and invalid credentials never reach secret put', async()=>{
  const input=JSON.parse(readFileSync(new URL('../wrangler.jsonc',import.meta.url),'utf8'));
  input.env.production.vars.HOSTED_TOOL_RUNTIMES='web_search';
  const env={DEPLOY_ENVIRONMENT:'production',RUNTIME_FEATURES:'configured',CLOUDFLARE_ACCOUNT_ID:'a'.repeat(32),
    CLOUDFLARE_API_TOKEN:'synthetic-cf',DEEPSEEK_API_KEY:key,GITHUB_SHA:'b'.repeat(40)};
  const order=[],reports=[];
  const options={fetch:async()=>{order.push('cloudflare');return Response.json({success:true,result:{bindings:[]}});},
    putSecret:()=>order.push('secret'),report:value=>reports.push(structuredClone(value)),
    verifyCredential:(secret,options)=>{
      order.push('credential');return verifyDeepseekCredential(secret,{...options,fetch:async()=>Response.json({error:{message:key}},{status:401,headers})});
    }};
  await assert.rejects(prepareDeploymentDependencies(input,env,options),/deepseek_models_auth_rejected/);
  assert.deepEqual(order,['credential']);assert.equal(reports.at(-1).deepseek_credential.http_status,401);
  assert.ok(!JSON.stringify(reports).includes(key));
  order.length=0;
  await prepareDeploymentDependencies(input,env,{...options,verifyCredential:(secret,options)=>{
    order.push('credential');return verifyDeepseekCredential(secret,{...options,fetch:async()=>new Response(catalog,{status:200,headers})});
  }});
  assert.deepEqual(order,['credential','cloudflare','secret']);
});
