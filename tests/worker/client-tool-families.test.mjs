import {createPublicImageInputFixture, publicImageOrigin, publicImagePng} from './public-image-input-journey.mjs';
import OpenAI from 'openai';
import {createBrowserPrincipal} from './browser-principal.mjs';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { createHash, randomBytes } from 'node:crypto';
import { mkdir, mkdtemp, readFile, readdir, writeFile } from 'node:fs/promises';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import { Miniflare, convertV4MiniflareOptions } from 'miniflare';
import { createIdentityFixture } from './auth-journey.mjs';
import { createNativeSemanticsProvider } from './native-semantics-journey.mjs';

const root = fileURLToPath(new URL('../../', import.meta.url));
const build = path.resolve(root, process.env.WORKER_BUILD_DIR ?? 'crates/agents-worker/build');
async function migrate(db) {
  for (const name of (await readdir(path.join(root, 'migrations'))).filter(name => name.endsWith('.sql')).sort()) {
    const sql = await readFile(path.join(root, 'migrations', name), 'utf8');
    const parsed = spawnSync(process.env.PYTHON ?? 'python', ['-c',
      'import sys,sqlite3,json\ns: str=""; out: list[str]=[]\nfor line in sys.stdin:\n s+=line\n if sqlite3.complete_statement(s): out.append(s); s=""\nassert not s.strip(), s\nprint(json.dumps(out))'],
    { input: sql, encoding: 'utf8' });
    assert.equal(parsed.status, 0, `${name}: ${parsed.stderr}`);
    await db.batch(JSON.parse(parsed.stdout).map(statement => db.prepare(statement)));
  }
}

test('compiled Rust Worker: caller-owned computer shell and patch lifecycle', { timeout: 75000 }, async t => {
  assert.ok(process.env.MAGENTS_CURRENT_SCHEMA_PYTHON,'Configure the current strict SDK interpreter for this new payload family');
  const encryptionKey = randomBytes(32);
  const identity = createIdentityFixture();
  const providers = createNativeSemanticsProvider();
  const publicImages = createPublicImageInputFixture();
  const deployment = JSON.parse(await readFile(path.join(root, 'wrangler.jsonc'), 'utf8'));
  await mkdir(path.join(root, '.temp'), { recursive: true });
  const directory = await mkdtemp(path.join(root, '.temp/native-semantics-'));
  const mf = new Miniflare(convertV4MiniflareOptions({
    modulesRoot: build,
    modules: [
      { type: 'ESModule', path: path.join(build, 'worker/shim.mjs') },
      { type: 'ESModule', path: path.join(build, 'index.js') },
      { type: 'CompiledWasm', path: path.join(build, 'index_bg.wasm') },
    ],
    compatibilityDate: '2026-10-23', compatibilityFlags: ['enable_request_signal'],
    resourcePersistencePath: path.join(directory, 'storage'),
    d1Databases: ['DB'], r2Buckets: ['WORKSPACES', 'CONVERSATIONS', 'DIAGNOSTICS'],
    durableObjects: {
      SESSIONS: { className: 'Session', useSQLite: true },
      AUTH_SESSIONS: { className: 'AuthSession', useSQLite: true },
      CONVERSATION_STATE: { className: 'ConversationState', useSQLite: true },
      MODEL_OPERATIONS: { className: 'ModelOperation', useSQLite: true },
    },
    ratelimits: Object.fromEntries(['INGRESS', 'ACCOUNT', 'MUTATION'].map((name, index) => [
      `${name}_RATE_LIMITER`, { namespace_id: String(index + 1), simple: { limit: 1000, period: 60 } },
    ])),
    bindings: {
      ENVIRONMENT: 'test', APP_ORIGIN: 'https://agents.test', ALLOWED_ORIGIN: 'https://agents.test',
      DATA_ENCRYPTION_KEY: encryptionKey.toString('base64url'),
      IDENTITY_ISSUER: 'https://identity.test', IDENTITY_WEB_CLIENT_ID: 'test-dashboard',
      IDENTITY_NATIVE_CLIENT_ID: 'test-native', IDENTITY_REDIRECT_URI: 'https://agents.test/auth/callback',
      IDENTITY_NATIVE_REDIRECT_URI: 'http://127.0.0.1/callback',
      IDENTITY_CLIENT_PRIVATE_JWK: JSON.stringify(identity.privateJwk),
      BILLING_PRODUCT_ID: 'agents', BILLING_MODE: 'test',
      MODEL_REGISTRY_JSON: '[]', MODEL_PRICING_JSON: '{}',
      RESOURCE_PRICING_JSON: deployment.env.staging.vars.RESOURCE_PRICING_JSON,
      RUN_RESOURCE_RESERVATION_MICROUSD: '20000', FREE_GRANT_BUDGET_MICROUSD_PER_DAY: '100000000',
    },
    async outboundService(request) {
      const image = await publicImages.handle(request);
      if(image) return image;
      const response = await providers.handle(request);
      assert.notEqual(response, undefined, `Native fixtures must not access external services: ${new URL(request.url).origin}`);
      return response;
    },
  }));
  t.after(() => mf.dispose());
  const db = await mf.getD1Database('DB');
  await migrate(db);
  async function request(method, endpoint, body, expected = 200, bearer) {
    const response = await mf.dispatchFetch('https://agents.test' + endpoint, {
      method, headers: { Authorization: `Bearer ${bearer}`, 'Content-Type': 'application/json', 'CF-Connecting-IP': '192.0.2.73' },
      ...(body === undefined ? {} : { body: JSON.stringify(body) }),
    });
    const text = await response.text();
    assert.equal(response.status, expected, `${method} ${endpoint}: ${text}`);
    return JSON.parse(text);
  }
  const context = { mf, db, encryptionKey, request, providers };
  const screenshot='data:image/png;base64,'+publicImagePng.toString('base64');
  const cases=[
    {name:'computer', tool:{type:'computer'}, call:{type:'computer_call',action:{type:'screenshot'},pending_safety_checks:[{id:'safety-1',code:'sensitive',message:'Confirm'}]}, result:{type:'computer_call_output',output:{type:'computer_screenshot',image_url:screenshot},acknowledged_safety_checks:[{id:'safety-1'}]}},
    {name:'local-shell', tool:{type:'local_shell'}, call:{type:'local_shell_call',action:{type:'exec',command:['echo','hello'],env:{}}},result:{type:'local_shell_call_output',output:'{"stdout":"hello","exit_code":0}'}},
    {name:'shell',tool:{type:'shell',environment:{type:'local'}},call:{type:'shell_call',environment:{type:'local'},action:{commands:['echo hello'],timeout_ms:1000,max_output_length:1024}},result:{type:'shell_call_output',output:[{stdout:'hello',stderr:'',outcome:{type:'exit',exit_code:0}}]}},
    {name:'patch',tool:{type:'apply_patch'},call:{type:'apply_patch_call',operation:{type:'update_file',path:'example.txt',diff:'@@\n-old\n+new'}},result:{type:'apply_patch_call_output',status:'completed',output:'Applied'}}
  ];
  const filter=process.env.MAGENTS_CLIENT_TOOL_CASE;
  const selected=cases.filter(spec=>!filter||spec.name===filter);
  assert.ok(selected.length, 'Unknown client tool selector');
  const receipts=[];
  for(const spec of cases) await t.test(spec.name+' returns exact typed results and forks without re-execution',{skip:!selected.includes(spec)},async()=>{
    const principal=await createBrowserPrincipal({mf,db,encryptionKey,owner:'client-'+spec.name});
    const callId='client-call-'+spec.name;
    const call={...spec.call,id:'provider-'+spec.name,call_id:callId,status:'completed',vendor_secret:'NEVER_PUBLIC'};
    const final={id:'provider-final',type:'message',role:'assistant',status:'completed',content:[{type:'output_text',text:'Caller result received.',annotations:[],logprobs:[]}]};
    const state=providers.register('client-'+spec.name,[{output:[call]},{output:[final],check(body){const results=body.input.filter(item=>item.type===spec.result.type);assert.equal(results.length,1);assert.equal(results[0][spec.name==='local-shell'?'id':'call_id'],callId);}},{output:[final]}]);
    const credential=await principal.browser('POST','/v1/provider-keys',{protocol:'openai',base_url:state.baseUrl,api_key:state.key},201);
    const agent=await request('POST','/v1/agents',{name:'Caller '+spec.name,instructions:'Protected caller prefix',model:{provider:'byok',credential_id:credential.id,protocol:'responses',model:state.model,context_window:32768,max_output_tokens:512},workspace:false,tools:[],compaction_percent:80},201,principal.apiKey);
    let rawEventsPromise;
    const client=new OpenAI({apiKey:principal.apiKey,baseURL:'https://agents.test/v1',maxRetries:0,fetch:async(input,init)=>{
      const response=await mf.dispatchFetch(input,init);
      if(response.headers.get('content-type')?.includes('text/event-stream')){
        rawEventsPromise=response.clone().text().then(text=>text.split(/\r?\n\r?\n/).flatMap(frame=>{const data=frame.split(/\r?\n/).filter(line=>line.startsWith('data:')).map(line=>line.slice(5).trimStart()).join('\n');return data&&data!=='[DONE]'?[JSON.parse(data)]:[];}));
      }
      return response;
    }});
    const events=[];
    for await(const event of await client.responses.create({model:agent.id,input:'Perform caller-owned task.',tools:[spec.tool],stream:true})) events.push(event);
    const initial=events.findLast(event=>event.type==='response.completed')?.response;
    assert.ok(initial,JSON.stringify(events));
    assert.equal(initial.output[0].type,call.type);
    assert.equal(initial.output[0].call_id,callId);
    assert.notEqual(initial.output[0].id,call.id);
    assert.ok(!JSON.stringify(events).includes('NEVER_PUBLIC'));
    const rawInitial=await request('GET','/v1/responses/'+initial.id,undefined,200,principal.apiKey);
    assert.ok(rawEventsPromise,'capture original SSE before SDK decoration');
    const schema=spawnSync(process.env.MAGENTS_CURRENT_SCHEMA_PYTHON,['tests/e2e/current_responses_sdk.py','response-wire','--input','-'],{cwd:root,input:JSON.stringify({response:rawInitial,events:await rawEventsPromise}),encoding:'utf8',maxBuffer:1024*1024});
    assert.equal(schema.status,0,schema.stdout);
    assert.equal(schema.stderr,'','strict schema failures must not leak raw payloads');
    const wrong={type:'function_call_output',call_id:callId,output:'wrong kind'};
    await request('POST','/v1/responses',{model:agent.id,previous_response_id:initial.id,input:[wrong],tools:[spec.tool]},400,principal.apiKey);
    assert.equal(state.calls,1,'wrong result must fail before inference');
    if(spec.name==='computer'){
      await request('POST','/v1/responses',{model:agent.id,previous_response_id:initial.id,input:[{...spec.result,call_id:callId,acknowledged_safety_checks:[]}],tools:[spec.tool]},400,principal.apiKey);
      assert.equal(state.calls,1,'missing safety acknowledgement must fail before inference');
      const foreign=await createBrowserPrincipal({mf,db,encryptionKey,owner:'client-foreign-screenshot'});
      const png=Buffer.from(screenshot.split(',')[1],'base64');
      const uploaded=await mf.dispatchFetch('https://agents.test/v1/files?filename=private.png&purpose=user_data',{method:'POST',headers:{Authorization:'Bearer '+foreign.apiKey,'Content-Type':'image/png','Content-Length':String(png.length),'X-Content-Sha256':createHash('sha256').update(png).digest('hex')},body:png});
      assert.equal(uploaded.status,200);
      const file=await uploaded.json();
      await request('POST','/v1/responses',{model:agent.id,previous_response_id:initial.id,input:[{...spec.result,call_id:callId,output:{type:'computer_screenshot',file_id:file.id}}],tools:[spec.tool]},404,principal.apiKey);
      assert.equal(state.calls,1,'another owner screenshot must fail before inference');
    }
    const result={...spec.result,[spec.name==='local-shell'?'id':'call_id']:callId};
    if(spec.name==='computer'){
      for(const source of ['/redirect-public','/redirect-private','/oversize','/chunked-oversize','/gif','/wrong-mime','/not-an-image','https://127.0.0.1/a.png','https://user:secret@public-image-fixture.example.com/a.png']){
        const image_url=source.startsWith('https:')?source:publicImageOrigin+source;
        await request('POST','/v1/responses',{model:agent.id,previous_response_id:initial.id,input:[{...result,output:{type:'computer_screenshot',image_url}}],tools:[spec.tool]},400,principal.apiKey);
        assert.equal(state.calls,1,'invalid public screenshots never infer');
      }
      result.output={type:'computer_screenshot',image_url:publicImageOrigin+'/image.png?signature=opaque'};
    }
    const key='client-result-'+spec.name;
    const continuation=await client.responses.create({model:agent.id,previous_response_id:initial.id,input:[result],tools:[spec.tool]},{headers:{'Idempotency-Key':key}});
    const replay=await client.responses.create({model:agent.id,previous_response_id:initial.id,input:[result],tools:[spec.tool]},{headers:{'Idempotency-Key':key}});
    assert.equal(replay.id,continuation.id);
    assert.equal(state.calls,2,'idempotent caller result must not rerun the tool or model');
    const before=await request('GET','/v1/responses/'+initial.id,undefined,200,principal.apiKey);
    const branch=await client.responses.create({model:agent.id,previous_response_id:initial.id,input:[result],tools:[spec.tool]},{headers:{'Idempotency-Key':'fork-'+key}});
    assert.notEqual(branch.id,continuation.id);
    assert.deepEqual(await request('GET','/v1/responses/'+initial.id,undefined,200,principal.apiKey),before);
    assert.equal(state.calls,3);
    if(spec.name==='computer'){
      assert.equal(publicImages.calls.filter(call=>call.path==='/redirect-target').length,0);
      assert.ok(publicImages.calls.some(call=>call.path==='/image.png'&&call.query==='?signature=opaque'));
      const carried=state.requests[1].input.find(item=>item.type==='computer_call_output');
      assert.ok(carried.output.image_url.startsWith('data:image/png;base64,'),'model sees frozen bytes, not mutable URL');
    }
    const ledger=await db.prepare("SELECT COUNT(*) AS count FROM usage_ledger l JOIN usage_reservations r ON r.id=l.reservation_id WHERE r.account_id=? AND l.resource='model'").bind(principal.owner).first();
    assert.equal(ledger.count,3,'each caller handoff/fork settles one known model action');
    receipts.push({family:spec.name,provider_actions:state.calls,root:initial.id,continuation:continuation.id,branch:branch.id});
  });
  assert.equal(receipts.length,selected.length,'do not publish evidence after any failed family');
  await mkdir(path.join(root,'.cache/v0.3-ci'),{recursive:true});
  await writeFile(path.join(root,'.cache/v0.3-ci/client-tool-families'+(filter?'-'+filter:'')+'.json'),JSON.stringify({source:process.env.GITHUB_SHA??null,fixture:'actual-compiled-rust-worker',wasm_sha256:createHash('sha256').update(await readFile(path.join(build,'index_bg.wasm'))).digest('hex'),external_network_calls:0,receipts},null,2));
});
